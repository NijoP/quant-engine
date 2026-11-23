//! Report generator: 8 PM paper report, hourly reports while trading toward the
//! single INR 50,000 target (17:00 IST 24 Nov) and the final report. The Stage 1
//! checkpoint report kind is legacy: the 22:30 checkpoint was removed.
//!
//! Every report keeps these sections apart and in this order: actual measured P&L,
//! target-implied requirements, hypothetical scenarios, forecasts, uncalibrated
//! probabilities, learning results, risk-limit events, execution quality. Paper
//! results are labelled PAPER and never presented as a CoinDCX balance.

use crate::capital::{CapitalController, Stage};
use crate::frequency::{Requirement, SimSummary};
use serde::Serialize;
use serde_json::Value;
use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ReportKind {
    Paper8pm,
    Stage1Checkpoint,
    Stage2Hourly,
    Final,
}

impl ReportKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "paper" | "paper-8pm" => Self::Paper8pm,
            "stage1" | "stage1-checkpoint" => Self::Stage1Checkpoint,
            "stage2" | "stage2-hourly" => Self::Stage2Hourly,
            "final" => Self::Final,
            _ => return None,
        })
    }
    pub fn title(self) -> &'static str {
        match self {
            Self::Paper8pm => "Stage 0 paper-trading report (20:00 IST)",
            Self::Stage1Checkpoint => {
                "Stage 1 checkpoint and withdrawal reconciliation report (22:30 IST)"
            }
            Self::Stage2Hourly => "Stage 2 hourly report",
            Self::Final => "Final trading and learning report (17:00 IST 24 Nov)",
        }
    }
    pub fn slug(self) -> &'static str {
        match self {
            Self::Paper8pm => "paper-8pm",
            Self::Stage1Checkpoint => "stage1-checkpoint",
            Self::Stage2Hourly => "stage2-hourly",
            Self::Final => "final",
        }
    }
}

/// One closed trade as the report sees it (INR, net after fees).
#[derive(Debug, Clone, Serialize)]
pub struct TradeLine {
    pub id: String,
    pub pair: String,
    pub side: String,
    pub net_pnl_inr: f64,
    pub fees_inr: f64,
    pub held_s: f64,
    pub exit_reason: String,
    pub paper: bool,
}

pub struct ReportInput<'a> {
    pub kind: ReportKind,
    pub generated_ms: u64,
    pub capital: &'a CapitalController,
    pub trades: &'a [TradeLine],
    pub requirement: Requirement,
    pub simulation: Option<&'a SimSummary>,
    /// Forecast accuracy summary (per horizon), as produced by the desk.
    pub forecasts: Value,
    /// Calibration authority status; uncalibrated models are listed here.
    pub calibration: Value,
    /// Learning-engine output (mistake classes, lessons, champion/challenger).
    pub learning: Value,
    pub risk_events: Vec<String>,
    /// Execution quality: latency, partial fills, rejections.
    pub execution: Value,
    /// Scanner coverage: discovered / eligible / subscribed per universe.
    pub coverage: Value,
}

fn ist(ms: u64) -> String {
    let s = ms / 1000 + 19_800; // +05:30
    let (d, r) = (s / 86_400, s % 86_400);
    // civil-from-days (Howard Hinnant)
    let z = d as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{day:02} {:02}:{:02}:{:02} IST",
        r / 3600,
        r % 3600 / 60,
        r % 60
    )
}

fn pretty(v: &Value) -> String {
    if v.is_null() {
        "not available".into()
    } else {
        format!(
            "```json\n{}\n```",
            serde_json::to_string_pretty(v).unwrap_or_default()
        )
    }
}

pub fn render(i: &ReportInput) -> String {
    let c = i.capital;
    let label = if c.paper { "PAPER" } else { "LIVE" };
    let mut o = String::new();
    let _ = writeln!(o, "# {} — {label}\n", i.kind.title());
    let _ = writeln!(
        o,
        "Generated {}. Stage: {:?}.",
        ist(i.generated_ms),
        c.stage
    );
    if c.paper {
        let _ = writeln!(o, "\n> All equity and P&L below are a PAPER ledger (simulated orders on live public CoinDCX data). They are not a CoinDCX balance and must not be represented as real equity.");
    }

    // 1. actual measured
    let wins = i.trades.iter().filter(|t| t.net_pnl_inr > 0.0).count();
    let losses = i.trades.iter().filter(|t| t.net_pnl_inr < 0.0).count();
    let net: f64 = i.trades.iter().map(|t| t.net_pnl_inr).sum();
    let fees: f64 = i.trades.iter().map(|t| t.fees_inr).sum();
    let _ = writeln!(o, "\n## 1. Actual measured results ({label})\n");
    let _ = writeln!(o, "| Metric | Value |\n|---|---|");
    let _ = writeln!(o, "| Ledger equity | INR {} |", c.equity.round_dp(4));
    let _ = writeln!(
        o,
        "| Stage start equity | INR {} |",
        c.stage_start_equity.round_dp(4)
    );
    let _ = writeln!(
        o,
        "| Stage realised net P&L | INR {} |",
        c.stage_realised_pnl.round_dp(4)
    );
    let _ = writeln!(o, "| Stage fees | INR {} |", c.stage_fees.round_dp(4));
    let _ = writeln!(
        o,
        "| High-water mark | INR {} |",
        c.high_water_mark.round_dp(4)
    );
    let _ = writeln!(
        o,
        "| Closed trades in report window | {} ({wins} won, {losses} lost) |",
        i.trades.len()
    );
    let _ = writeln!(
        o,
        "| Net P&L of those trades | INR {net:.4} (fees INR {fees:.4}) |"
    );
    if !i.trades.is_empty() {
        let _ = writeln!(o, "\n| Trade | Pair | Side | Net INR | Fees INR | Held s | Exit |\n|---|---|---|---|---|---|---|");
        for t in i.trades.iter().take(200) {
            let _ = writeln!(
                o,
                "| {} | {} | {} | {:.4} | {:.4} | {:.1} | {} |",
                t.id, t.pair, t.side, t.net_pnl_inr, t.fees_inr, t.held_s, t.exit_reason
            );
        }
    } else {
        let _ = writeln!(o, "\nNo trades closed. The governor may reject every proposal; that is a valid outcome, not a fault.");
    }
    if let Some(cp) = &c.checkpoint {
        let _ = writeln!(
            o,
            "\n### Checkpoint workflow ({:?})\n\n| Step | Status | Note |\n|---|---|---|",
            cp.kind
        );
        for (s, st, n) in &cp.steps {
            let _ = writeln!(o, "| {s:?} | {st:?} | {n} |");
        }
        if let Some(r) = &cp.reconciliation {
            let _ = writeln!(
                o,
                "\nReconciliation: ledger INR {} vs queried INR {} (difference INR {}; {}).",
                r.ledger_equity_inr,
                r.queried_balance_inr,
                r.difference_inr,
                if r.ok {
                    "matched"
                } else {
                    "MISMATCH: reconciliation required"
                }
            );
        }
        if let Some(w) = &cp.withdrawal {
            let _ = writeln!(
                o,
                "\nWithdrawal prepared (not executed): INR {}; expected retained INR {}. {}",
                w.amount_inr, w.expected_retained_inr, w.note
            );
        }
    }
    for w in &c.withdrawals {
        let _ = writeln!(
            o,
            "\nUser-confirmed withdrawal recorded {}: INR {}; verified retained balance INR {}.",
            ist(w.ts_ms),
            w.amount_inr,
            w.retained_balance_inr
        );
    }
    if matches!(i.kind, ReportKind::Stage1Checkpoint | ReportKind::Final) {
        let (target, _) = c.target();
        if c.equity < target {
            let _ = writeln!(o, "\n**Target missed:** equity INR {} < target INR {target}. Causes are in sections 6 and 7 (governor rejections, uncalibrated forecasts, minimum-size incompatibility, losses). Risk limits were not raised in response.", c.equity.round_dp(4));
        }
    }

    // 2. target-implied
    let r = &i.requirement;
    let _ = writeln!(
        o,
        "\n## 2. Target-implied requirements (mathematics, not forecasts)\n"
    );
    let _ = writeln!(
        o,
        "From equity INR {:.4} to INR {:.0} in {:.2} h: multiple x{:.4}; ideal hourly return {}.",
        r.equity,
        r.target,
        r.hours,
        r.multiple,
        r.ideal_hourly_return
            .map_or("n/a".into(), |x| format!("{:.4}%", x * 100.0))
    );
    let _ = writeln!(
        o,
        "\n| Trades/h | n | Required return per trade (all won) |\n|---|---|---|"
    );
    for row in &r.rows {
        let _ = writeln!(
            o,
            "| {} | {} | {:.4}% |",
            row.trades_per_hour,
            row.n_trades,
            row.required_return_per_trade * 100.0
        );
    }
    let _ = writeln!(o, "\nA trade count is a reporting metric, never a quota.");

    // 3. hypothetical
    let _ = writeln!(o, "\n## 3. Hypothetical scenarios (assumed inputs)\n");
    match i.simulation {
        Some(s) => {
            let _ = writeln!(o, "{}. {} windows, p_win {:.3}, win {:.4}, loss {:.4}, cost {:.4}, no-opportunity {:.2}, reject {:.2}, partial {:.2}, downtime {:.2}, {} paths, seed {}.", s.label, s.windows, s.params.p_win, s.params.win_ret, s.params.loss_ret, s.params.cost_ret, s.params.p_no_opportunity, s.params.p_reject, s.params.p_partial, s.params.p_downtime, s.params.paths, s.params.seed);
            let _ = writeln!(o, "\nP(reach target) {:.4}; P(session-loss stop) {:.4}; final equity p05 / median / p95: {:.2} / {:.2} / {:.2}; mean trades {:.1}.", s.p_reach_target, s.p_session_stop, s.p05_final, s.median_final, s.p95_final, s.mean_trades);
        }
        None => {
            let _ = writeln!(o, "not computed");
        }
    }

    let _ = writeln!(
        o,
        "\n## 4. Forecasts (prediction vs actual)\n\n{}",
        pretty(&i.forecasts)
    );
    let _ = writeln!(o, "\n## 5. Uncalibrated probabilities\n\nProbabilities without calibration authority are model priors, not measured frequencies.\n\n{}", pretty(&i.calibration));
    let _ = writeln!(o, "\n## 6. Learning results\n\n{}", pretty(&i.learning));
    let _ = writeln!(o, "\n## 7. Risk-limit events\n");
    if i.risk_events.is_empty() {
        let _ = writeln!(o, "none recorded");
    }
    for e in i.risk_events.iter().take(100) {
        let _ = writeln!(o, "- {e}");
    }
    let _ = writeln!(
        o,
        "\n## 8. Execution quality and coverage\n\n{}\n\n{}",
        pretty(&i.execution),
        pretty(&i.coverage)
    );
    if c.stage == Stage::Complete || i.kind == ReportKind::Final {
        let _ = writeln!(o, "\nNo position is carried past the final checkpoint.");
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capital::*;
    use rust_decimal_macros::dec;

    #[test]
    fn sections_in_order_and_paper_labelled() {
        let c = CapitalController::new(CapitalPlan::default(), dec!(100), true, STAGE0_END_MS - 1);
        let req = c.required(STAGE0_END_MS - 1);
        let r = render(&ReportInput {
            kind: ReportKind::Paper8pm,
            generated_ms: STAGE0_END_MS,
            capital: &c,
            trades: &[],
            requirement: req,
            simulation: None,
            forecasts: Value::Null,
            calibration: serde_json::json!({"desk.fp.v1": "uncalibrated"}),
            learning: Value::Null,
            risk_events: vec!["session loss limit".into()],
            execution: Value::Null,
            coverage: Value::Null,
        });
        assert!(r.contains("2026-11-23 20:00:00 IST"));
        assert!(r.contains("PAPER ledger"));
        let idx: Vec<usize> = [
            "## 1.", "## 2.", "## 3.", "## 4.", "## 5.", "## 6.", "## 7.", "## 8.",
        ]
        .iter()
        .map(|s| r.find(s).unwrap())
        .collect();
        assert!(idx.windows(2).all(|w| w[0] < w[1]));
    }
}
