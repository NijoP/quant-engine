//! Builds the [`CalibrationReport`] from a recording and (optionally) the
//! candle history store.
use crate::calibration::CalibrationReport;
use crate::eval::{self, DESK_PARAMS};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::Path;

pub const MODEL_DESK: &str = "desk.fp.v1";
pub const MODEL_MOM: &str = "mom.fp.v1";
pub const MODEL_MOM_1M: &str = "mom.fp.v1.1m";

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(v[v.len() / 2])
}

pub fn build(
    rec_dir: &Path,
    hist_dir: Option<&Path>,
    folds: usize,
    stride_s: f64,
) -> Result<CalibrationReport> {
    let rec = eval::load_recording(rec_dir)?;
    let stride = (stride_s * 1000.0) as i64;
    let desk = eval::rt_samples(&rec, &DESK_PARAMS, true, stride);
    let mom = eval::rt_samples(&rec, &DESK_PARAMS, false, stride);
    let mut entries = eval::entries(MODEL_DESK, "rt1s+book", &desk, folds);
    entries.extend(eval::entries(MODEL_MOM, "rt1s", &mom, folds));
    let mut evaluation = json!({
        "params": DESK_PARAMS,
        "folds": folds,
        "recording": {"dir": rec_dir, "rt_polls": rec.rt_polls, "t_first_ms": rec.t_first, "t_last_ms": rec.t_last,
            "minutes": (rec.t_last - rec.t_first) as f64 / 60_000.0, "pairs": rec.rt.len(),
            "book_pairs": rec.books.len(), "book_snapshots": rec.books.values().map(|b| b.len()).sum::<usize>(),
            "fees_from_metadata": rec.fees},
        MODEL_DESK: {"data": eval::describe(&desk, stride_s), "trading": eval::trading(&desk)},
        MODEL_MOM: {"data": eval::describe(&mom, stride_s), "trading": eval::trading(&mom)},
    });
    if let Some(h) = hist_dir {
        let mut all = vec![];
        let mut pairs: Vec<String> = std::fs::read_dir(h)?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect();
        pairs.sort();
        for pair in &pairs {
            let Ok(c) = crate::history::load(h, pair, "1") else {
                continue;
            };
            // net expectancy only where a spread was actually measured in the recording
            let cost = rec
                .books
                .get(pair)
                .and_then(|b| median(b.iter().map(|x| x.2).collect()))
                .map(|sp| {
                    2.0 * rec
                        .fees
                        .get(pair)
                        .copied()
                        .unwrap_or(eval::DEFAULT_TAKER_FEE)
                        + sp
                        + 2.0 * eval::SLIPPAGE_PER_SIDE
                });
            all.extend(eval::candle_samples(pair, &c, &DESK_PARAMS, cost));
        }
        entries.extend(eval::entries(MODEL_MOM_1M, "candles1m", &all, folds));
        evaluation[MODEL_MOM_1M] = json!({"data": eval::describe(&all, 60.0), "trading": eval::trading(&all),
            "history_dir": h, "cost_note": "cost uses the median spread measured in the recording; pairs without a recorded book have no net metrics"});
    }
    Ok(CalibrationReport {
        generated_ms: qe_coindcx::auth::request_timestamp_ms(),
        entries,
        evaluation,
    })
}

/// One line per entry with the headline OOS numbers.
pub fn summary(r: &CalibrationReport) -> Value {
    let rows: Vec<Value> = r
        .entries
        .iter()
        .map(|e| {
            let w = e.walk_forward.as_ref();
            json!({"model": e.model_id, "target": e.target, "status": e.status, "n_oos": e.n_oos,
                "base_rate": w.map(|w| w.base_rate_oos), "mean_p": w.map(|w| w.mean_p_raw_oos),
                "brier_raw": w.map(|w| w.brier_raw), "brier_clim": w.map(|w| w.brier_clim),
                "skill_raw": w.map(|w| w.brier_skill_raw), "skill_recal": w.map(|w| w.brier_skill_recal),
                "ece_raw": w.map(|w| w.ece_raw), "ece_recal": w.map(|w| w.ece_recal), "logloss_raw": w.map(|w| w.log_loss_raw)})
        })
        .collect();
    json!({"entries": rows, "evaluation": r.evaluation})
}
