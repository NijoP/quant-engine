//! qe-research: recorder, historical loader, chronological walk-forward
//! evaluator and calibration-status registry for the QUANT-ENGINE forecasts.
//!
//! Public market data only. This crate never reads credentials, never calls an
//! authenticated endpoint and never places an order.
//!
//! # API for s2-desk (narrow, stable)
//! ```no_run
//! use qe_research::calibration::{CalibrationReport, Status};
//! use qe_research::report::MODEL_DESK;
//! // produced offline by `qe-research evaluate ... --out calibration.json`
//! let r = CalibrationReport::load(std::path::Path::new("calibration.json"))
//!     .unwrap_or_default(); // missing file => every model Uncalibrated
//! let st: Status = r.status(MODEL_DESK, "tp_first_long");
//! if r.grants_authority(MODEL_DESK, "tp_first_long") {
//!     // only here may a forecast drive an entry; use the recalibrated probability
//!     let p = r.recalibrate(MODEL_DESK, "tp_first_long", 0.42);
//! } else {
//!     // Uncalibrated / Calibrating: show as advisory, deny strategy authority
//! }
//! ```
//! Model ids: [`report::MODEL_DESK`] (`desk.fp.v1`: the desk's live
//! momentum+OBI first-passage model on 1 s RT data), [`report::MODEL_MOM`]
//! (same with OBI = 0), [`report::MODEL_MOM_1M`] (1m-candle variant).
//! Targets: `tp_first_{long,short}`, `sl_first_{long,short}` (120 s), `up_{h}s`
//! for h in 1, 2, 5, 10, 15, 30, 60, 90, 120 (RT) or 60, 120 (1m).
//! The status rule is documented in [`calibration`]; results in docs/CALIBRATION.md.
//!
//! # API for s3-learn / s3-desk: champion/challenger registry
//! [`registry::ModelRegistry::load_or_default`]`("crates/qe-research/models.json")`, then
//! `champion()`, `model(id)`, `is_promotable(id)`. Built by `qe-research challenge` ([`challenger`]).
pub mod calibration;
pub mod challenger;
pub mod eval;
pub mod history;
pub mod record;
pub mod registry;
pub mod report;
