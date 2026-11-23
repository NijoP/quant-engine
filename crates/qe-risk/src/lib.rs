//! qe-risk: position sizing, leverage selection, Risk Governor, capital stages,
//! deterministic 120 s position lifecycle, paper execution, controls and audit.
//!
//! No exchange I/O lives here and no credentials are ever read. Time is always
//! passed in explicitly as `now_ms` (epoch milliseconds), so every test is
//! deterministic.
//!
//! # API for the engine loop
//! - [`RiskGovernor::evaluate_proposal`]: the only path to an approved order.
//!   Returns [`Decision`] with numeric reasons. The target controller has no
//!   handle that can loosen [`RiskPolicy`] ceilings ([`RiskPolicy::clamped`]).
//! - [`Position::new`] + `start` / `on_fill` / `on_protection_*` / `on_market_tick` /
//!   `on_timer` / `on_exit_fill` / `on_reconcile`: the lifecycle state machine
//!   returns [`Action`]s for the execution layer (paper or a future live adapter).
//! - [`PaperExchange::market_fill`]: fills against supplied bid/ask with fee and slippage.
//! - Controls: [`RiskGovernor::kill_switch`], [`RiskGovernor::pause_entries`],
//!   [`flatten_actions`], [`cancel_entry_actions`], each returning a [`ControlResult`].
//! - [`CapitalManager`]: stage 1 (growth) -> checkpoint -> stage 2 (protective).
//!   Withdrawals are recorded only when the user confirms them.

use serde::{Deserialize, Serialize};

pub mod gamble;

/// Maximum holding time, measured from the first confirmed fill.
pub const T_MAX_MS: u64 = 120_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Long,
    Short,
}

impl Side {
    fn sign(self) -> f64 {
        match self {
            Side::Long => 1.0,
            Side::Short => -1.0,
        }
    }
}

/// One dynamic leverage tier: `leverage` is allowed while position notional <= `max_notional`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LeverageTier {
    pub leverage: f64,
    pub max_notional: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarginMode {
    Isolated,
    Cross,
}

/// Instrument capability model. The lead maps live CoinDCX metadata into this;
/// nothing here is hardcoded per instrument.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstrumentCapability {
    pub pair: String,
    pub margin_currency: String,
    pub active: bool,
    pub exit_only: bool,
    pub max_leverage_long: f64,
    pub max_leverage_short: f64,
    pub tiers: Vec<LeverageTier>,
    /// Minimum notional in quote currency.
    pub min_notional: f64,
    pub min_quantity: f64,
    pub max_quantity: f64,
    pub quantity_increment: f64,
    pub price_increment: f64,
    pub taker_fee: f64,
    pub maker_fee: f64,
    /// Maintenance margin rate, if the exchange data provides it.
    pub maintenance_margin_rate: Option<f64>,
    pub margin_modes: Vec<MarginMode>,
    pub supports_market_order: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapitalMode {
    Growth,
    Protective,
}

/// Governor profile. `Standard` is the default and the only profile the desk
/// uses unless the operator explicitly opts in to the paper experiment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Profile {
    #[default]
    Standard,
    /// PAPER EXPERIMENT - UNGATED, MAX LEVERAGE. Paper only. No calibration
    /// gate, no session-loss / high-water-mark stop; all equity committed as
    /// margin at the highest exchange-permitted leverage, one position at a
    /// time. Raw EV > 0 after costs, model SL/TP, the 120 s time stop, fees and
    /// liquidation modelling are kept.
    ExperimentMaxLeverage,
    /// GAMBLE MODE (owner-armed, docs/GAMBLE_MODE.md): no calibration or
    /// net-EV gate; owner stake/leverage/floor; floor is a permanent stop.
    Gamble,
}

/// Hard ceilings. The governor may only tighten them, never loosen them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskPolicy {
    pub mode: CapitalMode,
    pub risk_fraction: f64,
    pub max_session_loss_fraction: f64,
    /// Max drawdown from the equity high-water mark before new entries stop.
    pub max_drawdown_from_hwm: f64,
    /// Risk fraction multiplier per consecutive loss (<= 1, so no escalation).
    pub loss_step_down: f64,
    pub min_expected_net_edge: f64,
    pub max_open_positions: usize,
    /// Liquidation must be at least this multiple of the stop distance away.
    pub liquidation_buffer: f64,
    pub policy_max_leverage: f64,
    pub exit_lead_ms: u64,
    pub entry_ack_timeout_ms: u64,
    pub exit_ack_timeout_ms: u64,
    #[serde(default)]
    pub profile: Profile,
    /// Owner gamble parameters; Some only for [`Profile::Gamble`].
    #[serde(default)]
    pub gamble: Option<gamble::GambleParams>,
}

impl RiskPolicy {
    pub fn growth() -> Self {
        Self {
            mode: CapitalMode::Growth,
            risk_fraction: 0.01,
            max_session_loss_fraction: 0.05,
            max_drawdown_from_hwm: 0.05,
            loss_step_down: 0.5,
            min_expected_net_edge: 0.0,
            max_open_positions: 1,
            liquidation_buffer: 1.5,
            policy_max_leverage: f64::INFINITY,
            exit_lead_ms: 3_000,
            entry_ack_timeout_ms: 2_000,
            exit_ack_timeout_ms: 2_000,
            profile: Profile::Standard,
            gamble: None,
        }
    }
    pub fn protective() -> Self {
        Self {
            mode: CapitalMode::Protective,
            risk_fraction: 0.005,
            max_session_loss_fraction: 0.02,
            max_drawdown_from_hwm: 0.02,
            loss_step_down: 0.5,
            min_expected_net_edge: 0.0,
            max_open_positions: 1,
            liquidation_buffer: 2.0,
            policy_max_leverage: f64::INFINITY,
            exit_lead_ms: 3_000,
            entry_ack_timeout_ms: 2_000,
            exit_ack_timeout_ms: 2_000,
            profile: Profile::Standard,
            gamble: None,
        }
    }
    /// Paper experiment policy: growth ceilings with [`Profile::ExperimentMaxLeverage`].
    /// The desk only builds this when the operator opts in and execution is paper.
    pub fn experiment_max_leverage() -> Self {
        Self {
            profile: Profile::ExperimentMaxLeverage,
            ..Self::growth()
        }
    }
    pub fn is_experiment(&self) -> bool {
        self.profile == Profile::ExperimentMaxLeverage
    }
    /// Gamble policy: growth ceilings for the standard fields (unused by the
    /// gamble path) plus the owner's parameters.
    pub fn gamble(params: gamble::GambleParams) -> Self {
        Self {
            profile: Profile::Gamble,
            gamble: Some(params),
            ..Self::growth()
        }
    }
    pub fn is_gamble(&self) -> bool {
        self.profile == Profile::Gamble && self.gamble.is_some()
    }
    /// Clamp a user/config-supplied policy to the mode's hard ceilings.
    pub fn clamped(mut self) -> Self {
        let ceil = match self.mode {
            CapitalMode::Growth => Self::growth(),
            CapitalMode::Protective => Self::protective(),
        };
        self.risk_fraction = self.risk_fraction.clamp(0.0, ceil.risk_fraction);
        self.max_session_loss_fraction = self
            .max_session_loss_fraction
            .clamp(0.0, ceil.max_session_loss_fraction);
        self.max_drawdown_from_hwm = self
            .max_drawdown_from_hwm
            .clamp(0.0, ceil.max_drawdown_from_hwm);
        self.loss_step_down = self.loss_step_down.clamp(0.0, 1.0);
        self.exit_lead_ms = self.exit_lead_ms.min(T_MAX_MS);
        self
    }
}

/// Structured trade proposal from the strategy layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeProposal {
    pub trace_id: String,
    pub pair: String,
    pub side: Side,
    pub entry_price: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    /// Expected net edge after costs (from the forecast engine).
    pub expected_net_edge: f64,
    pub p_tp_first: f64,
    pub calibrated: bool,
    pub data_healthy: bool,
    /// Fees + spread + slippage per unit, quote currency.
    pub cost_per_unit: f64,
    /// Quote currency -> account currency (1.0 for INR, USDT/INR rate otherwise).
    pub fx_to_account: f64,
}

/// Account view. `paper` marks a configured paper value, never a real balance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub equity: f64,
    pub available_margin: f64,
    pub paper: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovedOrder {
    pub trace_id: String,
    pub pair: String,
    pub side: Side,
    pub qty: f64,
    pub leverage: f64,
    pub entry_price: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    pub notional_account: f64,
    pub margin_account: f64,
    pub risk_budget: f64,
    pub risk_at_stop: f64,
    pub liquidation_price: Option<f64>,
    pub risk_fraction_used: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Decision {
    Approved(ApprovedOrder),
    Rejected {
        trace_id: String,
        reasons: Vec<String>,
    },
}

/// Highest leverage the exchange permits for this side and notional, capped by policy.
/// Returns 0 if no tier admits the notional.
pub fn max_permitted_leverage(
    inst: &InstrumentCapability,
    side: Side,
    notional: f64,
    policy_cap: f64,
) -> f64 {
    let side_max = match side {
        Side::Long => inst.max_leverage_long,
        Side::Short => inst.max_leverage_short,
    };
    let cap = side_max.min(policy_cap).max(1.0);
    if inst.tiers.is_empty() {
        return cap;
    }
    inst.tiers
        .iter()
        .filter(|t| t.max_notional >= notional)
        .map(|t| t.leverage.min(cap))
        .fold(0.0, f64::max)
}

/// Isolated-margin liquidation estimate: entry * (1 -/+ (1/L - mmr)). None if mmr unknown.
pub fn liquidation_price(entry: f64, side: Side, leverage: f64, mmr: Option<f64>) -> Option<f64> {
    let mmr = mmr?;
    let d = (1.0 / leverage - mmr).max(0.0);
    Some(entry * (1.0 - side.sign() * d))
}

fn floor_to(x: f64, inc: f64) -> f64 {
    let inc = if inc > 0.0 { inc } else { 1e-9 };
    (x / inc + 1e-9).floor() * inc
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditRecord {
    pub ts_ms: u64,
    pub kind: String,
    pub trace_id: String,
    pub detail: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ControlResult {
    pub ok: bool,
    pub message: String,
    pub actions: Vec<Action>,
}

/// Risk Governor: final authority over every entry.
#[derive(Debug, Clone, Serialize)]
pub struct RiskGovernor {
    pub policy: RiskPolicy,
    pub stage_start_equity: f64,
    pub session_realised_pnl: f64,
    pub high_water_mark: f64,
    pub consecutive_losses: u32,
    pub open_positions: usize,
    pub entries_paused: bool,
    pub killed: bool,
    pub final_target: f64,
    pub audit: Vec<AuditRecord>,
    /// Gamble mode: set when the floor is reached; permanent until the owner re-arms.
    pub gamble_stopped: Option<String>,
    /// Gamble mode: notional (account ccy) of the last approved bet.
    pub last_bet_notional: Option<f64>,
}

impl RiskGovernor {
    pub fn new(policy: RiskPolicy, stage_start_equity: f64, final_target: f64) -> Self {
        Self {
            policy: policy.clamped(),
            stage_start_equity,
            session_realised_pnl: 0.0,
            high_water_mark: stage_start_equity,
            consecutive_losses: 0,
            open_positions: 0,
            entries_paused: false,
            killed: false,
            final_target,
            audit: Vec::new(),
            gamble_stopped: None,
            last_bet_notional: None,
        }
    }

    /// Effective risk fraction: ceiling * step_down^consecutive_losses (never above ceiling).
    pub fn effective_risk_fraction(&self) -> f64 {
        self.policy.risk_fraction
            * self
                .policy
                .loss_step_down
                .powi(self.consecutive_losses as i32)
    }

    /// Call when a position closes (or an approved order is abandoned with pnl 0).
    pub fn record_realised(&mut self, pnl_net: f64, equity_after: f64, now_ms: u64) {
        self.session_realised_pnl += pnl_net;
        if pnl_net < 0.0 {
            self.consecutive_losses += 1;
        } else if pnl_net > 0.0 {
            self.consecutive_losses = 0;
        }
        self.high_water_mark = self.high_water_mark.max(equity_after);
        self.open_positions = self.open_positions.saturating_sub(1);
        self.log(
            now_ms,
            "realised",
            "",
            serde_json::json!({"pnl_net": pnl_net, "equity": equity_after}),
        );
    }

    fn log(&mut self, ts_ms: u64, kind: &str, trace_id: &str, detail: serde_json::Value) {
        self.audit.push(AuditRecord {
            ts_ms,
            kind: kind.into(),
            trace_id: trace_id.into(),
            detail,
        });
    }

    pub fn kill_switch(&mut self, now_ms: u64) -> ControlResult {
        self.killed = true;
        self.entries_paused = true;
        self.log(now_ms, "kill_switch", "", serde_json::json!({}));
        ControlResult {
            ok: true,
            message: "kill switch engaged: no new entries; flatten open positions".into(),
            actions: vec![],
        }
    }

    pub fn pause_entries(&mut self, paused: bool, now_ms: u64) -> ControlResult {
        if !paused && self.gamble_stopped.is_some() {
            return ControlResult {
                ok: false,
                message: format!(
                    "gamble mode stopped at the floor ({}); only the owner can re-arm",
                    self.gamble_stopped.clone().unwrap_or_default()
                ),
                actions: vec![],
            };
        }
        if !paused && self.killed {
            return ControlResult {
                ok: false,
                message: "kill switch engaged; cannot resume entries".into(),
                actions: vec![],
            };
        }
        self.entries_paused = paused;
        self.log(
            now_ms,
            "pause_entries",
            "",
            serde_json::json!({"paused": paused}),
        );
        ControlResult {
            ok: true,
            message: format!("entries paused = {paused}"),
            actions: vec![],
        }
    }

    pub fn evaluate_proposal(
        &mut self,
        p: &TradeProposal,
        inst: &InstrumentCapability,
        acct: &AccountSnapshot,
        now_ms: u64,
    ) -> Decision {
        let mut r: Vec<String> = Vec::new();
        let pol = self.policy.clone();
        if self.killed {
            r.push("kill switch engaged".into());
        }
        if self.entries_paused {
            r.push("new entries paused".into());
        }
        if acct.equity >= self.final_target {
            r.push(format!(
                "final target {} reached; no new entries",
                self.final_target
            ));
        }
        let experiment = pol.is_experiment();
        let gamble = pol.is_gamble();
        if let Some(why) = &self.gamble_stopped {
            r.push(format!("gamble mode stopped at the floor: {why}"));
        }
        let session_loss = -self.session_realised_pnl.min(0.0);
        let loss_limit = pol.max_session_loss_fraction * self.stage_start_equity;
        if !experiment && !gamble && session_loss >= loss_limit {
            r.push(format!(
                "session loss {session_loss:.4} >= limit {loss_limit:.4}"
            ));
        }
        let hwm_floor = self.high_water_mark * (1.0 - pol.max_drawdown_from_hwm);
        if !experiment && !gamble && acct.equity < hwm_floor {
            r.push(format!(
                "equity {:.4} below high-water-mark floor {hwm_floor:.4}",
                acct.equity
            ));
        }
        if self.open_positions >= pol.max_open_positions {
            r.push("max open positions reached".into());
        }
        if !inst.active {
            r.push("instrument inactive".into());
        }
        if inst.exit_only {
            r.push("instrument is exit-only".into());
        }
        if !(inst.max_leverage_long >= 1.0 && inst.max_leverage_short >= 1.0) {
            r.push("exchange max leverage unknown (null in metadata): ineligible".into());
        }
        if !inst.supports_market_order {
            r.push("market order not supported".into());
        }
        if inst.pair != p.pair {
            r.push("proposal/instrument pair mismatch".into());
        }
        if !p.data_healthy {
            r.push("market data integrity not established".into());
        }
        if !experiment && !gamble && !p.calibrated {
            r.push("forecast uncalibrated: denied strategy authority".into());
        }
        if !gamble
            && (p.expected_net_edge.is_nan() || p.expected_net_edge <= pol.min_expected_net_edge)
        {
            r.push(format!(
                "expected net edge {:.6} <= minimum {:.6}",
                p.expected_net_edge, pol.min_expected_net_edge
            ));
        }
        if p.fx_to_account.is_nan() || p.fx_to_account <= 0.0 {
            r.push("missing quote->account fx rate".into());
        }
        let s = p.side.sign();
        let stop_dist = (p.entry_price - p.stop_loss) * s;
        let tp_dist = (p.take_profit - p.entry_price) * s;
        if stop_dist.is_nan()
            || stop_dist <= 0.0
            || tp_dist.is_nan()
            || tp_dist <= 0.0
            || p.entry_price <= 0.0
        {
            r.push("stop/target on wrong side of entry".into());
        }
        if !r.is_empty() {
            return self.reject(p, r, now_ms);
        }
        if experiment {
            return self.size_experiment(p, inst, acct, stop_dist, now_ms);
        }
        if gamble {
            return self.size_gamble(p, inst, acct, stop_dist, now_ms);
        }

        // Sizing from the stop-loss risk budget, independent of leverage.
        let rf = self.effective_risk_fraction();
        let budget = acct.equity * rf;
        let loss_per_unit = (stop_dist + p.cost_per_unit.max(0.0)) * p.fx_to_account;
        let n_max = (budget / loss_per_unit).min(inst.max_quantity);
        let qty = floor_to(n_max, inst.quantity_increment);
        if qty <= 0.0 || qty < inst.min_quantity {
            return self.reject(
                p,
                vec![format!(
                    "min quantity {} exceeds risk budget (max {n_max:.8} units for budget {budget:.4})",
                    inst.min_quantity
                )],
                now_ms,
            );
        }
        let notional_quote = qty * p.entry_price;
        if notional_quote < inst.min_notional {
            return self.reject(
                p,
                vec![format!(
                    "min notional {} exceeds risk-budget notional {notional_quote:.4}; budget not raised",
                    inst.min_notional
                )],
                now_ms,
            );
        }
        let notional_account = notional_quote * p.fx_to_account;

        // Highest exchange-permitted leverage, stepped down only so liquidation stays
        // beyond the stop with buffer.
        let mut lev = max_permitted_leverage(inst, p.side, notional_quote, pol.policy_max_leverage);
        if lev < 1.0 {
            return self.reject(
                p,
                vec!["no leverage tier admits this notional".into()],
                now_ms,
            );
        }
        let mut liq = liquidation_price(p.entry_price, p.side, lev, inst.maintenance_margin_rate);
        while let Some(lp) = liq {
            let liq_dist = (p.entry_price - lp) * s;
            if liq_dist >= stop_dist * pol.liquidation_buffer || lev <= 1.0 {
                break;
            }
            lev = (lev - 1.0).floor().max(1.0);
            liq = liquidation_price(p.entry_price, p.side, lev, inst.maintenance_margin_rate);
        }
        if let Some(lp) = liq {
            if (p.entry_price - lp) * s < stop_dist * pol.liquidation_buffer {
                return self.reject(
                    p,
                    vec![format!("liquidation {lp:.6} too close to stop")],
                    now_ms,
                );
            }
        }
        let margin = notional_account / lev;
        if margin > acct.available_margin {
            return self.reject(
                p,
                vec![format!(
                    "margin {margin:.4} exceeds available {:.4}",
                    acct.available_margin
                )],
                now_ms,
            );
        }
        let order = ApprovedOrder {
            trace_id: p.trace_id.clone(),
            pair: p.pair.clone(),
            side: p.side,
            qty,
            leverage: lev,
            entry_price: p.entry_price,
            stop_loss: p.stop_loss,
            take_profit: p.take_profit,
            notional_account,
            margin_account: margin,
            risk_budget: budget,
            risk_at_stop: qty * loss_per_unit,
            liquidation_price: liq,
            risk_fraction_used: rf,
        };
        self.open_positions += 1;
        let detail = serde_json::to_value(&order).unwrap_or_default();
        self.log(now_ms, "approved", &p.trace_id, detail);
        Decision::Approved(order)
    }

    /// Experiment sizing: all available paper equity as margin at the highest
    /// exchange-permitted leverage for the side and resulting notional tier.
    /// Notional = margin * leverage, rounded down to the quantity increment;
    /// below the exchange minimum it is rejected, never raised. Leverage is not
    /// stepped down for the stop: a liquidation price is always estimated (with
    /// mmr 0, i.e. at 1/L, when the exchange gives no rate) and enforced by the
    /// lifecycle monitor.
    fn size_experiment(
        &mut self,
        p: &TradeProposal,
        inst: &InstrumentCapability,
        acct: &AccountSnapshot,
        stop_dist: f64,
        now_ms: u64,
    ) -> Decision {
        let cap = self.policy.policy_max_leverage;
        let margin_budget = acct.available_margin.min(acct.equity).max(0.0);
        let fx = p.fx_to_account;
        let min_notional_account = inst.min_notional * fx;
        if margin_budget <= 0.0
            || margin_budget
                < min_notional_account
                    / inst.max_leverage_long.max(inst.max_leverage_short).max(1.0)
        {
            return self.reject(
                p,
                vec![format!(
                    "equity {margin_budget:.4} below minimum tradable notional {min_notional_account:.4} for this instrument at max leverage"
                )],
                now_ms,
            );
        }
        // Highest leverage at the smallest notional, then step to the tier that
        // admits margin * leverage (monotone, bounded by the number of tiers).
        let mut lev = max_permitted_leverage(inst, p.side, 0.0, cap);
        let mut notional_quote = margin_budget * lev / fx;
        for _ in 0..=inst.tiers.len() + 1 {
            let l2 = max_permitted_leverage(inst, p.side, notional_quote, cap);
            if l2 >= lev {
                break;
            }
            if l2 < 1.0 {
                // Even the largest tier cannot hold margin * leverage: fill that tier.
                if let Some(t) = inst
                    .tiers
                    .iter()
                    .copied()
                    .max_by(|a, b| a.max_notional.total_cmp(&b.max_notional))
                {
                    lev = t.leverage.min(lev).max(1.0);
                    notional_quote = t.max_notional.min(margin_budget * lev / fx);
                }
                break;
            }
            lev = l2;
            notional_quote = margin_budget * lev / fx;
        }
        if lev < 1.0 {
            return self.reject(
                p,
                vec!["no leverage tier admits this notional".into()],
                now_ms,
            );
        }
        let n_max = (notional_quote / p.entry_price).min(inst.max_quantity);
        let qty = floor_to(n_max, inst.quantity_increment);
        if qty <= 0.0 || qty < inst.min_quantity {
            return self.reject(
                p,
                vec![format!(
                    "min quantity {} exceeds max-leverage quantity {n_max:.8} (margin {margin_budget:.4} x {lev}x)",
                    inst.min_quantity
                )],
                now_ms,
            );
        }
        let notional_quote = qty * p.entry_price;
        if notional_quote < inst.min_notional {
            return self.reject(
                p,
                vec![format!(
                    "min notional {} exceeds max-leverage notional {notional_quote:.4}; not raised",
                    inst.min_notional
                )],
                now_ms,
            );
        }
        let notional_account = notional_quote * fx;
        let margin = notional_account / lev;
        let liq = liquidation_price(
            p.entry_price,
            p.side,
            lev,
            Some(inst.maintenance_margin_rate.unwrap_or(0.0)),
        );
        let loss_per_unit = (stop_dist + p.cost_per_unit.max(0.0)) * fx;
        let order = ApprovedOrder {
            trace_id: p.trace_id.clone(),
            pair: p.pair.clone(),
            side: p.side,
            qty,
            leverage: lev,
            entry_price: p.entry_price,
            stop_loss: p.stop_loss,
            take_profit: p.take_profit,
            notional_account,
            margin_account: margin,
            risk_budget: margin_budget,
            risk_at_stop: (qty * loss_per_unit).min(margin),
            liquidation_price: liq,
            risk_fraction_used: if acct.equity > 0.0 {
                margin / acct.equity
            } else {
                0.0
            },
        };
        self.open_positions += 1;
        let mut detail = serde_json::to_value(&order).unwrap_or_default();
        detail["profile"] = serde_json::json!("experiment_max_leverage");
        self.log(now_ms, "approved", &p.trace_id, detail);
        Decision::Approved(order)
    }

    /// Gamble sizing ([`gamble::bet_notional`]): owner stake (or bold play) at
    /// the owner leverage capped by the exchange tier, margin plus fees kept
    /// above the floor, never larger after a loss, exchange min/max respected,
    /// and the stop must sit inside the estimated liquidation price.
    fn size_gamble(
        &mut self,
        p: &TradeProposal,
        inst: &InstrumentCapability,
        acct: &AccountSnapshot,
        stop_dist: f64,
        now_ms: u64,
    ) -> Decision {
        let Some(params) = self.policy.gamble.clone() else {
            return self.reject(p, vec!["gamble parameters missing".into()], now_ms);
        };
        let fx = p.fx_to_account;
        let s = p.side.sign();
        let cost_frac = (p.cost_per_unit / p.entry_price).max(0.0);
        let win_frac = ((p.take_profit - p.entry_price) * s - p.cost_per_unit) / p.entry_price;
        let owner = params.leverage as f64;
        let max_q = inst.max_quantity * p.entry_price * fx;
        let tier_max = inst
            .tiers
            .iter()
            .map(|t| t.max_notional)
            .fold(0.0, f64::max);
        let max_notional = if inst.tiers.is_empty() {
            max_q
        } else {
            max_q.min(tier_max * fx)
        };
        let after_loss_cap = if self.consecutive_losses > 0 {
            self.last_bet_notional
        } else {
            None
        };
        let mut lev = max_permitted_leverage(inst, p.side, 0.0, owner);
        let mut n_acct = 0.0;
        for _ in 0..=inst.tiers.len() + 1 {
            let ctx = gamble::SizeCtx {
                equity: acct.equity,
                available_margin: acct.available_margin,
                leverage: lev,
                cost_frac,
                win_frac,
                target: self.final_target,
                after_loss_cap,
                min_notional: inst.min_notional * fx,
                max_notional,
            };
            match gamble::bet_notional(&params, &ctx) {
                Ok(n) => n_acct = n,
                Err(e) => return self.reject(p, vec![format!("gamble: {e}")], now_ms),
            }
            let l2 = max_permitted_leverage(inst, p.side, n_acct / fx, owner);
            if l2 >= lev || l2 < 1.0 {
                break;
            }
            lev = l2;
        }
        if lev < 1.0 {
            return self.reject(
                p,
                vec!["no leverage tier admits this notional".into()],
                now_ms,
            );
        }
        let n_max = (n_acct / fx / p.entry_price).min(inst.max_quantity);
        let qty = floor_to(n_max, inst.quantity_increment);
        if qty <= 0.0 || qty < inst.min_quantity {
            return self.reject(
                p,
                vec![format!(
                    "gamble: min quantity {} exceeds bet quantity {n_max:.8}",
                    inst.min_quantity
                )],
                now_ms,
            );
        }
        let notional_quote = qty * p.entry_price;
        if notional_quote < inst.min_notional {
            return self.reject(
                p,
                vec![format!(
                    "gamble: min notional {} exceeds bet notional {notional_quote:.4}; not raised",
                    inst.min_notional
                )],
                now_ms,
            );
        }
        let liq = liquidation_price(
            p.entry_price,
            p.side,
            lev,
            Some(inst.maintenance_margin_rate.unwrap_or(0.0)),
        );
        if let Some(lp) = liq {
            if (p.entry_price - lp) * s <= stop_dist {
                return self.reject(
                    p,
                    vec![format!(
                        "gamble: stop {:.6} lies beyond the estimated liquidation {lp:.6} at {lev}x; lower the leverage",
                        p.stop_loss
                    )],
                    now_ms,
                );
            }
        }
        let notional_account = notional_quote * fx;
        let margin = notional_account / lev;
        let loss_per_unit = (stop_dist + p.cost_per_unit.max(0.0)) * fx;
        let order = ApprovedOrder {
            trace_id: p.trace_id.clone(),
            pair: p.pair.clone(),
            side: p.side,
            qty,
            leverage: lev,
            entry_price: p.entry_price,
            stop_loss: p.stop_loss,
            take_profit: p.take_profit,
            notional_account,
            margin_account: margin,
            risk_budget: acct.equity - params.floor(),
            risk_at_stop: (qty * loss_per_unit).min(margin),
            liquidation_price: liq,
            risk_fraction_used: if acct.equity > 0.0 {
                margin / acct.equity
            } else {
                0.0
            },
        };
        self.open_positions += 1;
        self.last_bet_notional = Some(notional_account);
        let mut detail = serde_json::to_value(&order).unwrap_or_default();
        detail["profile"] = serde_json::json!("gamble");
        detail["stake_policy"] = serde_json::to_value(params.stake_policy).unwrap_or_default();
        self.log(now_ms, "approved", &p.trace_id, detail);
        Decision::Approved(order)
    }

    /// Gamble floor stop: once equity minus the floor cannot carry the
    /// smallest bet (or equity is at/below the floor), gamble mode stops for
    /// good: entries paused and resuming refused until the owner re-arms.
    /// Returns the reason when it fires (once).
    pub fn gamble_floor_check(
        &mut self,
        equity: f64,
        min_margin: f64,
        now_ms: u64,
    ) -> Option<String> {
        let floor = self.policy.gamble.as_ref()?.floor();
        if self.gamble_stopped.is_some() {
            return None;
        }
        if equity <= floor || equity - floor < min_margin {
            let why = format!(
                "equity INR {equity:.4}, floor INR {floor:.4}, smallest bet margin INR {min_margin:.4}"
            );
            self.gamble_stopped = Some(why.clone());
            self.entries_paused = true;
            self.log(
                now_ms,
                "gamble_floor_stop",
                "",
                serde_json::json!({"reason": why}),
            );
            return Some(why);
        }
        None
    }

    fn reject(&mut self, p: &TradeProposal, reasons: Vec<String>, now_ms: u64) -> Decision {
        self.log(
            now_ms,
            "rejected",
            &p.trace_id,
            serde_json::json!({"reasons": reasons}),
        );
        Decision::Rejected {
            trace_id: p.trace_id.clone(),
            reasons,
        }
    }
}

// ---------------------------------------------------------------- lifecycle

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    StopLoss,
    TakeProfit,
    EdgeNegative,
    DataIntegrity,
    TimeExpiry,
    Emergency,
    ProtectionFailed,
    /// Paper experiment: the adverse move reached the estimated liquidation price.
    Liquidated,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Action {
    SetLeverage {
        pair: String,
        leverage: f64,
    },
    SubmitEntry {
        client_id: String,
        side: Side,
        qty: f64,
    },
    CancelEntryRemainder {
        client_id: String,
    },
    AttachProtection {
        stop_loss: f64,
        take_profit: f64,
        qty: f64,
    },
    SubmitExit {
        client_id: String,
        reason: ExitReason,
        qty: f64,
    },
    /// Query exchange order/position state; never blindly resend.
    Reconcile {
        client_id: String,
    },
    Incident {
        kind: String,
        detail: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum State {
    Validated,
    EntrySubmitted,
    Filling,
    ProtectionPending,
    Protected,
    Exiting,
    Reconciling,
    Closed,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub order: ApprovedOrder,
    pub state: State,
    pub filled_qty: f64,
    pub avg_entry: f64,
    pub entry_fees: f64,
    pub opened_at_ms: Option<u64>,
    pub submitted_at_ms: Option<u64>,
    pub exit_reason: Option<ExitReason>,
    pub exit_submitted_at_ms: Option<u64>,
    pub exit_seq: u32,
    pub exited_qty: f64,
    pub exit_value: f64,
    pub exit_fees: f64,
    pub realised_pnl: Option<f64>,
    pub exit_lead_ms: u64,
    pub ack_timeout_ms: u64,
}

impl Position {
    pub fn new(order: ApprovedOrder, policy: &RiskPolicy) -> Self {
        Self {
            order,
            state: State::Validated,
            filled_qty: 0.0,
            avg_entry: 0.0,
            entry_fees: 0.0,
            opened_at_ms: None,
            submitted_at_ms: None,
            exit_reason: None,
            exit_submitted_at_ms: None,
            exit_seq: 0,
            exited_qty: 0.0,
            exit_value: 0.0,
            exit_fees: 0.0,
            realised_pnl: None,
            exit_lead_ms: policy.exit_lead_ms,
            ack_timeout_ms: policy.entry_ack_timeout_ms.max(policy.exit_ack_timeout_ms),
        }
    }

    /// Deterministic client order id for the entry.
    pub fn entry_id(&self) -> String {
        format!("{}-entry", self.order.trace_id)
    }
    fn exit_id(&self) -> String {
        format!("{}-exit-{}", self.order.trace_id, self.exit_seq)
    }

    /// Steps 3-4: set leverage, submit entry.
    pub fn start(&mut self, now_ms: u64) -> Vec<Action> {
        self.state = State::EntrySubmitted;
        self.submitted_at_ms = Some(now_ms);
        vec![
            Action::SetLeverage {
                pair: self.order.pair.clone(),
                leverage: self.order.leverage,
            },
            Action::SubmitEntry {
                client_id: self.entry_id(),
                side: self.order.side,
                qty: self.order.qty,
            },
        ]
    }

    pub fn on_entry_rejected(&mut self) -> Vec<Action> {
        if self.filled_qty == 0.0 {
            self.state = State::Rejected;
        }
        vec![]
    }

    /// Partial or full entry fill. Timer starts at the first confirmed fill.
    pub fn on_fill(&mut self, qty: f64, price: f64, fee: f64, now_ms: u64) -> Vec<Action> {
        let new_qty = self.filled_qty + qty;
        self.avg_entry = (self.avg_entry * self.filled_qty + price * qty) / new_qty;
        self.filled_qty = new_qty;
        self.entry_fees += fee;
        self.opened_at_ms.get_or_insert(now_ms);
        if self.filled_qty + 1e-12 >= self.order.qty {
            self.state = State::ProtectionPending;
            vec![Action::AttachProtection {
                stop_loss: self.order.stop_loss,
                take_profit: self.order.take_profit,
                qty: self.filled_qty,
            }]
        } else {
            self.state = State::Filling;
            vec![]
        }
    }

    /// Entry order finished with a partial fill: cancel remainder, protect what we have.
    pub fn on_entry_done_partial(&mut self) -> Vec<Action> {
        if self.state != State::Filling {
            return vec![];
        }
        self.state = State::ProtectionPending;
        vec![
            Action::CancelEntryRemainder {
                client_id: self.entry_id(),
            },
            Action::AttachProtection {
                stop_loss: self.order.stop_loss,
                take_profit: self.order.take_profit,
                qty: self.filled_qty,
            },
        ]
    }

    pub fn on_protection_confirmed(&mut self) {
        if self.state == State::ProtectionPending {
            self.state = State::Protected;
        }
    }

    /// Never continue unprotected: emergency exit plus incident.
    pub fn on_protection_failed(&mut self, now_ms: u64, why: &str) -> Vec<Action> {
        let mut a = vec![Action::Incident {
            kind: "protection_failed".into(),
            detail: why.into(),
        }];
        a.extend(self.begin_exit(ExitReason::ProtectionFailed, now_ms));
        a
    }

    fn is_open(&self) -> bool {
        matches!(
            self.state,
            State::Filling | State::ProtectionPending | State::Protected
        )
    }

    fn begin_exit(&mut self, reason: ExitReason, now_ms: u64) -> Vec<Action> {
        if !self.is_open() {
            return vec![];
        }
        let mut a = Vec::new();
        if self.state == State::Filling {
            a.push(Action::CancelEntryRemainder {
                client_id: self.entry_id(),
            });
        }
        self.state = State::Exiting;
        self.exit_reason = Some(reason);
        self.exit_submitted_at_ms = Some(now_ms);
        self.exit_seq += 1;
        a.push(Action::SubmitExit {
            client_id: self.exit_id(),
            reason,
            qty: self.filled_qty - self.exited_qty,
        });
        a
    }

    pub fn emergency_exit(&mut self, now_ms: u64) -> Vec<Action> {
        self.begin_exit(ExitReason::Emergency, now_ms)
    }

    /// Steps 10-11: market monitoring. `edge` is the latest expected net edge.
    pub fn on_market_tick(
        &mut self,
        bid: f64,
        ask: f64,
        edge: f64,
        data_ok: bool,
        now_ms: u64,
    ) -> Vec<Action> {
        if !self.is_open() {
            return self.on_timer(now_ms);
        }
        let o = &self.order;
        let reason = match o.side {
            Side::Long if bid <= o.stop_loss => Some(ExitReason::StopLoss),
            Side::Long if bid >= o.take_profit => Some(ExitReason::TakeProfit),
            Side::Short if ask >= o.stop_loss => Some(ExitReason::StopLoss),
            Side::Short if ask <= o.take_profit => Some(ExitReason::TakeProfit),
            _ if !data_ok => Some(ExitReason::DataIntegrity),
            _ if edge < 0.0 => Some(ExitReason::EdgeNegative),
            _ => None,
        };
        match reason {
            Some(r) => self.begin_exit(r, now_ms),
            None => self.on_timer(now_ms),
        }
    }

    /// Independent timer: deadline exit and ack-timeout reconciliation.
    pub fn on_timer(&mut self, now_ms: u64) -> Vec<Action> {
        match self.state {
            State::EntrySubmitted => {
                let since = now_ms.saturating_sub(self.submitted_at_ms.unwrap_or(now_ms));
                if since >= self.ack_timeout_ms {
                    self.state = State::Reconciling;
                    return vec![Action::Reconcile {
                        client_id: self.entry_id(),
                    }];
                }
                vec![]
            }
            State::Exiting => {
                let since = now_ms.saturating_sub(self.exit_submitted_at_ms.unwrap_or(now_ms));
                if since >= self.ack_timeout_ms {
                    self.state = State::Reconciling;
                    return vec![Action::Reconcile {
                        client_id: self.exit_id(),
                    }];
                }
                vec![]
            }
            _ if self.is_open() => {
                let age = now_ms.saturating_sub(self.opened_at_ms.unwrap_or(now_ms));
                if age + self.exit_lead_ms >= T_MAX_MS {
                    let mut a = self.begin_exit(ExitReason::TimeExpiry, now_ms);
                    if age >= T_MAX_MS {
                        a.insert(
                            0,
                            Action::Incident {
                                kind: "deadline_breach".into(),
                                detail: format!("age {age} ms"),
                            },
                        );
                    }
                    return a;
                }
                vec![]
            }
            _ => vec![],
        }
    }

    /// Result of querying the exchange after a timeout. `exchange_position_qty` is what
    /// the exchange actually reports; only remaining exposure produces a new exit.
    pub fn on_reconcile(&mut self, exchange_position_qty: f64, now_ms: u64) -> Vec<Action> {
        if self.state != State::Reconciling {
            return vec![];
        }
        if let Some(r) = self.exit_reason {
            if exchange_position_qty <= 1e-12 {
                self.exited_qty = self.filled_qty;
                self.state = State::Closed;
                return vec![Action::Incident {
                    kind: "exit_reconciled_closed".into(),
                    detail: "exit confirmed by reconcile; fill price must be fetched".into(),
                }];
            }
            self.state = State::Protected;
            return self.begin_exit(r, now_ms);
        }
        if exchange_position_qty > 1e-12 {
            self.state = State::Filling;
            self.filled_qty = exchange_position_qty;
            self.opened_at_ms.get_or_insert(now_ms);
            return self.on_entry_done_partial();
        }
        self.state = State::Rejected;
        vec![]
    }

    /// Exit fill. Closes and returns realised net P&L (account currency) when flat.
    pub fn on_exit_fill(
        &mut self,
        qty: f64,
        price: f64,
        fee: f64,
        fx_to_account: f64,
    ) -> Option<f64> {
        self.exited_qty += qty;
        self.exit_value += qty * price;
        self.exit_fees += fee;
        if self.exited_qty + 1e-12 >= self.filled_qty {
            self.state = State::Closed;
            let gross =
                (self.exit_value - self.avg_entry * self.exited_qty) * self.order.side.sign();
            let pnl = (gross - self.entry_fees - self.exit_fees) * fx_to_account;
            self.realised_pnl = Some(pnl);
            return Some(pnl);
        }
        None
    }

    /// True when an open position's touch has reached its estimated liquidation
    /// price (long: bid <= liq, short: ask >= liq).
    pub fn liquidation_hit(&self, bid: f64, ask: f64) -> bool {
        if !self.is_open() || self.filled_qty <= 0.0 {
            return false;
        }
        match (self.order.liquidation_price, self.order.side) {
            (Some(l), Side::Long) => bid <= l,
            (Some(l), Side::Short) => ask >= l,
            _ => false,
        }
    }

    /// Close as liquidated: the position's margin is lost, plus the entry fee
    /// already paid. Returns the realised net P&L (account currency, <= 0).
    pub fn close_liquidated(&mut self, fx_to_account: f64) -> f64 {
        let liq = self.order.liquidation_price.unwrap_or(self.avg_entry);
        let open = self.filled_qty - self.exited_qty;
        self.exited_qty = self.filled_qty;
        self.exit_value += open * liq;
        self.exit_reason = Some(ExitReason::Liquidated);
        self.state = State::Closed;
        let pnl = -(self.order.margin_account + self.entry_fees * fx_to_account);
        self.realised_pnl = Some(pnl);
        pnl
    }

    pub fn age_ms(&self, now_ms: u64) -> Option<u64> {
        self.opened_at_ms.map(|t| now_ms.saturating_sub(t))
    }

    pub fn remaining_ms(&self, now_ms: u64) -> Option<u64> {
        self.age_ms(now_ms).map(|a| T_MAX_MS.saturating_sub(a))
    }
}

/// Emergency flatten: exit actions for every open position.
pub fn flatten_actions(positions: &mut [Position], now_ms: u64) -> ControlResult {
    let actions: Vec<Action> = positions
        .iter_mut()
        .flat_map(|p| p.emergency_exit(now_ms))
        .collect();
    ControlResult {
        ok: true,
        message: format!("flatten: {} exit actions", actions.len()),
        actions,
    }
}

/// Cancel outstanding entry orders.
pub fn cancel_entry_actions(positions: &[Position]) -> ControlResult {
    let actions: Vec<Action> = positions
        .iter()
        .filter(|p| matches!(p.state, State::EntrySubmitted | State::Filling))
        .map(|p| Action::CancelEntryRemainder {
            client_id: p.entry_id(),
        })
        .collect();
    ControlResult {
        ok: true,
        message: format!("cancel entries: {}", actions.len()),
        actions,
    }
}

// ---------------------------------------------------------------- paper

/// Paper execution: taker fills at the touch plus slippage, fee on notional.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperExchange {
    pub taker_fee: f64,
    pub slippage_bps: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub qty: f64,
    pub price: f64,
    pub fee: f64,
}

impl PaperExchange {
    /// `buy` = true lifts the ask; false hits the bid. None on an invalid/crossed book.
    pub fn market_fill(&self, buy: bool, qty: f64, bid: f64, ask: f64) -> Option<Fill> {
        if !(bid > 0.0 && ask >= bid && qty > 0.0) {
            return None;
        }
        let slip = self.slippage_bps / 10_000.0;
        let price = if buy {
            ask * (1.0 + slip)
        } else {
            bid * (1.0 - slip)
        };
        Some(Fill {
            qty,
            price,
            fee: qty * price * self.taker_fee,
        })
    }
}

// ---------------------------------------------------------------- capital

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    Stage1Growth,
    AwaitingCheckpoint,
    Stage2Protective,
    Complete,
}

/// Capital and stage manager. Equity is paper equity unless `paper` is false.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapitalManager {
    pub stage: Stage,
    pub equity: f64,
    pub paper: bool,
    pub stage1_target: f64,
    pub checkpoint_ms: u64,
    pub stage2_target: f64,
    pub withdrawals: Vec<(u64, f64)>,
}

impl CapitalManager {
    pub fn new(
        start_equity: f64,
        paper: bool,
        stage1_target: f64,
        checkpoint_ms: u64,
        stage2_target: f64,
    ) -> Self {
        Self {
            stage: Stage::Stage1Growth,
            equity: start_equity,
            paper,
            stage1_target,
            checkpoint_ms,
            stage2_target,
            withdrawals: vec![],
        }
    }

    pub fn mode(&self) -> CapitalMode {
        match self.stage {
            Stage::Stage1Growth | Stage::AwaitingCheckpoint => CapitalMode::Growth,
            _ => CapitalMode::Protective,
        }
    }

    pub fn on_realised(&mut self, pnl_net: f64) {
        self.equity += pnl_net;
        if self.stage == Stage::Stage2Protective && self.equity >= self.stage2_target {
            self.stage = Stage::Complete;
        }
    }

    /// True when new entries must stop and positions be flattened for the checkpoint.
    pub fn on_clock(&mut self, now_ms: u64) -> bool {
        if self.stage == Stage::Stage1Growth && now_ms >= self.checkpoint_ms {
            self.stage = Stage::AwaitingCheckpoint;
        }
        self.stage == Stage::AwaitingCheckpoint
    }

    /// Record a withdrawal only when the user confirmed it and supplied the reconciled
    /// remaining balance. Moves to stage 2 with that balance as stage start.
    pub fn confirm_withdrawal(
        &mut self,
        amount: f64,
        user_confirmed: bool,
        confirmed_balance: f64,
        now_ms: u64,
    ) -> Result<f64, String> {
        if self.stage != Stage::AwaitingCheckpoint {
            return Err("not at checkpoint".into());
        }
        if !user_confirmed {
            return Err("withdrawal not confirmed by user; not recorded".into());
        }
        self.withdrawals.push((now_ms, amount));
        self.equity = confirmed_balance;
        self.stage = Stage::Stage2Protective;
        Ok(self.equity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst() -> InstrumentCapability {
        InstrumentCapability {
            pair: "B-BTC_USDT".into(),
            margin_currency: "INR".into(),
            active: true,
            exit_only: false,
            max_leverage_long: 50.0,
            max_leverage_short: 40.0,
            tiers: vec![
                LeverageTier {
                    leverage: 50.0,
                    max_notional: 1_000.0,
                },
                LeverageTier {
                    leverage: 20.0,
                    max_notional: 100_000.0,
                },
            ],
            min_notional: 1.0,
            min_quantity: 0.001,
            max_quantity: 100.0,
            quantity_increment: 0.001,
            price_increment: 0.1,
            taker_fee: 0.0005,
            maker_fee: 0.0002,
            maintenance_margin_rate: Some(0.005),
            margin_modes: vec![MarginMode::Isolated],
            supports_market_order: true,
        }
    }
    fn prop(side: Side) -> TradeProposal {
        let (sl, tp) = match side {
            Side::Long => (99.0, 102.0),
            Side::Short => (101.0, 98.0),
        };
        TradeProposal {
            trace_id: "t1".into(),
            pair: "B-BTC_USDT".into(),
            side,
            entry_price: 100.0,
            stop_loss: sl,
            take_profit: tp,
            expected_net_edge: 0.1,
            p_tp_first: 0.5,
            calibrated: true,
            data_healthy: true,
            cost_per_unit: 0.0,
            fx_to_account: 1.0,
        }
    }
    fn acct(e: f64) -> AccountSnapshot {
        AccountSnapshot {
            equity: e,
            available_margin: e,
            paper: true,
        }
    }
    fn approved(d: Decision) -> ApprovedOrder {
        match d {
            Decision::Approved(o) => o,
            Decision::Rejected { reasons, .. } => panic!("rejected: {reasons:?}"),
        }
    }
    fn rejected(d: &Decision) -> bool {
        matches!(d, Decision::Rejected { .. })
    }

    #[test]
    fn sizing_uses_risk_budget_and_highest_leverage() {
        let mut g = RiskGovernor::new(RiskPolicy::growth(), 100.0, 18_000.0);
        let o = approved(g.evaluate_proposal(&prop(Side::Long), &inst(), &acct(100.0), 0));
        // R = 1.0, stop distance 1.0 -> 1.0 unit
        assert!((o.qty - 1.0).abs() < 1e-9);
        assert!((o.risk_at_stop - 1.0).abs() < 1e-9);
        // notional 100 in 50x tier; liq dist = 100*(0.02-0.005) = 1.5 >= 1.5*1.0
        assert_eq!(o.leverage, 50.0);
        assert!((o.margin_account - 2.0).abs() < 1e-9);
        assert_eq!(g.audit.last().unwrap().kind, "approved");
    }

    #[test]
    fn short_uses_short_max_leverage() {
        let mut g = RiskGovernor::new(RiskPolicy::growth(), 100.0, 18_000.0);
        let o = approved(g.evaluate_proposal(&prop(Side::Short), &inst(), &acct(100.0), 0));
        assert_eq!(o.leverage, 40.0);
        assert!(o.liquidation_price.unwrap() > o.stop_loss);
    }

    #[test]
    fn leverage_steps_down_when_liquidation_inside_stop() {
        let mut i = inst();
        i.maintenance_margin_rate = Some(0.015);
        let mut g = RiskGovernor::new(RiskPolicy::growth(), 100.0, 18_000.0);
        let o = approved(g.evaluate_proposal(&prop(Side::Long), &i, &acct(100.0), 0));
        // need 1/L - 0.015 >= 0.015 -> L <= 33.3
        assert_eq!(o.leverage, 33.0);
    }

    #[test]
    fn tier_caps_leverage_by_notional() {
        let i = inst();
        assert_eq!(
            max_permitted_leverage(&i, Side::Long, 500.0, f64::INFINITY),
            50.0
        );
        assert_eq!(
            max_permitted_leverage(&i, Side::Long, 5_000.0, f64::INFINITY),
            20.0
        );
        assert_eq!(
            max_permitted_leverage(&i, Side::Long, 1e9, f64::INFINITY),
            0.0
        );
    }

    #[test]
    fn min_notional_rejects_never_raises_budget() {
        let mut i = inst();
        i.min_notional = 500.0;
        let mut g = RiskGovernor::new(RiskPolicy::growth(), 100.0, 18_000.0);
        assert!(rejected(&g.evaluate_proposal(
            &prop(Side::Long),
            &i,
            &acct(100.0),
            0
        )));
        assert_eq!(g.open_positions, 0);
    }

    #[test]
    fn uncalibrated_exit_only_and_kill_rejected() {
        let mut g = RiskGovernor::new(RiskPolicy::growth(), 100.0, 18_000.0);
        let mut p = prop(Side::Long);
        p.calibrated = false;
        assert!(rejected(&g.evaluate_proposal(&p, &inst(), &acct(100.0), 0)));
        let mut i = inst();
        i.exit_only = true;
        assert!(rejected(&g.evaluate_proposal(
            &prop(Side::Long),
            &i,
            &acct(100.0),
            0
        )));
        let mut p = prop(Side::Long);
        p.expected_net_edge = -0.01;
        assert!(rejected(&g.evaluate_proposal(&p, &inst(), &acct(100.0), 0)));
        g.kill_switch(0);
        assert!(!g.pause_entries(false, 0).ok);
        assert!(rejected(&g.evaluate_proposal(
            &prop(Side::Long),
            &inst(),
            &acct(100.0),
            0
        )));
    }

    #[test]
    fn no_escalation_after_loss_and_session_limit() {
        let mut g = RiskGovernor::new(RiskPolicy::growth(), 100.0, 18_000.0);
        g.open_positions = 1;
        g.record_realised(-1.0, 99.0, 0);
        assert!(g.effective_risk_fraction() < 0.01);
        g.open_positions = 1;
        g.record_realised(-4.0, 95.0, 0);
        // session loss 5 >= 5% of 100
        assert!(rejected(&g.evaluate_proposal(
            &prop(Side::Long),
            &inst(),
            &acct(100.0),
            0
        )));
    }

    #[test]
    fn high_water_mark_protects() {
        let mut g = RiskGovernor::new(RiskPolicy::protective(), 8_000.0, 50_000.0);
        g.open_positions = 1;
        g.record_realised(2_000.0, 10_000.0, 0);
        assert!(rejected(&g.evaluate_proposal(
            &prop(Side::Long),
            &inst(),
            &acct(9_700.0),
            0
        )));
    }

    #[test]
    fn policy_cannot_exceed_ceiling() {
        let mut p = RiskPolicy::protective();
        p.risk_fraction = 0.05;
        p.max_session_loss_fraction = 0.5;
        let c = p.clamped();
        assert_eq!(c.risk_fraction, 0.005);
        assert_eq!(c.max_session_loss_fraction, 0.02);
    }

    #[test]
    fn final_target_stops_entries() {
        let mut g = RiskGovernor::new(RiskPolicy::protective(), 8_000.0, 50_000.0);
        assert!(rejected(&g.evaluate_proposal(
            &prop(Side::Long),
            &inst(),
            &acct(50_000.0),
            0
        )));
    }

    fn open_position() -> Position {
        let mut g = RiskGovernor::new(RiskPolicy::growth(), 100.0, 18_000.0);
        let o = approved(g.evaluate_proposal(&prop(Side::Long), &inst(), &acct(100.0), 0));
        Position::new(o, &RiskPolicy::growth())
    }

    #[test]
    fn lifecycle_timer_starts_at_fill_and_exits_before_120s() {
        let mut p = open_position();
        p.start(1_000);
        assert!(p.on_fill(0.4, 100.0, 0.0, 5_000).is_empty());
        let a = p.on_fill(0.6, 101.0, 0.0, 6_000);
        assert!(matches!(a[0], Action::AttachProtection { .. }));
        assert!((p.avg_entry - 100.6).abs() < 1e-9);
        p.on_protection_confirmed();
        assert_eq!(p.state, State::Protected);
        // opened at 5000 (first fill), lead 3000 -> exit at 122000
        assert!(p.on_timer(121_999).is_empty());
        let a = p.on_timer(122_000);
        assert!(matches!(
            a[0],
            Action::SubmitExit {
                reason: ExitReason::TimeExpiry,
                ..
            }
        ));
        let pnl = p.on_exit_fill(1.0, 100.6, 0.0, 1.0).unwrap();
        assert!(pnl.abs() < 1e-9);
        assert_eq!(p.state, State::Closed);
    }

    #[test]
    fn stop_and_edge_and_data_exits() {
        let mut p = open_position();
        p.start(0);
        p.on_fill(1.0, 100.0, 0.0, 0);
        p.on_protection_confirmed();
        let a = p.on_market_tick(98.9, 99.0, 0.1, true, 10);
        assert!(matches!(
            a[0],
            Action::SubmitExit {
                reason: ExitReason::StopLoss,
                ..
            }
        ));
        let mut p = open_position();
        p.start(0);
        p.on_fill(1.0, 100.0, 0.0, 0);
        let a = p.on_market_tick(100.0, 100.1, -0.01, true, 10);
        assert!(matches!(
            a[0],
            Action::SubmitExit {
                reason: ExitReason::EdgeNegative,
                ..
            }
        ));
        let mut p = open_position();
        p.start(0);
        p.on_fill(1.0, 100.0, 0.0, 0);
        let a = p.on_market_tick(100.0, 100.1, 0.1, false, 10);
        assert!(matches!(
            a[0],
            Action::SubmitExit {
                reason: ExitReason::DataIntegrity,
                ..
            }
        ));
    }

    #[test]
    fn protection_failure_triggers_emergency_exit_and_incident() {
        let mut p = open_position();
        p.start(0);
        p.on_fill(1.0, 100.0, 0.0, 0);
        let a = p.on_protection_failed(5, "rejected by exchange");
        assert!(matches!(a[0], Action::Incident { .. }));
        assert!(matches!(
            a[1],
            Action::SubmitExit {
                reason: ExitReason::ProtectionFailed,
                ..
            }
        ));
    }

    #[test]
    fn timeout_reconciles_before_resend() {
        let mut p = open_position();
        p.start(0);
        let a = p.on_timer(2_000);
        assert!(matches!(a[0], Action::Reconcile { .. }));
        // exchange shows it actually filled: no duplicate entry, protect instead
        let a = p.on_reconcile(1.0, 2_100);
        assert!(a
            .iter()
            .any(|x| matches!(x, Action::AttachProtection { .. })));
        assert!(!a.iter().any(|x| matches!(x, Action::SubmitEntry { .. })));
        p.on_protection_confirmed();
        p.on_market_tick(98.0, 98.1, 0.1, true, 3_000);
        let a = p.on_timer(5_000);
        assert!(matches!(a[0], Action::Reconcile { .. }));
        assert!(p
            .on_reconcile(0.0, 5_100)
            .iter()
            .all(|x| !matches!(x, Action::SubmitExit { .. })));
        assert_eq!(p.state, State::Closed);
    }

    #[test]
    fn paper_fill_costs_and_pnl() {
        let px = PaperExchange {
            taker_fee: 0.0005,
            slippage_bps: 1.0,
        };
        let b = px.market_fill(true, 1.0, 99.9, 100.0).unwrap();
        assert!((b.price - 100.01).abs() < 1e-9);
        let s = px.market_fill(false, 1.0, 102.0, 102.1).unwrap();
        let mut p = open_position();
        p.start(0);
        p.on_fill(b.qty, b.price, b.fee, 0);
        let pnl = p.on_exit_fill(s.qty, s.price, s.fee, 1.0).unwrap();
        let expect = (s.price - b.price) - b.fee - s.fee;
        assert!((pnl - expect).abs() < 1e-9);
        assert!(px.market_fill(true, 1.0, 100.0, 99.0).is_none());
    }

    #[test]
    fn controls_flatten_and_cancel() {
        let mut ps = vec![open_position(), open_position()];
        ps[0].start(0);
        ps[1].start(0);
        ps[1].on_fill(1.0, 100.0, 0.0, 0);
        assert_eq!(cancel_entry_actions(&ps).actions.len(), 1);
        let r = flatten_actions(&mut ps, 10);
        assert_eq!(r.actions.len(), 1);
    }

    #[test]
    fn capital_checkpoint_requires_user_confirmation() {
        let mut c = CapitalManager::new(100.0, true, 18_000.0, 1_795_451_400_000, 50_000.0);
        assert!(!c.on_clock(1_795_451_399_999));
        assert!(c.on_clock(1_795_451_400_000));
        assert!(c.confirm_withdrawal(10_000.0, false, 8_000.0, 0).is_err());
        assert!(c.withdrawals.is_empty());
        assert_eq!(
            c.confirm_withdrawal(10_000.0, true, 8_000.0, 1).unwrap(),
            8_000.0
        );
        assert_eq!(c.mode(), CapitalMode::Protective);
        c.on_realised(42_000.0);
        assert_eq!(c.stage, Stage::Complete);
    }

    fn exp_gov() -> RiskGovernor {
        RiskGovernor::new(RiskPolicy::experiment_max_leverage(), 100.0, 18_000.0)
    }

    #[test]
    fn experiment_sizes_all_equity_at_max_leverage() {
        let mut g = exp_gov();
        let mut p = prop(Side::Long);
        p.calibrated = false; // calibration gate is off in the experiment
        let mut i = inst();
        i.tiers = vec![
            LeverageTier {
                leverage: 50.0,
                max_notional: 10_000.0,
            },
            LeverageTier {
                leverage: 20.0,
                max_notional: 100_000.0,
            },
        ];
        let o = approved(g.evaluate_proposal(&p, &i, &acct(100.0), 0));
        // notional = 100 * 50 = 5000 -> 50 units at 100
        assert_eq!(o.leverage, 50.0);
        assert!((o.qty - 50.0).abs() < 1e-9);
        assert!((o.notional_account - 5_000.0).abs() < 1e-6);
        assert!((o.margin_account - 100.0).abs() < 1e-6);
        // liq = 100 * (1 - (0.02 - 0.005)) = 98.5, inside the 99 stop is allowed
        assert!((o.liquidation_price.unwrap() - 98.5).abs() < 1e-9);
        // short uses the short side maximum
        let mut g = exp_gov();
        let o = approved(g.evaluate_proposal(&prop(Side::Short), &i, &acct(100.0), 0));
        assert_eq!(o.leverage, 40.0);
        assert!((o.qty - 40.0).abs() < 1e-9);
    }

    #[test]
    fn experiment_steps_to_the_tier_that_admits_the_notional() {
        // 100 * 50 = 5000 > 1000 (50x tier) -> 20x tier: notional 2000
        let mut g = exp_gov();
        let o = approved(g.evaluate_proposal(&prop(Side::Long), &inst(), &acct(100.0), 0));
        assert_eq!(o.leverage, 20.0);
        assert!((o.notional_account - 2_000.0).abs() < 1e-6);
        // quantity rounds down to the increment, never up
        let mut g = exp_gov();
        let o = approved(g.evaluate_proposal(&prop(Side::Long), &inst(), &acct(33.3337), 0));
        assert!(o.qty * 100.0 <= 33.3337 * 20.0 + 1e-9);
        assert!((o.qty - 6.666).abs() < 1e-9);
    }

    #[test]
    fn experiment_keeps_ev_min_notional_and_one_position() {
        let mut g = exp_gov();
        let mut p = prop(Side::Long);
        p.expected_net_edge = -0.001;
        assert!(rejected(&g.evaluate_proposal(&p, &inst(), &acct(100.0), 0)));
        let mut i = inst();
        i.min_notional = 1_000_000.0;
        let d = g.evaluate_proposal(&prop(Side::Long), &i, &acct(100.0), 0);
        match d {
            Decision::Rejected { reasons, .. } => {
                assert!(
                    reasons[0].contains("minimum tradable notional"),
                    "{reasons:?}"
                )
            }
            _ => panic!("approved below min notional"),
        }
        approved(g.evaluate_proposal(&prop(Side::Long), &inst(), &acct(100.0), 0));
        assert!(rejected(&g.evaluate_proposal(
            &prop(Side::Long),
            &inst(),
            &acct(100.0),
            0
        )));
        // a big loss does not stop the experiment (no session/HWM stop), but
        // equity below the tradable minimum does
        g.record_realised(-60.0, 40.0, 1);
        approved(g.evaluate_proposal(&prop(Side::Long), &inst(), &acct(40.0), 2));
        g.record_realised(0.0, 40.0, 3);
        assert!(rejected(&g.evaluate_proposal(
            &prop(Side::Long),
            &inst(),
            &acct(0.0),
            4
        )));
    }

    #[test]
    fn standard_profile_is_unchanged_by_default() {
        assert_eq!(RiskPolicy::growth().profile, Profile::Standard);
        assert_eq!(RiskPolicy::protective().profile, Profile::Standard);
        let mut g = RiskGovernor::new(RiskPolicy::growth(), 100.0, 18_000.0);
        let mut p = prop(Side::Long);
        p.calibrated = false;
        assert!(rejected(&g.evaluate_proposal(&p, &inst(), &acct(100.0), 0)));
    }

    #[test]
    fn liquidation_closes_with_margin_lost() {
        let mut g = exp_gov();
        let o = approved(g.evaluate_proposal(&prop(Side::Long), &inst(), &acct(100.0), 0));
        let liq = o.liquidation_price.unwrap();
        let margin = o.margin_account;
        let mut pos = Position::new(o, &g.policy);
        pos.start(0);
        pos.on_fill(pos.order.qty, 100.0, 1.0, 0);
        pos.on_protection_confirmed();
        assert!(!pos.liquidation_hit(liq + 0.01, liq + 0.02));
        assert!(pos.liquidation_hit(liq, liq + 0.01));
        let pnl = pos.close_liquidated(1.0);
        assert!((pnl + margin + 1.0).abs() < 1e-9);
        assert_eq!(pos.state, State::Closed);
        assert_eq!(pos.exit_reason, Some(ExitReason::Liquidated));
        assert!(!pos.liquidation_hit(0.0, 0.0));
    }

    fn gamble_gov(policy: &str) -> RiskGovernor {
        let raw = gamble::RawGamble {
            enabled: Some(true),
            owner_ack: Some(gamble::OWNER_ACK.into()),
            stake_inr: Some("10".into()),
            leverage: Some("10".into()),
            floor_inr: Some("50".into()),
            stake_policy: Some(policy.into()),
        };
        let p = gamble::validate(&raw, 100.0).unwrap();
        RiskGovernor::new(RiskPolicy::gamble(p), 100.0, 50_000.0)
    }

    #[test]
    fn gamble_skips_calibration_and_ev_but_sizes_owner_stake() {
        let mut g = gamble_gov("fixed");
        let mut p = prop(Side::Long);
        p.calibrated = false;
        p.expected_net_edge = -0.01;
        let o = approved(g.evaluate_proposal(&p, &inst(), &acct(100.0), 0));
        assert_eq!(o.leverage, 10.0, "owner leverage, below exchange max 50");
        assert!((o.margin_account - 10.0).abs() < 1e-9);
        assert!((o.notional_account - 100.0).abs() < 1e-9);
        // one position: a second bet is refused while the first is open
        let d = g.evaluate_proposal(&p, &inst(), &acct(100.0), 1);
        assert!(rejected(&d));
        // data integrity and kill switch still apply
        let mut g = gamble_gov("fixed");
        p.data_healthy = false;
        assert!(rejected(&g.evaluate_proposal(&p, &inst(), &acct(100.0), 0)));
        p.data_healthy = true;
        g.kill_switch(0);
        assert!(rejected(&g.evaluate_proposal(&p, &inst(), &acct(100.0), 0)));
    }

    #[test]
    fn gamble_leverage_capped_by_exchange_and_stop_inside_liquidation() {
        let raw = gamble::RawGamble {
            enabled: Some(true),
            owner_ack: Some(gamble::OWNER_ACK.into()),
            stake_inr: Some("10".into()),
            leverage: Some("200".into()),
            floor_inr: Some("50".into()),
            stake_policy: None,
        };
        let pol = RiskPolicy::gamble(gamble::validate(&raw, 100.0).unwrap());
        let mut g = RiskGovernor::new(pol, 100.0, 50_000.0);
        // 50x cap (short 40x): liquidation ~1.5% away, stop 1% away -> allowed
        let o = approved(g.evaluate_proposal(&prop(Side::Long), &inst(), &acct(100.0), 0));
        assert_eq!(o.leverage, 50.0);
        // a 2% stop lies beyond the 50x liquidation -> refused
        let mut g = RiskGovernor::new(g.policy.clone(), 100.0, 50_000.0);
        let mut p = prop(Side::Long);
        p.stop_loss = 98.0;
        assert!(rejected(&g.evaluate_proposal(&p, &inst(), &acct(100.0), 0)));
    }

    #[test]
    fn gamble_floor_stop_is_permanent() {
        let mut g = gamble_gov("fixed");
        assert_eq!(g.gamble_floor_check(60.0, 0.6, 0), None);
        assert!(g.gamble_floor_check(50.3, 0.6, 0).is_some());
        assert!(g.entries_paused);
        assert!(!g.pause_entries(false, 1).ok, "only the owner re-arms");
        assert!(rejected(&g.evaluate_proposal(
            &prop(Side::Long),
            &inst(),
            &acct(100.0),
            2
        )));
        // the bet itself never risks the floor: margin + fees fit above it
        let mut g = gamble_gov("fixed");
        let o = approved(g.evaluate_proposal(&prop(Side::Long), &inst(), &acct(52.0), 0));
        assert!(52.0 - o.margin_account >= 50.0 - 1e-9);
    }

    #[test]
    fn gamble_never_sizes_up_after_a_loss() {
        let mut g = gamble_gov("target_optimal");
        let o1 = approved(g.evaluate_proposal(&prop(Side::Long), &inst(), &acct(100.0), 0));
        g.record_realised(-1.0, 99.0, 1);
        let o2 = approved(g.evaluate_proposal(&prop(Side::Long), &inst(), &acct(150.0), 2));
        assert!(o2.notional_account <= o1.notional_account + 1e-9);
    }
}
