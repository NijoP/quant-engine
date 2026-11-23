//! qe-research: recorder, historical loader, chronological walk-forward
//! evaluator and calibration-status registry for the QUANT-ENGINE forecasts.
//!
//! Public market data only. This crate never reads credentials, never calls an
//! authenticated endpoint and never places an order.
pub mod calibration;
pub mod eval;
pub mod history;
pub mod record;
pub mod report;
