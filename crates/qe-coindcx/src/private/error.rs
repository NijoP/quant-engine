//! Error taxonomy for authenticated CoinDCX calls.
//!
//! The documented error shape is an HTTP status plus a JSON body that carries a
//! `message` (see per-endpoint "Possible Error Codes" tables and the global
//! error table: 400, 401, 404, 422, 429, 500, 503). We classify on status and
//! well-known message fragments copied from those tables. Anything we cannot
//! classify keeps the raw message.
//!
//! The key safety distinction is [`PrivateError::OutcomeUnknown`]: a mutating
//! request whose outcome we do not know (timeout after send, 5xx, unreadable
//! response). It must be reconciled against exchange state before any retry.
use crate::live::GateError;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrivateError {
    /// No credentials loaded in this process.
    NotConfigured,
    /// Mutating call refused by the live gate.
    GateClosed {
        gate: GateError,
    },
    /// Refused locally: an order with this client id is already tracked.
    DuplicateClientOrder {
        client_order_id: String,
    },
    /// Circuit breaker open after 429s / repeated failures.
    CircuitOpen {
        retry_after_ms: u64,
    },
    /// Local limiter has no budget; nothing was sent.
    Throttled {
        retry_after_ms: u64,
    },
    /// HTTP 429 from the exchange.
    RateLimited {
        message: String,
    },
    /// HTTP 401: key wrong / signature or timestamp rejected.
    Unauthorized {
        message: String,
    },
    InsufficientFunds {
        message: String,
    },
    ExitOnly {
        message: String,
    },
    /// update_leverage tier rejection, e.g. "Max allowed leverage for current position size = 5x".
    LeverageTier {
        message: String,
        max_allowed: Option<f64>,
    },
    LeverageMismatch {
        message: String,
    },
    /// Other documented 4xx rejection (the request definitely did not execute).
    Rejected {
        status: u16,
        message: String,
    },
    /// 5xx on a read: safe to retry later.
    Server {
        status: u16,
        message: String,
    },
    /// Mutation sent but outcome unknown: reconcile before any retry.
    OutcomeUnknown {
        message: String,
    },
    /// Request never left this process (connect/DNS failure, serialisation).
    Transport {
        message: String,
    },
    Decode {
        message: String,
    },
}

impl std::fmt::Display for PrivateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for PrivateError {}

impl PrivateError {
    /// Whether an automatic retry (after backoff) is safe. Mutations are never
    /// auto-retried; `OutcomeUnknown` requires reconciliation instead.
    pub fn retry_safe(&self, mutating: bool) -> bool {
        if mutating {
            return false;
        }
        matches!(
            self,
            PrivateError::Throttled { .. }
                | PrivateError::RateLimited { .. }
                | PrivateError::Server { .. }
                | PrivateError::Transport { .. }
                | PrivateError::OutcomeUnknown { .. }
        )
    }
}

/// Pull `message` (or `error`) out of a CoinDCX error body; fall back to raw text.
pub fn error_message(body: &str) -> String {
    let v: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let m = v.as_ref().and_then(|v| {
        v.get("message")
            .or_else(|| v.get("error"))
            .and_then(|m| m.as_str())
            .map(str::to_string)
    });
    let mut s = m.unwrap_or_else(|| body.chars().take(300).collect());
    if s.is_empty() {
        s = "<empty body>".into();
    }
    s
}

/// Classify a non-2xx response. `mutating` turns 5xx into `OutcomeUnknown`.
pub fn classify(status: u16, body: &str, mutating: bool) -> PrivateError {
    let message = error_message(body);
    let lower = message.to_ascii_lowercase();
    match status {
        429 => PrivateError::RateLimited { message },
        401 => PrivateError::Unauthorized { message },
        500..=599 if mutating => PrivateError::OutcomeUnknown {
            message: format!("HTTP {status}: {message}"),
        },
        500..=599 => PrivateError::Server { status, message },
        _ if lower.contains("insufficient funds") => PrivateError::InsufficientFunds { message },
        _ if lower.contains("exit-only") || lower.contains("exit only") => {
            PrivateError::ExitOnly { message }
        }
        _ if lower.contains("max allowed leverage") => PrivateError::LeverageTier {
            max_allowed: parse_max_leverage(&message),
            message,
        },
        _ if lower.contains("leverage must be equal") => PrivateError::LeverageMismatch { message },
        _ => PrivateError::Rejected { status, message },
    }
}

/// "Max allowed leverage for current position size = 5x" -> Some(5.0).
fn parse_max_leverage(m: &str) -> Option<f64> {
    let tail = m.rsplit('=').next()?.trim();
    tail.trim_end_matches(['x', 'X']).trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifies_documented_errors() {
        let b = |m: &str| format!(r#"{{"message":"{m}","status":400,"code":400}}"#);
        assert!(matches!(
            classify(429, "Too Many Requests", false),
            PrivateError::RateLimited { .. }
        ));
        assert!(matches!(
            classify(401, &b("Unauthorized access"), false),
            PrivateError::Unauthorized { .. }
        ));
        assert!(matches!(
            classify(400, &b("Insufficient funds"), true),
            PrivateError::InsufficientFunds { .. }
        ));
        assert!(matches!(
            classify(
                400,
                &b("Instrument is in exit-only mode. You can’t add more position."),
                true
            ),
            PrivateError::ExitOnly { .. }
        ));
        match classify(
            400,
            &b("Max allowed leverage for current position size = 5x"),
            true,
        ) {
            PrivateError::LeverageTier { max_allowed, .. } => assert_eq!(max_allowed, Some(5.0)),
            e => panic!("{e:?}"),
        }
        assert!(matches!(
            classify(
                422,
                &b("Order leverage must be equal to position leverage"),
                true
            ),
            PrivateError::LeverageMismatch { .. }
        ));
        match classify(400, &b("Minimum order value should be 5 USDT"), true) {
            PrivateError::Rejected { status, message } => {
                assert_eq!(status, 400);
                assert!(message.contains("Minimum order value"));
            }
            e => panic!("{e:?}"),
        }
    }
    #[test]
    fn server_errors_on_mutations_are_unknown_outcomes() {
        assert!(matches!(
            classify(503, "", true),
            PrivateError::OutcomeUnknown { .. }
        ));
        assert!(matches!(
            classify(500, "", false),
            PrivateError::Server { .. }
        ));
        let unknown = classify(500, "", true);
        assert!(!unknown.retry_safe(true));
        assert!(classify(500, "", false).retry_safe(false));
        assert!(!classify(400, "{}", false).retry_safe(false));
    }
}
