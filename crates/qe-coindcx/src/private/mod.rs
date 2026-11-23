//! Authenticated CoinDCX futures integration (account reads, order management,
//! private socket events). Built against docs.coindcx.com, re-read 2026-11-23.
//! NEVER exercised against the real API by the developers of this module; the
//! account owner validates it with the `live_readiness` command.
//!
//! # Narrow API for qe-desk
//! ```text
//! let (creds, status) = qe_coindcx::auth::load_credentials();   // status: configured / not
//! let gate = qe_coindcx::live::LiveGate::from_env(status.configured); // closed unless all 5 conditions
//! let pc = qe_coindcx::private::PrivateClient::new(creds?, gate);
//! pc.wallets() / pc.positions(&["USDT","INR"]) / pc.list_orders(..)   // reads
//! pc.update_leverage / create_order / reconcile_order / create_tpsl /
//!   exit_position / cancel_order / cancel_all_for_position            // gated mutations
//! private::socket::{join_frame, parse_event}                         // account socket
//! ```
//! Errors: [`PrivateError`]; `OutcomeUnknown` means "reconcile first".
pub mod client;
pub mod error;
pub mod ratelimit;
pub mod readonly;
pub mod reconcile;
pub mod socket;
pub mod types;

pub use client::PrivateClient;
pub use error::PrivateError;
pub use reconcile::{LedgerState, OrderIntent, Reconciliation};
pub use types::{
    FuturesOrderRecord, FuturesPosition, FuturesWallet, NewOrder, Side, SpotBalance, TpslResult,
};
