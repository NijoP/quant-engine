//! QUANT-ENGINE capital and target engine (sprint 3, s3-desk).
//!
//! Narrow API:
//! - [`campaign`]: the 24 Nov 2026 campaign controller (INR 100 -> 50,000, 00:00 -> 22:00
//!   IST, `100 * 500^(h/22)` benchmark curve, campaign risk profile, permanent stop).
//! - [`capital`]: the two-stage capital plan (Stage 0 paper until 20:00 IST, Stage 1
//!   INR 100 -> 3,000 by 22:30 IST, the 22:30 checkpoint workflow with a user-confirmed
//!   INR 2,000 withdrawal, Stage 2 from the verified retained balance -> INR 50,000 by
//!   22:00 IST 24 Nov, final checkpoint). Money is `rust_decimal::Decimal`.
//! - [`frequency`]: r = (Ct/C0)^(1/n) - 1 reference tables and a seeded stochastic
//!   simulation with wins/losses, costs, missed opportunities, rejections and downtime.
//! - [`paper`]: seeded paper fill model (partial fills, rejections, simulated latency).
//! - [`report`]: report generator (8 PM paper, 22:30 Stage 1, hourly Stage 2, final),
//!   each separating measured P&L from target-implied requirements and hypotheticals.
//!
//! Nothing here calls CoinDCX, reads credentials or moves funds. Target logic never
//! selects or loosens risk limits: the risk mode is a function of the stage only.

pub mod campaign;
pub mod capital;
pub mod frequency;
pub mod paper;
pub mod report;

pub use rust_decimal::Decimal;
