//! CoinDCX integration: public market-data client (REST), authentication /
//! signing module, the authenticated futures client (`private`, every mutation
//! behind `live::LiveGate`), and the sprint-1 order body builders.
//!
//! Public endpoints were checked against live responses on 2026-11-23; the
//! authenticated endpoints in `private` were built from https://docs.coindcx.com/
//! only and have never been called by the developers. The desk never calls
//! them unless credentials are configured, and never mutates anything unless
//! the live gate is open.

pub mod auth;
pub mod client;
pub mod live;
pub mod orders;
pub mod private;
pub mod types;

pub use client::PublicClient;
