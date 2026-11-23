//! CoinDCX integration: public market-data client (REST), authentication /
//! signing module, and the futures order adapter (live submission disabled).
//!
//! Every endpoint and field used here was checked against a live public
//! response on 2026-11-23 and against https://docs.coindcx.com/ (see
//! docs/COINDCX_API_MATRIX.md). No authenticated endpoint is ever called.

pub mod auth;
pub mod client;
pub mod orders;
pub mod types;
pub mod ws;

pub use client::PublicClient;
