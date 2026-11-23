//! §6.2 Post-trade outcome records, appended separately from the decision.
//!
//! The caller supplies raw fills and observations ([`TradeOutcome`]); the
//! engine derives weighted averages, P&L, costs and slippage in `Decimal`
//! ([`OutcomeRecord`]). The pre-trade snapshot is only read, never modified.
//!
//! Conventions: slippage is measured against the decision mid (`ref_price`)
//! on entry and against `exit_ref_price` (the SL/TP trigger, or the mid at a
//! time exit) on exit, positive = adverse. It therefore includes the
//! half-spread crossed, matching [`crate::snapshot::DecisionSnapshot::est_cost_bps`].
use crate::snapshot::DecisionSnapshot;
use crate::types::{finite, to_f64, LearnError, Result, HORIZONS_S};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fill {
    pub ts_ms: i64,
    pub price: Decimal,
    pub qty: Decimal,
    /// fee paid on this fill, quote currency (>= 0)
    pub fee: Decimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    TakeProfit,
    StopLoss,
    /// the 120 s deadline closed the position
    TimeExit,
    KillSwitch,
    Manual,
    ExchangeReject,
    Error,
}

/// Actual mid move from the decision `ref_price` at decision time + horizon.
/// `None` = not observed (feed gap); never filled in with a guess.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HorizonMove {
    pub horizon_s: u32,
    pub actual_return: Option<f64>,
}

/// Facts observed during the trade that the classifier uses (all optional).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutcomeFlags {
    /// a feed went stale while the position was open
    pub stale_data: bool,
    /// operator- or monitor-tagged event outside available information
    pub external_event: bool,
    pub exchange_error: Option<String>,
    pub execution_error: Option<String>,
    pub spread_entry_bps: Option<f64>,
    pub spread_exit_bps: Option<f64>,
    /// regime label observed after the trade, if the detector changed it
    pub regime_after: Option<String>,
    /// realised volatility over the holding window, fraction per trade horizon
    pub realised_vol: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeOutcome {
    pub trade_id: String,
    pub entry_fills: Vec<Fill>,
    pub exit_fills: Vec<Fill>,
    pub exit_ref_price: Decimal,
    /// funding paid (positive) or received (negative), quote currency
    pub funding: Decimal,
    pub exit_reason: ExitReason,
    /// max favourable / adverse excursion while open, fraction of avg entry (>= 0)
    pub mfe: f64,
    pub mae: f64,
    pub horizon_moves: Vec<HorizonMove>,
    pub flags: OutcomeFlags,
    /// when this record is appended; must not precede any observation it holds
    pub recorded_ms: i64,
}

impl TradeOutcome {
    pub fn move_at(&self, h: u32) -> Option<f64> {
        self.horizon_moves
            .iter()
            .find(|m| m.horizon_s == h)
            .and_then(|m| m.actual_return)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRecord {
    pub seq: u64,
    pub sha256: String,
    pub outcome: TradeOutcome,
    pub entry_qty: Decimal,
    pub exit_qty: Decimal,
    pub avg_entry: Decimal,
    pub avg_exit: Decimal,
    pub fees: Decimal,
    pub gross_pnl: Decimal,
    /// gross - fees - funding
    pub net_pnl: Decimal,
    pub entry_notional: Decimal,
    /// first entry fill to last exit fill
    pub holding_ms: i64,
    pub partial_fill: bool,
    pub entry_slippage_bps: f64,
    pub exit_slippage_bps: f64,
    /// fees + funding + entry and exit slippage, bps of entry notional
    pub actual_cost_bps: f64,
    pub est_cost_bps: f64,
    /// net P&L / entry notional
    pub net_return: f64,
    pub won: bool,
}

fn wavg(fills: &[Fill]) -> (Decimal, Decimal, Decimal) {
    let q: Decimal = fills.iter().map(|f| f.qty).sum();
    let pq: Decimal = fills.iter().map(|f| f.price * f.qty).sum();
    let fee: Decimal = fills.iter().map(|f| f.fee).sum();
    (q, if q.is_zero() { Decimal::ZERO } else { pq / q }, fee)
}

fn bps(num: Decimal, den: Decimal) -> f64 {
    if den.is_zero() {
        0.0
    } else {
        to_f64(num / den) * 1e4
    }
}

/// Validate a raw outcome against its decision and derive the accounting.
pub fn derive(snap: &DecisionSnapshot, o: TradeOutcome) -> Result<OutcomeRecord> {
    if o.trade_id != snap.trade_id {
        return Err(LearnError::Invalid("trade id mismatch".into()));
    }
    if !snap.risk.approved {
        return Err(LearnError::Invalid(
            "risk governor rejected this decision: no outcome can exist".into(),
        ));
    }
    if o.entry_fills.is_empty() || o.exit_fills.is_empty() {
        return Err(LearnError::Invalid("entry and exit fills required".into()));
    }
    for f in o.entry_fills.iter().chain(&o.exit_fills) {
        if f.qty <= Decimal::ZERO || f.price <= Decimal::ZERO || f.fee < Decimal::ZERO {
            return Err(LearnError::Invalid(
                "fill needs qty, price > 0 and fee >= 0".into(),
            ));
        }
    }
    let first_entry = o.entry_fills.iter().map(|f| f.ts_ms).min().unwrap_or(0);
    let last_entry = o.entry_fills.iter().map(|f| f.ts_ms).max().unwrap_or(0);
    let first_exit = o.exit_fills.iter().map(|f| f.ts_ms).min().unwrap_or(0);
    let last_exit = o.exit_fills.iter().map(|f| f.ts_ms).max().unwrap_or(0);
    // no leakage: nothing can be filled before the decision existed
    if first_entry < snap.ts_ms {
        return Err(LearnError::Invalid(
            "entry fill precedes the decision".into(),
        ));
    }
    if first_exit < last_entry {
        return Err(LearnError::Invalid(
            "exit fill precedes the last entry fill".into(),
        ));
    }
    if o.recorded_ms < last_exit {
        return Err(LearnError::Invalid(
            "recorded before the last exit fill".into(),
        ));
    }
    finite("mfe", o.mfe)?;
    finite("mae", o.mae)?;
    if o.mfe < 0.0 || o.mae < 0.0 {
        return Err(LearnError::Invalid(
            "mfe and mae are magnitudes (>= 0)".into(),
        ));
    }
    let mut last_h = 0;
    for m in &o.horizon_moves {
        if !HORIZONS_S.contains(&m.horizon_s) || m.horizon_s <= last_h {
            return Err(LearnError::Invalid(
                "horizon moves not ascending in HORIZONS_S".into(),
            ));
        }
        last_h = m.horizon_s;
        if let Some(r) = m.actual_return {
            finite("actual_return", r)?;
            // an observation from the future of the record would be leakage
            if snap.ts_ms + i64::from(m.horizon_s) * 1000 > o.recorded_ms {
                return Err(LearnError::Invalid(format!(
                    "{}s move observed after recorded_ms",
                    m.horizon_s
                )));
            }
        }
    }
    for x in [
        o.flags.spread_entry_bps,
        o.flags.spread_exit_bps,
        o.flags.realised_vol,
    ]
    .into_iter()
    .flatten()
    {
        finite("flag", x)?;
    }
    let (eq, avg_entry, fe) = wavg(&o.entry_fills);
    let (xq, avg_exit, fx) = wavg(&o.exit_fills);
    if eq != xq {
        return Err(LearnError::ReconciliationRequired(format!(
            "entry qty {eq} != exit qty {xq}"
        )));
    }
    let s = snap.direction.sign_dec();
    let fees = fe + fx;
    let gross = s * (avg_exit - avg_entry) * eq;
    let net = gross - fees - o.funding;
    let notional = avg_entry * eq;
    let entry_slip = bps(s * (avg_entry - snap.ref_price), snap.ref_price);
    let exit_slip = bps(s * (o.exit_ref_price - avg_exit), o.exit_ref_price);
    let actual_cost = bps(fees + o.funding, notional) + entry_slip + exit_slip;
    Ok(OutcomeRecord {
        seq: 0,
        sha256: String::new(),
        entry_qty: eq,
        exit_qty: xq,
        avg_entry,
        avg_exit,
        fees,
        gross_pnl: gross,
        net_pnl: net,
        entry_notional: notional,
        holding_ms: last_exit - first_entry,
        partial_fill: eq < snap.qty,
        entry_slippage_bps: entry_slip,
        exit_slippage_bps: exit_slip,
        actual_cost_bps: actual_cost,
        est_cost_bps: snap.est_cost_bps(),
        net_return: to_f64(if notional.is_zero() {
            Decimal::ZERO
        } else {
            net / notional
        }),
        won: net > Decimal::ZERO,
        outcome: o,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::snapshot::tests::snap;
    use crate::types::dec;

    /// helper: long 1 @ 100.01 (fee 0.05), exit 1 @ `exit_px` (fee 0.05) at
    /// +`hold_s`; every horizon move set to `mv`.
    pub fn outcome(id: &str, ts: i64, exit_px: &str, reason: ExitReason, mv: f64) -> TradeOutcome {
        TradeOutcome {
            trade_id: id.into(),
            entry_fills: vec![Fill {
                ts_ms: ts + 50,
                price: dec("100.01"),
                qty: dec("1"),
                fee: dec("0.05"),
            }],
            exit_fills: vec![Fill {
                ts_ms: ts + 30_000,
                price: exit_px.parse().unwrap(),
                qty: dec("1"),
                fee: dec("0.05"),
            }],
            exit_ref_price: exit_px.parse().unwrap(),
            funding: Decimal::ZERO,
            exit_reason: reason,
            mfe: 0.002,
            mae: 0.003,
            horizon_moves: HORIZONS_S
                .iter()
                .map(|&h| HorizonMove {
                    horizon_s: h,
                    actual_return: Some(mv),
                })
                .collect(),
            flags: OutcomeFlags::default(),
            recorded_ms: ts + 121_000,
        }
    }

    #[test]
    fn derives_weighted_averages_pnl_and_costs_in_decimal() {
        let s = snap("a", 1_000);
        let mut o = outcome("a", 1_000, "100.6", ExitReason::TakeProfit, 0.006);
        o.entry_fills = vec![
            Fill {
                ts_ms: 1_100,
                price: dec("100"),
                qty: dec("0.5"),
                fee: dec("0.025"),
            },
            Fill {
                ts_ms: 1_200,
                price: dec("100.02"),
                qty: dec("0.5"),
                fee: dec("0.025"),
            },
        ];
        let r = derive(&s, o).unwrap();
        assert_eq!(r.avg_entry, dec("100.01"));
        assert_eq!(r.fees, dec("0.1"));
        assert_eq!(r.gross_pnl, dec("0.59"));
        assert_eq!(r.net_pnl, dec("0.49"));
        assert!(r.won && !r.partial_fill);
        assert!((r.entry_slippage_bps - 1.0).abs() < 1e-9);
        assert!((r.actual_cost_bps - (10.0 / 1.0001 + 1.0)).abs() < 1e-3);
        assert_eq!(r.holding_ms, 31_000 - 1_100);
    }

    #[test]
    fn short_pnl_sign_and_funding() {
        let mut s = snap("a", 1_000);
        s.direction = crate::types::Direction::Short;
        s.stop_price = dec("100.5");
        s.target_price = dec("99.4");
        let mut o = outcome("a", 1_000, "99.4", ExitReason::TakeProfit, -0.006);
        o.funding = dec("0.01");
        let r = derive(&s, o).unwrap();
        assert_eq!(r.gross_pnl, dec("0.61"));
        assert_eq!(r.net_pnl, dec("0.50"));
    }

    #[test]
    fn rejects_leakage_and_unreconciled_quantities() {
        let s = snap("a", 1_000);
        let mut o = outcome("a", 1_000, "100.6", ExitReason::TakeProfit, 0.0);
        o.entry_fills[0].ts_ms = 900;
        assert!(derive(&s, o).is_err());
        let mut o = outcome("a", 1_000, "100.6", ExitReason::TakeProfit, 0.0);
        o.recorded_ms = 1_000 + 60_000; // 90 s / 120 s moves not yet observable
        assert!(derive(&s, o).is_err());
        let mut o = outcome("a", 1_000, "100.6", ExitReason::TakeProfit, 0.0);
        o.exit_fills[0].qty = dec("0.4");
        assert!(matches!(
            derive(&s, o),
            Err(LearnError::ReconciliationRequired(_))
        ));
        let mut rejected = snap("a", 1_000);
        rejected.risk.approved = false;
        let o = outcome("a", 1_000, "100.6", ExitReason::TakeProfit, 0.0);
        assert!(derive(&rejected, o).is_err());
    }

    #[test]
    fn partial_fill_flagged() {
        let s = snap("a", 1_000);
        let mut o = outcome("a", 1_000, "100.6", ExitReason::TakeProfit, 0.0);
        o.entry_fills[0].qty = dec("0.6");
        o.exit_fills[0].qty = dec("0.6");
        assert!(derive(&s, o).unwrap().partial_fill);
    }
}
