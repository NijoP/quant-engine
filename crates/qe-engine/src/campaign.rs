//! 24 November 2026 campaign controller: INR 100 -> INR 50,000 between 00:00 and
//! 22:00 IST (22 hours, 500x, no intermediate withdrawals, no resets).
//!
//! # Mathematics
//!
//! `target_equity(h) = start * (target / start) ^ (h / 22)` with start = 100,
//! target = 50,000 and `h` the elapsed hours (fractional, clamped to `[0, 22]`).
//! The hourly multiplier is `500 ^ (1/22) = 1.3264181...`, i.e. a required
//! compounded growth of about 32.6418 % per hour.
//!
//! This curve is a **mathematical requirement, not a trading forecast**. In the
//! default protective profile it is a progress benchmark only: the per-trade
//! budget ([`CampaignConfig::trade_budget`]) is identical whether the campaign is
//! ahead, on track or behind. The owner-selected **bold-play** profile
//! ([`CampaignConfig::bold`], configs/campaign-live.toml, owner instruction of
//! 23 Nov 2026 23:21 IST) sizes each bet so one TP win reaches the next hourly
//! milestone ([`CampaignConfig::bold_stake`]), capped by the equity above an
//! owner-set floor; it has no proven edge and can lose everything above the floor.
//!
//! # Rounding (documented, applied consistently)
//!
//! - The power is evaluated in `f64` (about 15 significant digits; the error at
//!   INR 50,000 is below 1e-9 INR) and converted to `Decimal`. h = 0 gives exactly
//!   100 and h = 22 exactly 50,000.
//! - **Hourly checkpoints** (`required_inr` in the milestone table) are rounded to
//!   the whole rupee, half away from zero. This reproduces the published 23-row
//!   schedule exactly (regression-tested below).
//! - **Continuous values** (required equity now, partial hours, deltas, P&L,
//!   remaining) are rounded to 2 dp (paise), half away from zero.
//! - Status compares unrounded actual equity against the 2 dp required value.
//!
//! # Risk profile (never relaxed to make the target achievable)
//!
//! Floor INR 95; at most INR 1 planned loss per trade *including* estimated fees,
//! slippage and the stop-loss loss; at most INR 5 cumulative loss from the INR 100
//! start; one open position; 120 s maximum hold; no martingale, averaging down or
//! size increase after a loss. Reaching the floor or exhausting the loss budget
//! enters a **permanent** stop that survives restarts (the state is serialisable
//! and the desk persists it); a new campaign requires explicit owner review.
//!
//! At 22:00 IST the campaign enters exit-only mode; the deadline workflow records
//! each close/cancel/reconcile step and never reports the account flat unless the
//! reconciliation source confirmed it.

use rust_decimal::prelude::*;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// 2026-11-24 00:00 IST (2026-11-23T18:30:00Z).
pub const CAMPAIGN_START_MS: i64 = 1_795_458_600_000;
/// 2026-11-24 22:00 IST (2026-11-24T16:30:00Z).
pub const CAMPAIGN_DEADLINE_MS: i64 = 1_795_537_800_000;
pub const CAMPAIGN_HOURS: u32 = 22;
pub const HOUR_MS: i64 = 3_600_000;
/// IST = UTC + 05:30, no daylight saving.
pub const IST_OFFSET_MS: i64 = 19_800_000;

/// Where the reported equity comes from. Paper equity is never real capital.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EquitySource {
    /// A verified CoinDCX account balance (only when the owner armed live).
    LiveExchange,
    /// The simulated paper ledger.
    Paper,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CampaignStatus {
    Ahead,
    OnTrack,
    Behind,
    Stopped,
    Completed,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CampaignConfig {
    pub start_ms: i64,
    pub deadline_ms: i64,
    pub start_equity: Decimal,
    pub target: Decimal,
    /// Absolute equity floor (INR 95).
    pub floor: Decimal,
    /// Maximum planned loss per trade incl. fees, slippage and stop loss (INR 1).
    pub max_loss_per_trade: Decimal,
    /// Maximum cumulative loss from the starting equity (INR 5).
    pub max_campaign_loss: Decimal,
    pub max_open_positions: usize,
    pub max_hold_secs: u32,
    /// Relative band around the required equity that counts as "on track".
    pub on_track_band: Decimal,
    /// Owner-selected campaign bold-play profile (configs/campaign-live.toml):
    /// the per-trade budget is all equity above the owner floor and the INR 1 /
    /// INR 5 caps and the no-size-up-after-loss rule do not apply. Default false
    /// (the protective INR 1 / INR 5 profile).
    #[serde(default)]
    pub bold_play: bool,
}

impl Default for CampaignConfig {
    fn default() -> Self {
        Self {
            start_ms: CAMPAIGN_START_MS,
            deadline_ms: CAMPAIGN_DEADLINE_MS,
            start_equity: dec!(100),
            target: dec!(50000),
            floor: dec!(95),
            max_loss_per_trade: dec!(1),
            max_campaign_loss: dec!(5),
            max_open_positions: 1,
            max_hold_secs: 120,
            on_track_band: dec!(0.01),
            bold_play: false,
        }
    }
}

impl CampaignConfig {
    /// Bold-play profile with an owner-set floor (required, no default).
    pub fn bold(floor: Decimal) -> Self {
        Self {
            floor,
            max_loss_per_trade: dec!(50000),
            max_campaign_loss: dec!(100) - floor,
            bold_play: true,
            ..Self::default()
        }
    }

    /// Bold-play margin so that one take-profit win (net of round-trip cost,
    /// `net_tp_frac` of notional) lifts equity to the next hourly milestone,
    /// capped by the equity above the floor. Returns INR margin (may be 0).
    pub fn bold_stake(
        &self,
        equity: f64,
        next_required: f64,
        leverage: f64,
        net_tp_frac: f64,
    ) -> f64 {
        let above = (equity - self.floor.to_f64().unwrap_or(f64::INFINITY)).max(0.0);
        if !(leverage >= 1.0 && net_tp_frac > 0.0 && equity.is_finite()) {
            return 0.0;
        }
        let need = (next_required - equity).max(0.0);
        (need / (leverage * net_tp_frac)).min(above)
    }
}

fn r2(x: Decimal) -> Decimal {
    let mut y = x.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
    y.rescale(2); // always two decimals in strings ("1.00")
    y
}

fn r0(x: Decimal) -> Decimal {
    x.round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
}

impl CampaignConfig {
    pub fn duration_hours(&self) -> f64 {
        (self.deadline_ms - self.start_ms) as f64 / HOUR_MS as f64
    }

    /// `(target/start)^(1/hours)`: the constant hourly multiplier (1.326418...).
    pub fn hourly_multiplier(&self) -> f64 {
        let m = (self.target / self.start_equity)
            .to_f64()
            .unwrap_or(f64::NAN);
        m.powf(1.0 / self.duration_hours())
    }

    /// Unrounded `start * (target/start)^(h/H)`, `h` clamped to `[0, H]`.
    pub fn target_equity_exact(&self, h: f64) -> Decimal {
        let hh = self.duration_hours();
        let h = if h.is_finite() { h.clamp(0.0, hh) } else { 0.0 };
        if h <= 0.0 {
            return self.start_equity;
        }
        if h >= hh {
            return self.target;
        }
        let m = (self.target / self.start_equity)
            .to_f64()
            .unwrap_or(f64::NAN);
        let s = self.start_equity.to_f64().unwrap_or(f64::NAN);
        Decimal::from_f64(s * m.powf(h / hh)).unwrap_or(self.start_equity)
    }

    /// Hourly checkpoint requirement, whole rupees (the published table).
    pub fn checkpoint_required(&self, hour: u32) -> Decimal {
        r0(self.target_equity_exact(hour as f64))
    }

    /// Fractional hours elapsed since the start (may be negative or > H).
    pub fn elapsed_hours(&self, now_ms: i64) -> f64 {
        (now_ms - self.start_ms) as f64 / HOUR_MS as f64
    }

    /// Required equity at `now_ms` (continuous curve), 2 dp.
    pub fn required_at(&self, now_ms: i64) -> Decimal {
        r2(self.target_equity_exact(self.elapsed_hours(now_ms)))
    }

    pub fn checkpoint_ms(&self, hour: u32) -> i64 {
        self.start_ms + hour as i64 * HOUR_MS
    }

    /// The per-trade planned-loss budget allowed right now. It depends only on the
    /// fixed limits, the equity and the loss already used: never on the schedule.
    pub fn trade_budget(&self, equity: Decimal, loss_used: Decimal) -> Decimal {
        if self.bold_play {
            return (equity - self.floor).max(Decimal::ZERO);
        }
        let remaining = (self.max_campaign_loss - loss_used).max(Decimal::ZERO);
        let above_floor = (equity - self.floor).max(Decimal::ZERO);
        self.max_loss_per_trade
            .min(remaining)
            .min(above_floor)
            .max(Decimal::ZERO)
    }
}

/// Planned worst-case loss of one trade, all in INR (account currency).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlannedLoss {
    /// Loss if the stop fills exactly at the stop price.
    pub stop_loss_inr: Decimal,
    /// Estimated entry + exit fees (for the actual order type used).
    pub fees_inr: Decimal,
    /// Estimated entry + exit slippage.
    pub slippage_inr: Decimal,
}

impl PlannedLoss {
    pub fn total(&self) -> Decimal {
        self.stop_loss_inr + self.fees_inr + self.slippage_inr
    }
}

/// What the governor approved, as the campaign needs to see it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TradeCheck {
    pub planned: PlannedLoss,
    /// Account-currency notional of the approved order.
    pub notional_inr: Decimal,
    /// Exchange minimum notional in account currency.
    pub min_notional_inr: Decimal,
    /// Stop-loss price and estimated liquidation price (same quote currency).
    pub stop_price: f64,
    pub liquidation_price: Option<f64>,
    pub long: bool,
    pub isolated_margin: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeadlineStep {
    StopEntries,
    CancelEntryOrders,
    Flatten,
    ReconcilePositions,
    ReconcileProtectiveOrders,
    VerifyFlat,
    RecordFinal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepRecord {
    pub step: DeadlineStep,
    pub ok: bool,
    pub at_ms: i64,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinalRecord {
    pub at_ms: i64,
    pub equity_inr: Option<Decimal>,
    pub equity_source: EquitySource,
    /// True only when the reconciliation source confirmed zero positions/orders.
    pub verified_flat: bool,
    pub flat_source: String,
    pub result: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CampaignEvent {
    pub ts_ms: i64,
    pub kind: String,
    pub message: String,
    pub data: Value,
}

/// The campaign state. Serialisable so a restart resumes it (including a stop).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Campaign {
    pub cfg: CampaignConfig,
    pub equity_source: EquitySource,
    /// Latest verified equity (realised ledger or exchange balance); None = unknown.
    pub equity: Option<Decimal>,
    pub unrealised: Decimal,
    pub realised_pnl: Decimal,
    pub peak_equity: Decimal,
    /// Actual equity recorded at each hourly checkpoint (index = hour).
    pub checkpoint_actual: Vec<Option<Decimal>>,
    pub history: Vec<(i64, Decimal)>,
    pub stop_reason: Option<String>,
    pub stopped_at_ms: Option<i64>,
    pub exit_only: bool,
    pub completed_at_ms: Option<i64>,
    /// Planned loss of the last losing trade: the next trade may not exceed it.
    pub last_loss_planned: Option<Decimal>,
    pub last_planned: Option<Decimal>,
    pub deadline_steps: Vec<StepRecord>,
    pub final_record: Option<FinalRecord>,
    /// Gate reason last logged (transition-only logging of repeated blocks).
    pub last_gate: Option<String>,
    #[serde(skip)]
    pub events: Vec<CampaignEvent>,
}

impl Campaign {
    pub fn new(cfg: CampaignConfig, source: EquitySource) -> Self {
        let start = cfg.start_equity;
        Self {
            checkpoint_actual: vec![None; CAMPAIGN_HOURS as usize + 1],
            cfg,
            equity_source: source,
            equity: None,
            unrealised: Decimal::ZERO,
            realised_pnl: Decimal::ZERO,
            peak_equity: start,
            history: vec![],
            stop_reason: None,
            stopped_at_ms: None,
            exit_only: false,
            completed_at_ms: None,
            last_loss_planned: None,
            last_planned: None,
            deadline_steps: vec![],
            final_record: None,
            last_gate: None,
            events: vec![],
        }
    }

    /// Restore persisted state. A stopped campaign stays stopped; a corrupt or
    /// foreign (different schedule) state is refused rather than reset silently.
    pub fn restore(json: &str, cfg: &CampaignConfig) -> Result<Self, String> {
        let c: Campaign = serde_json::from_str(json).map_err(|e| format!("campaign state: {e}"))?;
        if &c.cfg != cfg {
            return Err(
                "persisted campaign has a different schedule/risk profile: owner review required"
                    .into(),
            );
        }
        if c.checkpoint_actual.len() != CAMPAIGN_HOURS as usize + 1 {
            return Err("persisted campaign checkpoint table is malformed".into());
        }
        Ok(c)
    }

    fn event(&mut self, now: i64, kind: &str, message: String, data: Value) {
        self.events.push(CampaignEvent {
            ts_ms: now,
            kind: kind.into(),
            message,
            data,
        });
    }

    pub fn drain_events(&mut self) -> Vec<CampaignEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn is_stopped(&self) -> bool {
        self.stop_reason.is_some()
    }

    /// Equity marked to market (realised ledger + open losses, never open gains).
    pub fn marked_equity(&self) -> Option<Decimal> {
        self.equity.map(|e| e + self.unrealised.min(Decimal::ZERO))
    }

    /// Loss used against the INR 5 budget, from the starting equity.
    pub fn loss_used(&self) -> Decimal {
        self.marked_equity().map_or(Decimal::ZERO, |e| {
            (self.cfg.start_equity - e).max(Decimal::ZERO)
        })
    }

    pub fn drawdown(&self) -> Decimal {
        self.marked_equity()
            .map_or(Decimal::ZERO, |e| (self.peak_equity - e).max(Decimal::ZERO))
    }

    pub fn risk_budget_now(&self) -> Decimal {
        if self.is_stopped() || self.exit_only {
            return Decimal::ZERO;
        }
        match self.marked_equity() {
            Some(e) => self.cfg.trade_budget(e, self.loss_used()),
            None => Decimal::ZERO,
        }
    }

    fn stop(&mut self, now: i64, reason: String) {
        if self.is_stopped() {
            return;
        }
        self.stop_reason = Some(reason.clone());
        self.stopped_at_ms = Some(now);
        self.exit_only = true;
        let data = json!({"equity": self.equity, "marked_equity": self.marked_equity(),
                          "loss_used": self.loss_used()});
        self.event(
            now,
            "campaign_stop",
            format!(
                "PERMANENT campaign stop: {reason}; owner review required to start a new campaign"
            ),
            data,
        );
    }

    /// Owner-commanded stop (e.g. kill switch): also permanent.
    pub fn owner_stop(&mut self, now: i64, why: &str) {
        self.stop(now, format!("owner stop: {why}"));
    }

    /// Feed an equity observation. Updates history, checkpoints, peak and the
    /// floor / loss-budget stop. Unrealised P&L is tracked separately.
    pub fn observe(
        &mut self,
        now: i64,
        equity: Option<Decimal>,
        source: EquitySource,
        realised_pnl: Decimal,
        unrealised: Decimal,
    ) {
        if source != self.equity_source {
            let msg = format!("equity source {:?} -> {:?}", self.equity_source, source);
            self.event(now, "equity_source", msg, Value::Null);
            self.equity_source = source;
        }
        self.equity = equity;
        self.unrealised = unrealised;
        self.realised_pnl = realised_pnl;
        let Some(e) = equity else { return };
        let in_window = now >= self.cfg.start_ms && now <= self.cfg.deadline_ms;
        if in_window {
            if e > self.peak_equity {
                self.peak_equity = e;
            }
            let push = match self.history.last() {
                None => true,
                Some((t, v)) => *v != e || now - *t >= 60_000,
            };
            if push {
                self.history.push((now, e));
                if self.history.len() > 2_000 {
                    self.history.remove(0);
                }
            }
            let h = ((now - self.cfg.start_ms) / HOUR_MS) as usize;
            if h <= CAMPAIGN_HOURS as usize && self.checkpoint_actual[h].is_none() {
                self.checkpoint_actual[h] = Some(e);
                let req = self.cfg.checkpoint_required(h as u32);
                self.event(
                    now,
                    "checkpoint",
                    format!(
                        "hour {h} checkpoint: actual INR {} vs required INR {req} (benchmark only)",
                        r2(e)
                    ),
                    json!({"hour": h, "actual": r2(e), "required": req}),
                );
            }
            if e >= self.cfg.target && self.completed_at_ms.is_none() {
                self.completed_at_ms = Some(now);
                self.exit_only = true;
                let msg = format!("target INR {} reached: exit-only", self.cfg.target);
                self.event(now, "completed", msg, Value::Null);
            }
        }
        if let Some(m) = self.marked_equity() {
            if m <= self.cfg.floor {
                let why = format!(
                    "equity INR {} at or below the INR {} floor",
                    r2(m),
                    self.cfg.floor
                );
                self.stop(now, why);
            } else if self.loss_used() >= self.cfg.max_campaign_loss {
                let why = format!(
                    "cumulative loss INR {} exhausted the INR {} budget",
                    r2(self.loss_used()),
                    self.cfg.max_campaign_loss
                );
                self.stop(now, why);
            }
        }
    }

    /// Clock tick: enters exit-only mode at the deadline.
    pub fn tick(&mut self, now: i64) {
        if now >= self.cfg.deadline_ms && !self.exit_only {
            self.exit_only = true;
            let msg = "22:00 IST deadline: exit-only, no new entries".to_string();
            self.event(now, "deadline", msg, Value::Null);
        }
    }

    pub fn expired(&self, now: i64) -> bool {
        now >= self.cfg.deadline_ms
    }

    /// May a new entry be considered at all? Independent of the schedule delta:
    /// an Ok here permits evaluation, it never requires a trade.
    pub fn entry_gate(&mut self, now: i64, open_positions: usize) -> Result<(), String> {
        self.tick(now);
        let r = if let Some(s) = &self.stop_reason {
            Err(format!("campaign stopped (permanent): {s}"))
        } else if now < self.cfg.start_ms {
            Err("campaign not started (starts 00:00 IST 24 Nov 2026)".into())
        } else if self.expired(now) {
            Err("campaign deadline 22:00 IST passed: exit-only".into())
        } else if self.completed_at_ms.is_some() {
            Err("campaign target reached: exit-only".into())
        } else if self.exit_only {
            Err("campaign exit-only".into())
        } else if self.equity.is_none() {
            Err("campaign equity unknown (no verified balance): no entries".into())
        } else if open_positions >= self.cfg.max_open_positions {
            Err(format!(
                "campaign allows {} open position(s)",
                self.cfg.max_open_positions
            ))
        } else if self.risk_budget_now() <= Decimal::ZERO {
            Err("campaign risk budget exhausted".into())
        } else {
            Ok(())
        };
        let key = r.as_ref().err().cloned();
        if key != self.last_gate {
            let msg = match &key {
                None => "entries allowed by campaign gate".to_string(),
                Some(k) => format!("entries blocked: {k}"),
            };
            self.event(now, "entry_gate", msg, Value::Null);
            self.last_gate = key;
        }
        r
    }

    /// Check a governor-approved order against the campaign risk profile. Never
    /// resizes: an order that does not fit is rejected.
    pub fn check_trade(&mut self, now: i64, t: &TradeCheck) -> Result<(), String> {
        let total = t.planned.total();
        let budget = self.risk_budget_now();
        let mut why: Vec<String> = vec![];
        if total > budget {
            why.push(format!(
                "planned loss INR {} (stop {} + fees {} + slippage {}) exceeds campaign budget INR {}",
                total.round_dp(4),
                t.planned.stop_loss_inr.round_dp(4),
                t.planned.fees_inr.round_dp(4),
                t.planned.slippage_inr.round_dp(4),
                budget.round_dp(4)
            ));
        }
        if t.notional_inr < t.min_notional_inr {
            why.push(format!(
                "notional INR {} below exchange minimum INR {} (size is never raised to meet it)",
                t.notional_inr.round_dp(4),
                t.min_notional_inr.round_dp(4)
            ));
        }
        if let Some(prev) = self.last_loss_planned.filter(|_| !self.cfg.bold_play) {
            if total > prev {
                why.push(format!(
                    "no size increase after a loss: planned INR {} > previous INR {}",
                    total.round_dp(4),
                    prev.round_dp(4)
                ));
            }
        }
        match t.liquidation_price {
            Some(liq) if liq.is_finite() && liq > 0.0 => {
                let beyond = if t.long {
                    t.stop_price <= liq
                } else {
                    t.stop_price >= liq
                };
                if beyond {
                    why.push(format!(
                        "stop {} is beyond the estimated liquidation {liq}",
                        t.stop_price
                    ));
                }
            }
            Some(_) => {}
            None => why.push("liquidation price unknown: stop cannot be verified inside it".into()),
        }
        if !t.isolated_margin {
            why.push("isolated margin not confirmed".into());
        }
        let ok = why.is_empty();
        let msg = if ok {
            format!("campaign approved planned loss INR {}", total.round_dp(4))
        } else {
            format!("campaign rejected: {}", why.join("; "))
        };
        let data = json!({"planned": t.planned, "planned_total": total, "budget": budget,
            "notional": t.notional_inr, "min_notional": t.min_notional_inr, "stop": t.stop_price,
            "liquidation": t.liquidation_price, "approved": ok});
        self.event(now, "trade_check", msg, data);
        if ok {
            self.last_planned = Some(total);
            Ok(())
        } else {
            Err(why.join("; "))
        }
    }

    /// Record a closed trade's realised P&L. A loss pins the next planned loss.
    pub fn on_trade_closed(&mut self, now: i64, pnl: Decimal) {
        if pnl < Decimal::ZERO {
            self.last_loss_planned = self.last_planned.or(Some(self.cfg.max_loss_per_trade));
        } else {
            self.last_loss_planned = None;
        }
        let msg = format!("trade closed, realised INR {}", r2(pnl));
        self.event(now, "trade_closed", msg, json!({"pnl": pnl}));
    }

    /// Record a deadline-workflow step (performed by the desk / live executor).
    pub fn record_step(&mut self, now: i64, step: DeadlineStep, ok: bool, detail: String) {
        let msg = format!("{step:?}: {} ({detail})", if ok { "ok" } else { "FAILED" });
        self.event(now, "deadline_step", msg, Value::Null);
        self.deadline_steps.push(StepRecord {
            step,
            ok,
            at_ms: now,
            detail,
        });
    }

    /// Record the final result once. `verified_flat` must come from a
    /// reconciliation (paper ledger or exchange query), never from an assumption.
    pub fn finalize(&mut self, now: i64, verified_flat: bool, flat_source: &str) {
        if self.final_record.is_some() {
            return;
        }
        let eq = self.equity;
        let result = match (self.is_stopped(), eq) {
            (true, _) => "stopped".to_string(),
            (_, Some(e)) if e >= self.cfg.target => "target reached".into(),
            (_, Some(_)) => "target not reached".into(),
            (_, None) => "final equity unknown".into(),
        };
        self.final_record = Some(FinalRecord {
            at_ms: now,
            equity_inr: eq.map(r2),
            equity_source: self.equity_source,
            verified_flat,
            flat_source: flat_source.into(),
            result: result.clone(),
        });
        let msg =
            format!("campaign final: {result}; verified flat: {verified_flat} ({flat_source})");
        self.event(now, "final", msg, Value::Null);
    }

    pub fn status(&self, now: i64) -> CampaignStatus {
        if self.is_stopped() {
            return CampaignStatus::Stopped;
        }
        if self.completed_at_ms.is_some() {
            return CampaignStatus::Completed;
        }
        if self.expired(now) {
            return CampaignStatus::Expired;
        }
        let Some(e) = self.equity else {
            return CampaignStatus::OnTrack;
        };
        let req = self.cfg.required_at(now);
        let band = req * self.cfg.on_track_band;
        if e > req + band {
            CampaignStatus::Ahead
        } else if e < req - band {
            CampaignStatus::Behind
        } else {
            CampaignStatus::OnTrack
        }
    }

    /// `(target / equity)^(1 / remaining_hours) - 1`, reported only. None when
    /// equity is unknown/non-positive or no time remains.
    pub fn required_hourly_rate(&self, now: i64) -> Option<f64> {
        let e = self.equity?.to_f64()?;
        let rem_h = (self.cfg.deadline_ms - now.max(self.cfg.start_ms)) as f64 / HOUR_MS as f64;
        if e <= 0.0 || rem_h <= 0.0 {
            return None;
        }
        let t = self.cfg.target.to_f64()?;
        Some((t / e).powf(1.0 / rem_h) - 1.0)
    }

    pub fn checkpoints_done(&self, now: i64) -> u32 {
        if now < self.cfg.start_ms {
            return 0;
        }
        (((now.min(self.cfg.deadline_ms) - self.cfg.start_ms) / HOUR_MS) as u32).min(CAMPAIGN_HOURS)
    }

    /// The `/api/state.campaign` object (docs/DASHBOARD_API.md).
    pub fn to_json(&self, now: i64, open_position: Value, equity_currency_note: &str) -> Value {
        let c = &self.cfg;
        let ms = |h: u32| {
            json!({"hour": h, "at": iso_ist(c.checkpoint_ms(h)),
                   "required_inr": c.checkpoint_required(h).to_string()})
        };
        let cur = self.checkpoints_done(now);
        let next_h = (cur + 1).min(CAMPAIGN_HOURS);
        let milestones: Vec<Value> = (0..=CAMPAIGN_HOURS)
            .map(|h| {
                json!({"hour": h, "at": iso_ist(c.checkpoint_ms(h)),
                       "required_inr": c.checkpoint_required(h).to_string(),
                       "actual_inr": self.checkpoint_actual[h as usize].map(|v| r2(v).to_string())})
            })
            .collect();
        let req = c.required_at(now);
        let s = |x: Decimal| r2(x).to_string();
        let history: Vec<Value> = self
            .history
            .iter()
            .map(|(t, e)| json!({"t": iso_ist(*t), "equity_inr": s(*e)}))
            .collect();
        json!({
            "start": iso_ist(c.start_ms),
            "deadline": iso_ist(c.deadline_ms),
            "start_equity_inr": c.start_equity.to_string(),
            "target_inr": c.target.to_string(),
            "equity_inr": self.equity.map(s),
            "equity_source": self.equity_source,
            "equity_currency_note": equity_currency_note,
            "required_now_inr": req.to_string(),
            "delta_inr": self.equity.map_or("0.00".to_string(), |e| s(e - req)),
            "prev_milestone": ms(cur),
            "next_milestone": ms(next_h),
            "milestones": milestones,
            "realised_pnl_inr": s(self.realised_pnl),
            "unrealised_pnl_inr": s(self.unrealised),
            "remaining_inr": s(self.equity.map_or(c.target, |e| (c.target - e).max(Decimal::ZERO))),
            "remaining_secs": ((c.deadline_ms - now).max(0) / 1000),
            "drawdown_inr": s(self.drawdown()),
            "required_hourly_rate": self.required_hourly_rate(now),
            "hourly_multiplier": c.hourly_multiplier(),
            "checkpoints_done": self.checkpoints_done(now),
            "status": self.status(now),
            "floor_inr": c.floor.to_string(),
            "loss_budget_inr": c.max_campaign_loss.to_string(),
            "loss_used_inr": s(self.loss_used()),
            "per_trade_max_loss_inr": c.max_loss_per_trade.to_string(),
            "risk_budget_now_inr": s(self.risk_budget_now()),
            "max_hold_secs": c.max_hold_secs,
            "open_position": open_position,
            "stop_reason": self.stop_reason,
            "exit_only": self.exit_only || self.expired(now),
            "deadline_steps": self.deadline_steps,
            "final": self.final_record,
            "equity_history": history,
            "label": "hourly curve = mathematical requirement, not a trading forecast",
        })
    }
}

/// ISO-8601 timestamp in IST with an explicit +05:30 offset.
pub fn iso_ist(ms: i64) -> String {
    let t = ms + IST_OFFSET_MS;
    let secs = t.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+05:30",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: [u32; 23] = [
        100, 133, 176, 233, 310, 411, 545, 722, 958, 1271, 1686, 2236, 2966, 3934, 5218, 6922,
        9181, 12178, 16153, 21425, 28419, 37696, 50000,
    ];
    const INCREASES: [u32; 22] = [
        33, 43, 57, 77, 101, 134, 177, 236, 313, 415, 550, 730, 968, 1284, 1704, 2259, 2997, 3975,
        5272, 6994, 9277, 12304,
    ];

    fn cfg() -> CampaignConfig {
        CampaignConfig::default()
    }

    fn at(eq: &str, now: i64) -> Campaign {
        let mut c = Campaign::new(cfg(), EquitySource::Paper);
        let e: Decimal = eq.parse().unwrap();
        c.observe(
            now,
            Some(e),
            EquitySource::Paper,
            Decimal::ZERO,
            Decimal::ZERO,
        );
        c
    }

    fn check(stop: &str, fees: &str, slip: &str) -> TradeCheck {
        TradeCheck {
            planned: PlannedLoss {
                stop_loss_inr: stop.parse().unwrap(),
                fees_inr: fees.parse().unwrap(),
                slippage_inr: slip.parse().unwrap(),
            },
            notional_inr: dec!(300),
            min_notional_inr: dec!(100),
            stop_price: 99.0,
            liquidation_price: Some(90.0),
            long: true,
            isolated_margin: true,
        }
    }

    const P: EquitySource = EquitySource::Paper;
    const Z: Decimal = Decimal::ZERO;

    #[test]
    fn all_23_checkpoints_match_the_table() {
        let c = cfg();
        for (h, v) in TABLE.iter().enumerate() {
            assert_eq!(
                c.checkpoint_required(h as u32),
                Decimal::from(*v),
                "hour {h}"
            );
        }
        for h in 1..=22usize {
            assert_eq!(
                TABLE[h] - TABLE[h - 1],
                INCREASES[h - 1],
                "increase hour {h}"
            );
        }
    }

    #[test]
    fn hourly_multiplier_is_32_6418_percent() {
        let m = cfg().hourly_multiplier();
        assert!((m - 1.326418).abs() < 1e-6, "{m}");
        assert!(((m - 1.0) * 100.0 - 32.6418).abs() < 1e-4);
    }

    #[test]
    fn endpoints_are_exact() {
        let c = cfg();
        assert_eq!(c.target_equity_exact(0.0), dec!(100));
        assert_eq!(c.target_equity_exact(22.0), dec!(50000));
        assert_eq!(c.required_at(CAMPAIGN_START_MS), dec!(100));
        assert_eq!(c.required_at(CAMPAIGN_DEADLINE_MS), dec!(50000));
        assert_eq!(c.required_at(CAMPAIGN_START_MS - HOUR_MS), dec!(100));
        assert_eq!(c.required_at(CAMPAIGN_DEADLINE_MS + HOUR_MS), dec!(50000));
        assert_eq!(c.target_equity_exact(f64::NAN), dec!(100));
    }

    #[test]
    fn rounding_is_half_away_from_zero() {
        let c = cfg();
        // hour 1 exact 132.6418... -> 133 (checkpoint) and 132.64 (continuous)
        assert_eq!(c.checkpoint_required(1), dec!(133));
        assert_eq!(c.required_at(CAMPAIGN_START_MS + HOUR_MS), dec!(132.64));
        assert_eq!(r2(dec!(1.005)), dec!(1.01));
        assert_eq!(r0(dec!(232.5)), dec!(233));
        // hour 7 exact 722.3758 -> 722 (rounds down)
        assert_eq!(c.checkpoint_required(7), dec!(722));
    }

    #[test]
    fn partial_hour_targets() {
        let c = cfg();
        // h = 0.5: 100 * 500^(1/44) = 115.17
        assert_eq!(c.required_at(CAMPAIGN_START_MS + HOUR_MS / 2), dec!(115.17));
        let v = c.required_at(CAMPAIGN_START_MS + 21 * HOUR_MS + HOUR_MS / 2);
        assert!(v > dec!(37696) && v < dec!(50000), "{v}");
        let mut last = Decimal::ZERO;
        for m in 0..=(22 * 60) {
            let r = c.required_at(CAMPAIGN_START_MS + m * 60_000);
            assert!(r >= last);
            last = r;
        }
    }

    #[test]
    fn ist_handling() {
        assert_eq!(iso_ist(CAMPAIGN_START_MS), "2026-11-24T00:00:00+05:30");
        assert_eq!(iso_ist(CAMPAIGN_DEADLINE_MS), "2026-11-24T22:00:00+05:30");
        assert_eq!(
            iso_ist(cfg().checkpoint_ms(17)),
            "2026-11-24T17:00:00+05:30"
        );
        assert_eq!(
            iso_ist(CAMPAIGN_START_MS - 1000),
            "2026-11-23T23:59:59+05:30"
        );
        assert_eq!(CAMPAIGN_DEADLINE_MS - CAMPAIGN_START_MS, 22 * HOUR_MS);
        // 2026-11-23T18:30:00Z
        assert_eq!(CAMPAIGN_START_MS / 1000 % 86_400, 18 * 3600 + 1800);
    }

    #[test]
    fn deadline_expiry_and_no_entries_after() {
        let mut c = at("120", CAMPAIGN_START_MS + 10);
        assert!(c.entry_gate(CAMPAIGN_DEADLINE_MS - 1, 0).is_ok());
        let e = c.entry_gate(CAMPAIGN_DEADLINE_MS, 0).unwrap_err();
        assert!(e.contains("deadline"), "{e}");
        assert!(c.exit_only);
        assert_eq!(c.status(CAMPAIGN_DEADLINE_MS), CampaignStatus::Expired);
        assert_eq!(c.risk_budget_now(), Z);
        assert!(c.entry_gate(CAMPAIGN_DEADLINE_MS + HOUR_MS, 0).is_err());
    }

    #[test]
    fn no_entries_before_start() {
        let mut c = at("100", CAMPAIGN_START_MS - 5);
        let e = c.entry_gate(CAMPAIGN_START_MS - 1, 0).unwrap_err();
        assert!(e.contains("not started"));
    }

    #[test]
    fn paper_equity_is_labelled_and_unknown_equity_blocks() {
        let c = at("100", CAMPAIGN_START_MS);
        let j = c.to_json(CAMPAIGN_START_MS, Value::Null, "paper ledger, INR");
        assert_eq!(j["equity_source"], "paper");
        let mut u = Campaign::new(cfg(), EquitySource::LiveExchange);
        u.observe(CAMPAIGN_START_MS, None, EquitySource::LiveExchange, Z, Z);
        let j = u.to_json(CAMPAIGN_START_MS, Value::Null, "no verified balance");
        assert_eq!(j["equity_source"], "live_exchange");
        assert!(j["equity_inr"].is_null());
        let e = u.entry_gate(CAMPAIGN_START_MS + 1, 0).unwrap_err();
        assert!(e.contains("unknown"));
    }

    #[test]
    fn floor_stop_is_permanent() {
        let mut c = at("100", CAMPAIGN_START_MS);
        c.observe(CAMPAIGN_START_MS + 1000, Some(dec!(95)), P, dec!(-5), Z);
        assert!(c.is_stopped());
        assert_eq!(c.status(CAMPAIGN_START_MS + 2000), CampaignStatus::Stopped);
        c.observe(CAMPAIGN_START_MS + 3000, Some(dec!(99)), P, dec!(-1), Z);
        let e = c.entry_gate(CAMPAIGN_START_MS + 4000, 0).unwrap_err();
        assert!(e.contains("permanent"));
        assert!(c.drain_events().iter().any(|e| e.kind == "campaign_stop"));
    }

    #[test]
    fn open_loss_counts_toward_floor() {
        let mut c = at("100", CAMPAIGN_START_MS);
        c.observe(
            CAMPAIGN_START_MS + 1,
            Some(dec!(99)),
            P,
            dec!(-1),
            dec!(-4.2),
        );
        assert!(c.is_stopped(), "marked 94.8 is below the floor");
    }

    #[test]
    fn per_trade_cap_includes_fees_and_slippage() {
        let mut c = at("100", CAMPAIGN_START_MS);
        let now = CAMPAIGN_START_MS + 1;
        assert!(c.check_trade(now, &check("0.80", "0.15", "0.05")).is_ok()); // = 1.00
        let e = c
            .check_trade(now, &check("0.80", "0.15", "0.06"))
            .unwrap_err();
        assert!(e.contains("exceeds campaign budget"), "{e}");
    }

    #[test]
    fn campaign_loss_cap_limits_budget() {
        let mut c = at("100", CAMPAIGN_START_MS);
        c.observe(CAMPAIGN_START_MS + 1, Some(dec!(95.60)), P, dec!(-4.4), Z);
        assert!(!c.is_stopped());
        assert_eq!(c.risk_budget_now(), dec!(0.60));
        let t = CAMPAIGN_START_MS + 2;
        assert!(c.check_trade(t, &check("0.5", "0.05", "0.05")).is_ok());
        assert!(c.check_trade(t, &check("0.5", "0.1", "0.05")).is_err());
        c.observe(CAMPAIGN_START_MS + 3, Some(dec!(95.01)), P, dec!(-4.99), Z);
        assert_eq!(c.risk_budget_now(), dec!(0.01));
    }

    #[test]
    fn min_notional_is_rejected_not_raised() {
        let mut c = at("100", CAMPAIGN_START_MS);
        let mut t = check("0.5", "0.1", "0.1");
        t.notional_inr = dec!(50);
        t.min_notional_inr = dec!(500);
        let e = c.check_trade(CAMPAIGN_START_MS + 1, &t).unwrap_err();
        assert!(e.contains("never raised"), "{e}");
    }

    #[test]
    fn stop_beyond_liquidation_rejected() {
        let mut c = at("100", CAMPAIGN_START_MS);
        let now = CAMPAIGN_START_MS + 1;
        let mut t = check("0.5", "0.1", "0.1");
        t.liquidation_price = Some(99.5); // long: stop 99 is beyond liq 99.5
        assert!(c.check_trade(now, &t).unwrap_err().contains("liquidation"));
        t.liquidation_price = None;
        assert!(c.check_trade(now, &t).is_err());
        t.liquidation_price = Some(90.0);
        t.isolated_margin = false;
        assert!(c.check_trade(now, &t).unwrap_err().contains("isolated"));
    }

    #[test]
    fn no_size_increase_after_a_loss() {
        let mut c = at("100", CAMPAIGN_START_MS);
        let now = CAMPAIGN_START_MS + 1;
        assert!(c.check_trade(now, &check("0.4", "0.05", "0.05")).is_ok()); // 0.50
        c.on_trade_closed(now + 1, dec!(-0.5));
        c.observe(now + 2, Some(dec!(99.5)), P, dec!(-0.5), Z);
        let e = c
            .check_trade(now + 3, &check("0.6", "0.05", "0.05"))
            .unwrap_err();
        assert!(e.contains("no size increase"), "{e}");
        assert!(c
            .check_trade(now + 3, &check("0.4", "0.05", "0.05"))
            .is_ok());
    }

    #[test]
    fn behind_schedule_never_changes_risk_or_forces_trades() {
        let now = CAMPAIGN_START_MS + 10 * HOUR_MS;
        let mut behind = at("100", now); // required 1,685.79
        assert_eq!(behind.status(now), CampaignStatus::Behind);
        let mut ahead = at("100", now);
        ahead.cfg.start_ms = now; // requirement 100 -> on track
        assert_eq!(behind.risk_budget_now(), dec!(1));
        assert_eq!(behind.risk_budget_now(), ahead.risk_budget_now());
        // the gate permits evaluation, never requires a trade
        assert!(behind.entry_gate(now, 1).is_err());
        assert!(behind.check_trade(now, &check("1.5", "0", "0")).is_err());
        let rate = behind.required_hourly_rate(now).unwrap();
        assert!(rate > 0.326, "reported catch-up rate {rate}");
        let j = behind.to_json(now, Value::Null, "paper");
        assert_eq!(j["status"], "behind");
        assert_eq!(j["per_trade_max_loss_inr"], "1");
        assert_eq!(j["risk_budget_now_inr"], "1.00");
    }

    #[test]
    fn one_position_at_a_time() {
        let mut c = at("100", CAMPAIGN_START_MS);
        assert!(c.entry_gate(CAMPAIGN_START_MS + 1, 0).is_ok());
        let e = c.entry_gate(CAMPAIGN_START_MS + 1, 1).unwrap_err();
        assert!(e.contains("open position"));
    }

    #[test]
    fn exit_only_deadline_workflow_never_claims_flat() {
        let mut c = at("101", CAMPAIGN_START_MS);
        let t = CAMPAIGN_DEADLINE_MS;
        c.tick(t);
        assert!(c.exit_only);
        c.record_step(t, DeadlineStep::StopEntries, true, "entries closed".into());
        c.record_step(
            t,
            DeadlineStep::Flatten,
            false,
            "live close path not verified".into(),
        );
        c.finalize(t, false, "live close path not verified: owner alerted");
        let f = c.final_record.clone().unwrap();
        assert!(!f.verified_flat);
        assert_eq!(f.result, "target not reached");
        c.finalize(t + 1, true, "ignored");
        assert!(
            !c.final_record.unwrap().verified_flat,
            "final record is written once"
        );
    }

    #[test]
    fn restart_restores_state_including_stop() {
        let mut c = at("100", CAMPAIGN_START_MS);
        c.observe(CAMPAIGN_START_MS + 1, Some(dec!(94)), P, dec!(-6), Z);
        let s = serde_json::to_string(&c).unwrap();
        let r = Campaign::restore(&s, &cfg()).unwrap();
        assert!(r.is_stopped());
        assert_eq!(r.checkpoint_actual[0], Some(dec!(100)));
        let mut other = cfg();
        other.max_loss_per_trade = dec!(10);
        assert!(
            Campaign::restore(&s, &other).is_err(),
            "changed limits need owner review"
        );
        assert!(Campaign::restore("{not json", &cfg()).is_err());
    }

    #[test]
    fn checkpoints_and_history_record_actuals() {
        let mut c = at("100", CAMPAIGN_START_MS);
        let t = CAMPAIGN_START_MS + HOUR_MS + 5;
        c.observe(t, Some(dec!(101)), P, dec!(1), Z);
        assert_eq!(c.checkpoint_actual[1], Some(dec!(101)));
        assert_eq!(c.checkpoints_done(t), 1);
        assert_eq!(c.checkpoints_done(CAMPAIGN_DEADLINE_MS + 1), 22);
        let j = c.to_json(t, Value::Null, "paper");
        assert_eq!(j["milestones"].as_array().unwrap().len(), 23);
        assert_eq!(j["milestones"][1]["actual_inr"], "101.00");
        assert_eq!(j["prev_milestone"]["hour"], 1);
        assert_eq!(j["next_milestone"]["required_inr"], "176");
        assert_eq!(j["equity_history"].as_array().unwrap().len(), 2);
        assert_eq!(j["drawdown_inr"], "0.00");
    }

    #[test]
    fn every_decision_is_audited() {
        let mut c = at("100", CAMPAIGN_START_MS);
        let _ = c.entry_gate(CAMPAIGN_START_MS - 1, 0);
        let _ = c.entry_gate(CAMPAIGN_START_MS - 1, 0); // repeated block: logged once
        let _ = c.entry_gate(CAMPAIGN_START_MS + 1, 0);
        let _ = c.check_trade(CAMPAIGN_START_MS + 2, &check("2", "0", "0"));
        c.on_trade_closed(CAMPAIGN_START_MS + 3, dec!(0.2));
        let ev = c.drain_events();
        let kinds: Vec<&str> = ev.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds.iter().filter(|k| **k == "entry_gate").count(), 2);
        assert!(kinds.contains(&"trade_check"));
        assert!(kinds.contains(&"trade_closed"));
        assert!(kinds.contains(&"checkpoint"));
    }

    #[test]
    fn bold_profile_keeps_floor_liquidation_and_one_position() {
        let mut c = Campaign::new(CampaignConfig::bold(dec!(60)), P);
        c.observe(CAMPAIGN_START_MS, Some(dec!(100)), P, Z, Z);
        assert_eq!(c.risk_budget_now(), dec!(40));
        let now = CAMPAIGN_START_MS + 1;
        assert!(c.check_trade(now, &check("30", "1", "1")).is_ok());
        assert!(
            c.check_trade(now, &check("41", "0", "0")).is_err(),
            "never beyond the floor"
        );
        let mut t = check("10", "0", "0");
        t.liquidation_price = Some(99.5);
        assert!(
            c.check_trade(now, &t).is_err(),
            "SL stays inside liquidation"
        );
        assert!(c.entry_gate(now, 1).is_err(), "one position");
        c.observe(now + 1, Some(dec!(60)), P, dec!(-40), Z);
        assert!(c.is_stopped(), "owner floor is a permanent stop");
        // stake: need 133 - 100 = 33 with 10x and 1% net TP -> 330, capped at 40 above floor
        let cfg = CampaignConfig::bold(dec!(60));
        assert!((cfg.bold_stake(100.0, 133.0, 10.0, 0.01) - 40.0).abs() < 1e-9);
        assert!((cfg.bold_stake(100.0, 101.0, 10.0, 0.01) - 10.0).abs() < 1e-9);
        assert_eq!(cfg.bold_stake(100.0, 133.0, 10.0, -0.01), 0.0);
    }

    #[test]
    fn completion_is_exit_only() {
        let mut c = at("100", CAMPAIGN_START_MS);
        c.observe(CAMPAIGN_START_MS + 5, Some(dec!(50000)), P, dec!(49900), Z);
        assert_eq!(c.status(CAMPAIGN_START_MS + 6), CampaignStatus::Completed);
        assert!(c.entry_gate(CAMPAIGN_START_MS + 6, 0).is_err());
    }
}
