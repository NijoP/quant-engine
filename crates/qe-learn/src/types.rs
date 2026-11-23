//! Shared vocabulary: horizons, direction, paper/live mode, errors.
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Forecast horizons (seconds) evaluated by the learning engine (intent §4).
pub const HORIZONS_S: [u32; 8] = [1, 5, 10, 15, 30, 60, 90, 120];
/// Position lifetime from the confirmed fill (intent: 120 000 ms).
pub const LIFETIME_MS: i64 = 120_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Long,
    Short,
}

impl Direction {
    pub fn sign(self) -> f64 {
        match self {
            Direction::Long => 1.0,
            Direction::Short => -1.0,
        }
    }
    pub fn sign_dec(self) -> Decimal {
        match self {
            Direction::Long => Decimal::ONE,
            Direction::Short => Decimal::NEGATIVE_ONE,
        }
    }
}

/// Paper and live records are never mixed in a report unless the caller asks
/// for both explicitly; every serialised record carries its mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Paper,
    Live,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LearnError {
    /// A record failed validation; the message names the field.
    Invalid(String),
    /// trade id already recorded (records are append-only, never replaced)
    Duplicate(String),
    UnknownTrade(String),
    /// a record would be appended out of time order (no look-ahead / leakage)
    NotChronological {
        last_ms: i64,
        got_ms: i64,
    },
    /// entry and exit quantities disagree: exchange state must be reconciled first
    ReconciliationRequired(String),
    /// a learning output tried to loosen risk (§6.6); every violation listed
    SafetyViolation(Vec<String>),
    /// champion/challenger gate failed; every failed criterion listed
    PromotionRejected(Vec<String>),
    /// journal hash or sequence mismatch
    Tampered(String),
    Io(String),
}

impl fmt::Display for LearnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LearnError::Invalid(m) => write!(f, "invalid record: {m}"),
            LearnError::Duplicate(id) => write!(f, "duplicate record for trade {id}"),
            LearnError::UnknownTrade(id) => write!(f, "unknown trade {id}"),
            LearnError::NotChronological { last_ms, got_ms } => {
                write!(f, "not chronological: {got_ms} < last {last_ms}")
            }
            LearnError::ReconciliationRequired(m) => write!(f, "reconciliation required: {m}"),
            LearnError::SafetyViolation(v) => write!(f, "safety violation: {}", v.join("; ")),
            LearnError::PromotionRejected(v) => write!(f, "promotion rejected: {}", v.join("; ")),
            LearnError::Tampered(m) => write!(f, "journal tampered: {m}"),
            LearnError::Io(m) => write!(f, "io: {m}"),
        }
    }
}

impl std::error::Error for LearnError {}

pub type Result<T> = std::result::Result<T, LearnError>;

pub(crate) fn finite(name: &str, x: f64) -> Result<()> {
    if x.is_finite() {
        Ok(())
    } else {
        Err(LearnError::Invalid(format!("{name} is not finite")))
    }
}

pub(crate) fn prob(name: &str, x: f64) -> Result<()> {
    if x.is_finite() && (0.0..=1.0).contains(&x) {
        Ok(())
    } else {
        Err(LearnError::Invalid(format!(
            "{name}={x} is not a probability"
        )))
    }
}

pub(crate) fn to_f64(d: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    d.to_f64().unwrap_or(f64::NAN)
}

#[cfg(test)]
pub(crate) fn dec(s: &str) -> Decimal {
    s.parse().expect("decimal literal")
}
