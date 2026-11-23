//! Authenticated CoinDCX futures client.
//!
//! * Read-only-in-effect calls (wallets, positions, list orders) need only
//!   credentials.
//! * Every state-changing call (update_leverage, create order, cancel,
//!   create_tpsl, exit, cancel-all-for-position) first passes
//!   [`LiveGate::check`]; with the default gate (paper) each one returns
//!   `PrivateError::GateClosed` without touching the network.
//! * Requests pass the private [`RateLimiter`] (global + per endpoint + 429
//!   backoff) and a circuit breaker (5 consecutive failures -> 30 s open).
//! * Mutations are never auto-retried. A mutation with an unknown outcome is
//!   recorded as `Unknown` in the [`OrderLedger`] and must go through
//!   [`PrivateClient::reconcile_order`] before anything else is submitted.
//! * Nothing here logs request bodies, headers or credentials.
use super::error::{classify, PrivateError};
use super::ratelimit::RateLimiter;
use super::reconcile::{match_order, LedgerState, OrderIntent, OrderLedger, Reconciliation};
use super::types::*;
use crate::auth::{request_timestamp_ms, Credentials, HEADER_API_KEY, HEADER_SIGNATURE};
use crate::live::LiveGate;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub const API_BASE: &str = "https://api.coindcx.com";
const TIMEOUT: Duration = Duration::from_secs(5);
const BREAKER_THRESHOLD: u32 = 5;
const BREAKER_OPEN: Duration = Duration::from_secs(30);
const READ_ATTEMPTS: u32 = 3;

#[derive(Debug, Default)]
struct Breaker {
    consecutive_failures: u32,
    open_until: Option<Instant>,
}

#[derive(Clone)]
pub struct PrivateClient {
    http: reqwest::Client,
    base: String,
    creds: Arc<Credentials>,
    gate: Arc<std::sync::RwLock<LiveGate>>,
    limiter: Arc<Mutex<RateLimiter>>,
    breaker: Arc<Mutex<Breaker>>,
    ledger: Arc<Mutex<OrderLedger>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Method {
    Get,
    Post,
}

impl PrivateClient {
    pub fn new(creds: Credentials, gate: LiveGate) -> Self {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent("quant-engine-desk/0.1")
            .build()
            .expect("http client");
        Self {
            http,
            base: API_BASE.into(),
            creds: Arc::new(creds),
            gate: Arc::new(std::sync::RwLock::new(gate)),
            limiter: Arc::default(),
            breaker: Arc::default(),
            ledger: Arc::default(),
        }
    }

    pub fn gate(&self) -> LiveGate {
        self.gate.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// Replace the gate (e.g. after the readiness checklist is re-run).
    pub fn set_gate(&self, gate: LiveGate) {
        if let Ok(mut g) = self.gate.write() {
            *g = gate;
        }
    }

    fn require_gate(&self) -> Result<(), PrivateError> {
        self.gate()
            .check()
            .map_err(|gate| PrivateError::GateClosed { gate })
    }

    async fn send<B: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: &B,
        mutating: bool,
    ) -> Result<Value, PrivateError> {
        {
            let b = self.breaker.lock().await;
            if let Some(t) = b.open_until.filter(|t| *t > Instant::now()) {
                return Err(PrivateError::CircuitOpen {
                    retry_after_ms: (t - Instant::now()).as_millis() as u64,
                });
            }
        }
        self.limiter
            .lock()
            .await
            .try_acquire(path, Instant::now())
            .map_err(|w| PrivateError::Throttled {
                retry_after_ms: w.as_millis() as u64,
            })?;
        let signed = self
            .creds
            .sign_body(body)
            .map_err(|e| PrivateError::Transport {
                message: e.to_string(),
            })?;
        let url = format!("{}{}", self.base, path);
        let req = match method {
            Method::Get => self.http.get(url),
            Method::Post => self.http.post(url),
        }
        .header("Content-Type", "application/json")
        .header(HEADER_API_KEY, self.creds.api_key())
        .header(HEADER_SIGNATURE, signed.signature)
        .body(signed.body);
        let result = match req.send().await {
            Err(e) if e.is_connect() || e.is_builder() => Err(PrivateError::Transport {
                message: "request not sent (connect/build failure)".into(),
            }),
            Err(e) if mutating => Err(PrivateError::OutcomeUnknown {
                message: if e.is_timeout() {
                    "timeout after send"
                } else {
                    "transport error after send"
                }
                .into(),
            }),
            Err(e) => Err(PrivateError::Transport {
                message: if e.is_timeout() {
                    "timeout"
                } else {
                    "transport error"
                }
                .into(),
            }),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let text = resp.text().await;
                match (status, text) {
                    (200..=299, Ok(t)) => serde_json::from_str::<Value>(&t).map_err(|e| {
                        if mutating {
                            PrivateError::OutcomeUnknown {
                                message: format!("2xx with unreadable body: {e}"),
                            }
                        } else {
                            PrivateError::Decode {
                                message: e.to_string(),
                            }
                        }
                    }),
                    (_, Ok(t)) => Err(classify(status, &t, mutating)),
                    (_, Err(_)) if mutating => Err(PrivateError::OutcomeUnknown {
                        message: format!("HTTP {status} with unreadable body"),
                    }),
                    (_, Err(e)) => Err(PrivateError::Decode {
                        message: e.to_string(),
                    }),
                }
            }
        };
        self.record(&result).await;
        result
    }

    async fn record(&self, r: &Result<Value, PrivateError>) {
        let now = Instant::now();
        let mut b = self.breaker.lock().await;
        match r {
            Ok(_) => {
                b.consecutive_failures = 0;
                b.open_until = None;
                self.limiter.lock().await.on_success();
            }
            Err(PrivateError::RateLimited { .. }) => {
                self.limiter.lock().await.on_429(now);
                b.consecutive_failures += 1;
            }
            Err(PrivateError::Server { .. })
            | Err(PrivateError::Transport { .. })
            | Err(PrivateError::OutcomeUnknown { .. }) => b.consecutive_failures += 1,
            Err(_) => {}
        }
        if b.consecutive_failures >= BREAKER_THRESHOLD {
            b.open_until = Some(now + BREAKER_OPEN);
            b.consecutive_failures = 0;
        }
    }

    /// Read with bounded retry + exponential backoff (reads only).
    async fn read<B: Serialize, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: impl Fn() -> B,
    ) -> Result<T, PrivateError> {
        let mut attempt = 0;
        loop {
            match self.send(method, path, &body(), false).await {
                Ok(v) => {
                    return serde_json::from_value(v).map_err(|e| PrivateError::Decode {
                        message: e.to_string(),
                    })
                }
                Err(e) if e.retry_safe(false) && attempt + 1 < READ_ATTEMPTS => {
                    attempt += 1;
                    let wait = match &e {
                        PrivateError::Throttled { retry_after_ms } => {
                            Duration::from_millis(*retry_after_ms)
                        }
                        _ => Duration::from_millis(250 << attempt),
                    };
                    tokio::time::sleep(wait.min(Duration::from_secs(5))).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    // ------------------------------------------------------------ reads

    /// GET futures wallets (docs: GET with a signed JSON body).
    pub async fn wallets(&self) -> Result<Vec<FuturesWallet>, PrivateError> {
        self.read(Method::Get, PATH_WALLETS, || TimestampOnly {
            timestamp: request_timestamp_ms(),
        })
        .await
    }

    /// All positions for the given margin currencies (pages of 100, max 10 pages).
    pub async fn positions(&self, margin: &[&str]) -> Result<Vec<FuturesPosition>, PrivateError> {
        self.positions_filtered(margin, None, None).await
    }

    pub async fn positions_filtered(
        &self,
        margin: &[&str],
        pairs: Option<&str>,
        position_ids: Option<&str>,
    ) -> Result<Vec<FuturesPosition>, PrivateError> {
        let mut all = Vec::new();
        for page in 1..=10 {
            let batch: Vec<FuturesPosition> = self
                .read(Method::Post, PATH_POSITIONS, || ListPositionsBody {
                    timestamp: request_timestamp_ms(),
                    page: page.to_string(),
                    size: "100".into(),
                    pairs: pairs.map(str::to_string),
                    position_ids: position_ids.map(str::to_string),
                    margin_currency_short_name: margin.iter().map(|m| m.to_string()).collect(),
                })
                .await?;
            let n = batch.len();
            all.extend(batch);
            if n < 100 {
                break;
            }
        }
        Ok(all)
    }

    pub async fn list_orders(
        &self,
        side: Side,
        statuses: &str,
        page: u32,
        size: u32,
        margin: &[&str],
    ) -> Result<Vec<FuturesOrderRecord>, PrivateError> {
        self.read(Method::Post, PATH_LIST_ORDERS, || ListOrdersBody {
            timestamp: request_timestamp_ms(),
            status: statuses.into(),
            side,
            page: page.to_string(),
            size: size.to_string(),
            margin_currency_short_name: margin.iter().map(|m| m.to_string()).collect(),
        })
        .await
    }

    // -------------------------------------------------------- mutations

    async fn mutate<B: Serialize>(&self, path: &str, body: &B) -> Result<Value, PrivateError> {
        self.require_gate()?;
        self.send(Method::Post, path, body, true).await
    }

    /// Never exceeds what the caller passes; the caller derives `leverage` from
    /// live instrument metadata and the risk governor. On a tier rejection the
    /// error carries the exchange's max for the current size: refresh
    /// constraints, do not loop.
    pub async fn update_leverage(
        &self,
        pair: &str,
        margin: &str,
        leverage: u32,
    ) -> Result<Ack, PrivateError> {
        if leverage < 1 {
            return Err(PrivateError::Rejected {
                status: 0,
                message: "leverage must be >= 1 (local check)".into(),
            });
        }
        let v = self
            .mutate(
                PATH_UPDATE_LEVERAGE,
                &UpdateLeverageBody {
                    timestamp: request_timestamp_ms(),
                    leverage: leverage.to_string(),
                    pair: pair.into(),
                    margin_currency_short_name: margin.into(),
                },
            )
            .await?;
        decode(v)
    }

    /// Submit an entry/exit order exactly once under `client_order_id`.
    pub async fn create_order(
        &self,
        client_order_id: &str,
        order: NewOrder,
    ) -> Result<FuturesOrderRecord, PrivateError> {
        self.require_gate()?;
        order.validate().map_err(|m| PrivateError::Rejected {
            status: 0,
            message: format!("local validation: {m}"),
        })?;
        let ts = request_timestamp_ms();
        self.ledger.lock().await.begin(OrderIntent {
            client_order_id: client_order_id.into(),
            pair: order.pair.clone(),
            side: order.side,
            total_quantity: order.total_quantity,
            margin_currency_short_name: order.margin_currency_short_name.clone(),
            submitted_at_ms: ts,
        })?;
        let r = self
            .send(
                Method::Post,
                PATH_CREATE_ORDER,
                &CreateOrderBody {
                    timestamp: ts,
                    order,
                },
                true,
            )
            .await;
        let mut ledger = self.ledger.lock().await;
        match r {
            Ok(v) => {
                let first = match v {
                    Value::Array(mut a) if !a.is_empty() => Some(a.swap_remove(0)),
                    Value::Object(_) => Some(v),
                    _ => None,
                }
                .and_then(|o| serde_json::from_value::<FuturesOrderRecord>(o).ok())
                .filter(|o| o.id.is_some());
                match first {
                    Some(o) => {
                        ledger.set(
                            client_order_id,
                            LedgerState::Acked {
                                exchange_order_id: o.id.clone().unwrap_or_default(),
                            },
                        );
                        Ok(o)
                    }
                    None => {
                        let reason = "2xx without an order id".to_string();
                        ledger.set(
                            client_order_id,
                            LedgerState::Unknown {
                                reason: reason.clone(),
                            },
                        );
                        Err(PrivateError::OutcomeUnknown { message: reason })
                    }
                }
            }
            Err(PrivateError::OutcomeUnknown { message }) => {
                ledger.set(
                    client_order_id,
                    LedgerState::Unknown {
                        reason: message.clone(),
                    },
                );
                Err(PrivateError::OutcomeUnknown { message })
            }
            Err(e) => {
                ledger.set(
                    client_order_id,
                    LedgerState::Rejected {
                        reason: e.to_string(),
                    },
                );
                Err(e)
            }
        }
    }

    /// Resolve an `Unknown` order by listing exchange orders (read-only).
    pub async fn reconcile_order(
        &self,
        client_order_id: &str,
    ) -> Result<LedgerState, PrivateError> {
        let intent = self
            .ledger
            .lock()
            .await
            .get(client_order_id)
            .map(|(i, _)| i.clone())
            .ok_or_else(|| PrivateError::Rejected {
                status: 0,
                message: format!("unknown client order id {client_order_id}"),
            })?;
        let orders = self
            .list_orders(
                intent.side,
                ALL_ORDER_STATUSES,
                1,
                50,
                &[intent.margin_currency_short_name.as_str()],
            )
            .await?;
        let r: Reconciliation = match_order(&intent, &orders);
        Ok(self
            .ledger
            .lock()
            .await
            .apply_reconciliation(client_order_id, &r))
    }

    pub async fn ledger_snapshot(&self) -> Vec<(OrderIntent, LedgerState)> {
        self.ledger.lock().await.snapshot()
    }

    pub async fn cancel_order(&self, exchange_order_id: &str) -> Result<Ack, PrivateError> {
        let v = self
            .mutate(
                PATH_CANCEL_ORDER,
                &IdBody {
                    timestamp: request_timestamp_ms(),
                    id: exchange_order_id.into(),
                },
            )
            .await?;
        decode(v)
    }

    /// Attach exchange-side protection. Callers must treat anything other than
    /// `fully_protected()` as a protection failure and trigger the emergency exit.
    pub async fn create_tpsl(
        &self,
        position_id: &str,
        take_profit: f64,
        stop_loss: f64,
    ) -> Result<TpslResult, PrivateError> {
        if !(take_profit.is_finite()
            && stop_loss.is_finite()
            && take_profit > 0.0
            && stop_loss > 0.0)
        {
            return Err(PrivateError::Rejected {
                status: 0,
                message: "TP/SL must be positive finite prices (local check)".into(),
            });
        }
        let v = self
            .mutate(
                PATH_CREATE_TPSL,
                &CreateTpslBody::new(request_timestamp_ms(), position_id, take_profit, stop_loss),
            )
            .await?;
        Ok(TpslResult::from_value(&v))
    }

    /// Quick exit of the whole position (docs: positions/exit). An unknown
    /// outcome must be followed by `positions_filtered(.., Some(position_id))`
    /// before sending another exit.
    pub async fn exit_position(&self, position_id: &str) -> Result<Ack, PrivateError> {
        let v = self
            .mutate(
                PATH_EXIT_POSITION,
                &IdBody {
                    timestamp: request_timestamp_ms(),
                    id: position_id.into(),
                },
            )
            .await?;
        decode(v)
    }

    pub async fn cancel_all_for_position(&self, position_id: &str) -> Result<Ack, PrivateError> {
        let v = self
            .mutate(
                PATH_CANCEL_ALL_FOR_POSITION,
                &IdBody {
                    timestamp: request_timestamp_ms(),
                    id: position_id.into(),
                },
            )
            .await?;
        decode(v)
    }
}

fn decode<T: DeserializeOwned>(v: Value) -> Result<T, PrivateError> {
    serde_json::from_value(v).map_err(|e| PrivateError::Decode {
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> PrivateClient {
        let mut c = PrivateClient::new(
            Credentials::new("k".into(), "s".into()).unwrap(),
            LiveGate::default(),
        );
        // Unroutable base: even if a gate bug let a call through, nothing reaches CoinDCX.
        c.base = "http://127.0.0.1:9".into();
        c
    }

    #[tokio::test]
    async fn every_mutation_is_gated_in_paper_mode() {
        let c = client();
        let gated = |r: Result<(), PrivateError>| matches!(r, Err(PrivateError::GateClosed { .. }));
        assert!(gated(
            c.update_leverage("B-BTC_USDT", "USDT", 5).await.map(|_| ())
        ));
        assert!(gated(
            c.create_order(
                "qe-1",
                NewOrder::market(Side::Buy, "B-BTC_USDT", 0.001, 5, "USDT")
            )
            .await
            .map(|_| ())
        ));
        assert!(gated(c.cancel_order("x").await.map(|_| ())));
        assert!(gated(c.create_tpsl("p", 2.0, 1.0).await.map(|_| ())));
        assert!(gated(c.exit_position("p").await.map(|_| ())));
        assert!(gated(c.cancel_all_for_position("p").await.map(|_| ())));
        // Gate refusal happens before the ledger, so nothing was recorded.
        assert!(c.ledger_snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn reads_fail_closed_without_network() {
        // Port 9 on localhost refuses: a Transport error, never a fabricated balance.
        let c = client();
        match c.wallets().await {
            Err(PrivateError::Transport { .. }) | Err(PrivateError::CircuitOpen { .. }) => {}
            other => panic!("{other:?}"),
        }
    }
}
