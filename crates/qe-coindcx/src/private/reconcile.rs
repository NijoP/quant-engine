//! Deterministic order tracking and reconcile-before-retry.
//!
//! CoinDCX futures create-order does NOT document a client order id field, so
//! the id from `orders::client_order_id` is local only. To find out whether an
//! order whose outcome is unknown (timeout, 5xx, unreadable body) actually
//! reached the exchange, we list orders for the same side and match on pair,
//! side, quantity and creation time. Until that reconciliation says
//! `ConfirmedAbsent`, the ledger refuses any new submission for the intent,
//! and even then the retry must use a NEW client id (next sequence number).
use super::error::PrivateError;
use super::types::{FuturesOrderRecord, Side};
use serde::Serialize;
use std::collections::BTreeMap;

/// Allowed clock skew before submission / latency after it when matching.
pub const MATCH_BEFORE_MS: f64 = 5_000.0;
pub const MATCH_AFTER_MS: f64 = 30_000.0;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OrderIntent {
    pub client_order_id: String,
    pub pair: String,
    pub side: Side,
    pub total_quantity: f64,
    pub margin_currency_short_name: String,
    pub submitted_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LedgerState {
    Submitting,
    Acked {
        exchange_order_id: String,
    },
    /// Outcome unknown: reconcile before doing anything else.
    Unknown {
        reason: String,
    },
    /// Reconciliation found the order on the exchange.
    Reconciled {
        exchange_order_id: String,
    },
    /// Reconciliation found no matching order: safe to retry with a NEW id.
    ConfirmedAbsent,
    /// Definite exchange rejection; nothing executed.
    Rejected {
        reason: String,
    },
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Reconciliation {
    Found(FuturesOrderRecord),
    NotFound,
    /// More than one candidate: do not guess; treat as present and escalate.
    Ambiguous(usize),
}

pub fn match_order(intent: &OrderIntent, orders: &[FuturesOrderRecord]) -> Reconciliation {
    let t0 = intent.submitted_at_ms as f64;
    let hits: Vec<&FuturesOrderRecord> = orders
        .iter()
        .filter(|o| o.pair.as_deref() == Some(intent.pair.as_str()))
        .filter(|o| o.side.as_deref() == Some(intent.side.as_str()))
        .filter(|o| {
            o.margin_currency_short_name
                .as_deref()
                .is_none_or(|m| m == intent.margin_currency_short_name)
        })
        .filter(|o| {
            o.total_quantity.is_some_and(|q| {
                (q - intent.total_quantity).abs() <= 1e-9 * intent.total_quantity.abs().max(1.0)
            })
        })
        .filter(|o| {
            o.created_at
                .is_some_and(|c| c >= t0 - MATCH_BEFORE_MS && c <= t0 + MATCH_AFTER_MS)
        })
        .collect();
    match hits.len() {
        0 => Reconciliation::NotFound,
        1 => Reconciliation::Found(hits[0].clone()),
        n => Reconciliation::Ambiguous(n),
    }
}

#[derive(Debug, Default, Serialize)]
pub struct OrderLedger {
    entries: BTreeMap<String, (OrderIntent, LedgerState)>,
}

impl OrderLedger {
    /// Register a new submission. Refuses a client id that is already known
    /// (idempotency) and any new intent while another one is unresolved.
    pub fn begin(&mut self, intent: OrderIntent) -> Result<(), PrivateError> {
        if self.entries.contains_key(&intent.client_order_id) {
            return Err(PrivateError::DuplicateClientOrder {
                client_order_id: intent.client_order_id,
            });
        }
        if let Some((id, _)) = self
            .entries
            .iter()
            .find(|(_, (_, s))| matches!(s, LedgerState::Unknown { .. } | LedgerState::Submitting))
        {
            return Err(PrivateError::OutcomeUnknown {
                message: format!("order {id} is unresolved; reconcile before submitting another"),
            });
        }
        self.entries.insert(
            intent.client_order_id.clone(),
            (intent, LedgerState::Submitting),
        );
        Ok(())
    }

    pub fn set(&mut self, client_order_id: &str, state: LedgerState) {
        if let Some(e) = self.entries.get_mut(client_order_id) {
            e.1 = state;
        }
    }

    pub fn get(&self, client_order_id: &str) -> Option<&(OrderIntent, LedgerState)> {
        self.entries.get(client_order_id)
    }

    pub fn apply_reconciliation(
        &mut self,
        client_order_id: &str,
        r: &Reconciliation,
    ) -> LedgerState {
        let state = match r {
            Reconciliation::Found(o) => LedgerState::Reconciled {
                exchange_order_id: o.id.clone().unwrap_or_default(),
            },
            Reconciliation::NotFound => LedgerState::ConfirmedAbsent,
            Reconciliation::Ambiguous(n) => LedgerState::Unknown {
                reason: format!("{n} candidate orders match; manual review required"),
            },
        };
        self.set(client_order_id, state.clone());
        state
    }

    pub fn snapshot(&self) -> Vec<(OrderIntent, LedgerState)> {
        self.entries.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(id: &str) -> OrderIntent {
        OrderIntent {
            client_order_id: id.into(),
            pair: "B-ID_USDT".into(),
            side: Side::Buy,
            total_quantity: 45.0,
            margin_currency_short_name: "USDT".into(),
            submitted_at_ms: 1_705_565_061_000,
        }
    }

    fn rec(pair: &str, side: &str, q: f64, at: f64) -> FuturesOrderRecord {
        FuturesOrderRecord {
            id: Some(format!("{pair}-{q}-{at}")),
            pair: Some(pair.into()),
            side: Some(side.into()),
            total_quantity: Some(q),
            created_at: Some(at),
            ..Default::default()
        }
    }

    #[test]
    fn matches_only_the_submitted_order() {
        let i = intent("qe-1");
        let t = i.submitted_at_ms as f64;
        let book = vec![
            rec("B-ID_USDT", "buy", 45.0, t + 504.0),
            rec("B-ID_USDT", "sell", 45.0, t + 504.0),
            rec("B-ID_USDT", "buy", 44.0, t + 504.0),
            rec("B-BTC_USDT", "buy", 45.0, t + 504.0),
            rec("B-ID_USDT", "buy", 45.0, t - 60_000.0),
        ];
        assert!(
            matches!(match_order(&i, &book), Reconciliation::Found(o) if o.created_at == Some(t + 504.0))
        );
        assert_eq!(match_order(&i, &book[1..]), Reconciliation::NotFound);
        let dup = vec![book[0].clone(), rec("B-ID_USDT", "buy", 45.0, t + 900.0)];
        assert_eq!(match_order(&i, &dup), Reconciliation::Ambiguous(2));
    }

    #[test]
    fn ledger_blocks_duplicates_and_unresolved_retries() {
        let mut l = OrderLedger::default();
        l.begin(intent("qe-1")).unwrap();
        assert!(matches!(
            l.begin(intent("qe-1")),
            Err(PrivateError::DuplicateClientOrder { .. })
        ));
        // Still Submitting -> a different id is refused too.
        assert!(matches!(
            l.begin(intent("qe-2")),
            Err(PrivateError::OutcomeUnknown { .. })
        ));
        l.set(
            "qe-1",
            LedgerState::Unknown {
                reason: "timeout".into(),
            },
        );
        assert!(l.begin(intent("qe-2")).is_err());
        let s = l.apply_reconciliation("qe-1", &Reconciliation::NotFound);
        assert_eq!(s, LedgerState::ConfirmedAbsent);
        // The same id can never be reused; a new id may proceed.
        assert!(l.begin(intent("qe-1")).is_err());
        l.begin(intent("qe-2")).unwrap();
        l.set(
            "qe-2",
            LedgerState::Acked {
                exchange_order_id: "x".into(),
            },
        );
        let s = l.apply_reconciliation("qe-2", &Reconciliation::Ambiguous(2));
        assert!(matches!(s, LedgerState::Unknown { .. }));
        assert!(l.begin(intent("qe-3")).is_err());
    }
}
