//! Owner-run live-readiness tooling for QUANT-ENGINE (sprint 3, `fm/s3-live`).
//!
//! Every command here is run BY THE ACCOUNT OWNER on their own machine. The
//! developers never ran any of it against the real CoinDCX API; it is tested
//! with an in-memory account and a local mock HTTP server that serves the
//! documented example responses.
//!
//! # Narrow API (hooks for qe-desk)
//! ```text
//! let dir = qe_live_check::state::StateDir::from_env();        // QE_LIVE_STATE_DIR, default data/live
//! let gate = qe_live_check::arming::gate_for_desk(&dir, creds_configured, now_ms);
//!     // LiveGate: env mode/confirmation + the owner's unexpired arm record; closed otherwise
//! qe_live_check::arming::arm_status(&dir, now_ms)               // secret-free JSON for the dashboard
//! qe_live_check::arming::kill(&dir, "reason", now_ms)           // kill switch: disarm now (audited)
//! qe_live_check::checkpoint::flatten_all(&api, margins, poll)   // cancel-all + exit + verify flat
//! qe_live_check::checkpoint::read_balances(&api)                // actual balances (Decimal)
//! qe_live_check::readiness::{snapshot, reconcile}               // exchange vs desk state
//! qe_live_check::state::record_check(&dir, item, status, now_ms)
//! ```
//! `api` is anything implementing [`api::AccountApi`]; `qe_coindcx::private::PrivateClient`
//! implements it. Arming, validation and checkpoints need typed owner
//! confirmation ([`confirm::Confirm`]); nothing in the desk can supply it.
//! Nothing here transfers or withdraws funds: no transfer endpoint is wired.
pub mod api;
pub mod arming;
pub mod checkpoint;
pub mod confirm;
pub mod money;
pub mod readiness;
pub mod state;
pub mod validate;

/// Milliseconds since the Unix epoch (wall clock).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Polling policy for "wait until the exchange shows X" loops.
#[derive(Debug, Clone, Copy)]
pub struct Poll {
    pub interval: std::time::Duration,
    pub attempts: u32,
}

impl Poll {
    /// 500 ms x 40 = 20 s.
    pub const DEFAULT: Poll = Poll {
        interval: std::time::Duration::from_millis(500),
        attempts: 40,
    };
    /// For tests: no sleeping.
    pub const IMMEDIATE: Poll = Poll {
        interval: std::time::Duration::ZERO,
        attempts: 5,
    };
}
