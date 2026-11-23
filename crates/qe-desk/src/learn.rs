//! s3-learn integration: an immutable decision snapshot is journaled BEFORE every
//! paper entry (a journal failure blocks the entry), and the outcome is appended
//! once the position is closed and every forecast horizon (<= 120 s) has elapsed.
//! The learning signal is shrink-only and never reaches the risk governor's limits.
use crate::desk::{dec, Desk};
use qe_learn::{
    DataQuality, DecisionSnapshot, Direction, EngineConfig, ExitReason, Fill, HorizonForecast,
    HorizonMove, LearningEngine, Mode, OutcomeFlags, RiskDecision, TradeOutcome, HORIZONS_S,
};
use qe_math::prob;
use qe_risk::Side;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub struct Pending {
    pub trade_id: String,
    pub pair: String,
    pub side: Side,
    pub opened_ms: i64,
    pub closed_ms: i64,
    pub avg_entry: f64,
    pub exit_ref: f64,
    pub exit_reason: String,
    pub entry_fills: Vec<Fill>,
    pub exit_fills: Vec<Fill>,
}

#[derive(Default)]
pub struct LearnCtx {
    pub engine: Option<LearningEngine>,
    pub error: Option<String>,
    pub pending: Vec<Pending>,
    pub snapshots: u64,
    pub outcomes: u64,
}

impl LearnCtx {
    pub fn open(dir: &std::path::Path) -> Self {
        match LearningEngine::open(EngineConfig::default(), &dir.join("learn.jsonl")) {
            Ok(e) => Self {
                engine: Some(e),
                ..Default::default()
            },
            Err(e) => Self {
                error: Some(format!("learning journal not opened: {e:?}")),
                ..Default::default()
            },
        }
    }
}

/// Inputs available at decision time (all measured before the order).
pub struct DecisionCtx<'a> {
    pub trace: &'a str,
    pub pair: &'a str,
    pub ts_ms: i64,
    pub side: Side,
    pub mu: f64,
    pub sigma_1s: f64,
    pub obi: f64,
    pub spread_bps: f64,
    pub book_age_ms: i64,
    pub data_ok: bool,
    pub regimes: &'a [String],
    pub p_tp: f64,
    pub p_sl: f64,
    pub tp_frac: f64,
    pub sl_frac: f64,
    pub fee: f64,
    pub slip: f64,
    pub mid: f64,
    pub calibrated: bool,
    pub order: &'a qe_risk::ApprovedOrder,
    pub model_version: &'a str,
    /// Gamble-mode bet (mode=gamble in the snapshot).
    pub gamble: bool,
}

pub fn snapshot(c: &DecisionCtx) -> DecisionSnapshot {
    let forecasts = HORIZONS_S
        .iter()
        .map(|&h| {
            let hs = h as f64;
            let sd = (c.sigma_1s * hs.sqrt()).max(1e-12);
            let er = c.mu * hs;
            HorizonForecast {
                horizon_s: h,
                expected_return: er,
                return_sd: sd,
                p_up: prob::norm_cdf(er / sd).clamp(0.0, 1.0),
                ci_low: er - 1.645 * sd,
                ci_high: er + 1.645 * sd,
            }
        })
        .collect();
    let s120 = c.sigma_1s * 120f64.sqrt();
    let mut features = BTreeMap::new();
    features.insert("mu_per_s".into(), c.mu);
    features.insert("sigma_1s".into(), c.sigma_1s);
    let mut book = BTreeMap::new();
    book.insert("obi_10".into(), c.obi);
    book.insert("spread_bps".into(), c.spread_bps);
    let o = c.order;
    DecisionSnapshot {
        trade_id: c.trace.into(),
        instrument: c.pair.into(),
        market: "inr_futures".into(),
        mode: Mode::Paper,
        ts_ms: c.ts_ms,
        direction: if c.side == Side::Long {
            Direction::Long
        } else {
            Direction::Short
        },
        features,
        regime: if c.regimes.is_empty() {
            "none".into()
        } else {
            c.regimes.join("+")
        },
        timeframe_context: BTreeMap::new(),
        book_features: book,
        data_quality: DataQuality {
            book_age_ms: c.book_age_ms.max(0),
            trade_age_ms: 0,
            book_valid: c.data_ok,
            book_snapshot_interval_ms: 1_000,
        },
        forecasts,
        trade_horizon_s: 120,
        win_probability: c.p_tp,
        p_tp_first: c.p_tp,
        p_sl_first: c.p_sl.min(1.0 - c.p_tp).max(0.0),
        expected_mfe: c.tp_frac.min(s120 * 1.5).max(0.0),
        expected_mae: c.sl_frac.min(s120 * 1.5).max(0.0),
        model_disagreement_sd: None,
        est_spread_bps: c.spread_bps.max(0.0),
        est_fee_bps: 2.0 * c.fee * 1e4,
        est_slippage_bps: 2.0 * c.slip * 1e4,
        ref_price: dec(c.mid),
        stop_price: dec(o.stop_loss),
        target_price: dec(o.take_profit),
        qty: dec(o.qty),
        notional: dec(o.notional_account),
        margin: dec(o.margin_account),
        leverage: dec(o.leverage.max(1.0)),
        sizing_note: format!(
            "risk budget INR {:.4}, risk at stop INR {:.4}, fraction {:.4}",
            o.risk_budget, o.risk_at_stop, o.risk_fraction_used
        ),
        strategy_version: if c.gamble {
            "desk.gamble.v1".into()
        } else {
            "desk.bracket.v1".into()
        },
        model_version: c.model_version.into(),
        risk: RiskDecision {
            approved: true,
            codes: vec!["governor_approved".into()],
            risk_budget: dec(o.risk_budget.max(o.risk_at_stop)),
        },
        reasoning_codes: {
            let mut v = vec![if c.calibrated {
                "calibrated".to_string()
            } else {
                "uncalibrated_prior".to_string()
            }];
            if c.gamble {
                v.push("mode=gamble".into());
            }
            v
        },
    }
}

/// Journal the snapshot; Err means the entry must not be sent.
pub fn record_decision(d: &mut Desk, s: DecisionSnapshot) -> Result<(), String> {
    let Some(e) = d.learn.engine.as_mut() else {
        return Err(d
            .learn
            .error
            .clone()
            .unwrap_or_else(|| "learning journal unavailable".into()));
    };
    e.record_decision(s).map_err(|e| format!("{e:?}"))?;
    d.learn.snapshots += 1;
    Ok(())
}

fn fills_for(d: &Desk, id: &str, entry: bool) -> Vec<Fill> {
    d.fills
        .iter()
        .rev()
        .filter(|f| f["position_id"] == id)
        .filter(|f| f["reason"].as_str().unwrap_or("").starts_with("entry") == entry)
        .map(|f| Fill {
            ts_ms: f["ts_ms"].as_i64().unwrap_or(0),
            price: dec(f["price"].as_f64().unwrap_or(0.0)),
            qty: dec(f["qty"].as_f64().unwrap_or(0.0)),
            fee: dec(f["fee"].as_f64().unwrap_or(0.0)),
        })
        .collect()
}

/// Queue the closed trade until every horizon has elapsed.
pub fn on_closed(d: &mut Desk, idx: usize, exit_price: f64, reason: &str, now: i64) {
    let p = &d.positions[idx].pos;
    let id = p.order.trace_id.clone();
    let pend = Pending {
        trade_id: id.clone(),
        pair: p.order.pair.clone(),
        side: p.order.side,
        opened_ms: p.opened_at_ms.unwrap_or(now as u64) as i64,
        closed_ms: now,
        avg_entry: p.avg_entry,
        exit_ref: exit_price,
        exit_reason: reason.into(),
        entry_fills: fills_for(d, &id, true),
        exit_fills: fills_for(d, &id, false),
    };
    d.learn.pending.push(pend);
}

fn exit_reason(r: &str) -> ExitReason {
    match r {
        "tp" => ExitReason::TakeProfit,
        "sl" => ExitReason::StopLoss,
        "time_stop" => ExitReason::TimeExit,
        "flatten" => ExitReason::KillSwitch,
        "liquidated" | "protection_failed" | "data_integrity" => ExitReason::Error,
        _ => ExitReason::Manual,
    }
}

/// Append outcomes whose 120 s horizon window has fully elapsed.
pub fn resolve(d: &mut Desk, now: i64) {
    let ready: Vec<Pending> = {
        let (r, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut d.learn.pending)
            .into_iter()
            .partition(|p| now >= p.opened_ms + 121_000);
        d.learn.pending = keep;
        r
    };
    for p in ready {
        let h = d.hist.get(&p.pair);
        let px_at = |t: i64| h.and_then(|h| h.iter().find(|x| x.0 >= t).map(|x| x.1));
        let sgn = if p.side == Side::Long { 1.0 } else { -1.0 };
        let (mut mfe, mut mae) = (0.0f64, 0.0f64);
        if let Some(h) = h {
            for x in h
                .iter()
                .filter(|x| x.0 >= p.opened_ms && x.0 <= p.closed_ms)
            {
                let r = sgn * (x.1 / p.avg_entry - 1.0);
                mfe = mfe.max(r);
                mae = mae.max(-r);
            }
        }
        let horizon_moves = HORIZONS_S
            .iter()
            .map(|&hs| HorizonMove {
                horizon_s: hs,
                actual_return: px_at(p.opened_ms + hs as i64 * 1000)
                    .map(|x| (x / p.avg_entry).ln()),
            })
            .collect();
        let o = TradeOutcome {
            trade_id: p.trade_id.clone(),
            entry_fills: p.entry_fills,
            exit_fills: p.exit_fills,
            exit_ref_price: dec(p.exit_ref),
            funding: dec(0.0),
            exit_reason: exit_reason(&p.exit_reason),
            mfe,
            mae,
            horizon_moves,
            flags: OutcomeFlags::default(),
            recorded_ms: now,
        };
        let r = match d.learn.engine.as_mut() {
            Some(e) => e
                .record_outcome(o)
                .map(|_| ())
                .map_err(|e| format!("{e:?}")),
            None => Err("learning journal unavailable".into()),
        };
        match r {
            Ok(()) => d.learn.outcomes += 1,
            Err(e) => d.incident(
                "learning",
                format!("{}: outcome not recorded: {e}", p.trade_id),
            ),
        }
    }
}

/// Dashboard / report JSON: learning dashboard (paper), registry status.
pub fn learning_json(d: &Desk) -> Value {
    let models = std::env::var("QE_MODELS_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../qe-research/models.json")
        });
    let reg = qe_research::registry::ModelRegistry::load_or_default(&models);
    let promotable: Vec<&str> = reg.promotable().map(|m| m.id.as_str()).collect();
    let dash = d.learn.engine.as_ref().map(|e| {
        serde_json::to_value(e.dashboard(i64::MAX, Some(Mode::Paper))).unwrap_or_default()
    });
    json!({
        "status": if d.learn.engine.is_some() { "wired" } else { "unavailable" },
        "error": d.learn.error, "snapshots_recorded": d.learn.snapshots,
        "outcomes_recorded": d.learn.outcomes, "pending_outcomes": d.learn.pending.len(),
        "dashboard": dash,
        "models": {"file": models.display().to_string(), "found": models.exists(),
            "champion": reg.champion(), "champion_reason": reg.champion_reason,
            "rollback": reg.rollback, "promotable": promotable,
            "promotion_criteria": reg.promotion_criteria, "promotion_history": reg.promotion_history,
            "cards": reg.models, "note": "promotion is advisory; no model overrides the risk governor"},
        "label": "PAPER",
    })
}
