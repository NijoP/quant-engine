//! CoinDCX futures order adapter — LIVE SUBMISSION DISABLED.
//!
//! Request bodies follow the documented schemas (docs.coindcx.com, 2026-11-23):
//! * POST /exchange/v1/derivatives/futures/orders/create  {timestamp, order:{...}}
//! * POST /exchange/v1/derivatives/futures/positions/update_leverage
//!   {timestamp, leverage (string), pair, margin_currency_short_name}
//! * POST /exchange/v1/derivatives/futures/positions/create_tpsl
//!   {timestamp, id, take_profit:{stop_price, order_type:"take_profit_market"},
//!   stop_loss:{stop_price, order_type:"stop_market"}}
//!   (docs table: only take_profit_market / stop_market supported; examples differ.)
//!
//! These sprint-1 body builders are kept for compatibility; the signed client
//! that actually sends orders is `crate::private::PrivateClient`, and every
//! mutation there passes `crate::live::LiveGate::check()` first.
use crate::auth::request_timestamp_ms;
use serde::Serialize;

pub const PATH_CREATE_ORDER: &str = "/exchange/v1/derivatives/futures/orders/create";
pub const PATH_UPDATE_LEVERAGE: &str = "/exchange/v1/derivatives/futures/positions/update_leverage";
pub const PATH_CREATE_TPSL: &str = "/exchange/v1/derivatives/futures/positions/create_tpsl";

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FuturesOrder {
    pub side: Side,
    pub pair: String,
    pub order_type: String, // "market_order" (as in official examples)
    pub price: Option<String>,
    pub stop_price: Option<String>,
    pub total_quantity: String,
    pub leverage: String,
    pub notification: String,
    pub margin_currency_short_name: String,
    pub position_margin_type: String, // "isolated" preferred
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateOrderBody {
    pub timestamp: i64,
    pub order: FuturesOrder,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateLeverageBody {
    pub timestamp: i64,
    pub leverage: String,
    pub pair: String,
    pub margin_currency_short_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TriggerLeg {
    pub stop_price: String,
    pub order_type: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateTpslBody {
    pub timestamp: i64,
    pub id: String,
    pub take_profit: TriggerLeg,
    pub stop_loss: TriggerLeg,
}

impl UpdateLeverageBody {
    pub fn new(pair: &str, margin: &str, leverage: f64) -> Self {
        Self {
            timestamp: request_timestamp_ms(),
            leverage: format!("{}", leverage as i64),
            pair: pair.into(),
            margin_currency_short_name: margin.into(),
        }
    }
}

impl CreateTpslBody {
    pub fn new(position_id: &str, tp: f64, sl: f64) -> Self {
        Self {
            timestamp: request_timestamp_ms(),
            id: position_id.into(),
            take_profit: TriggerLeg {
                stop_price: tp.to_string(),
                order_type: "take_profit_market".into(),
            },
            stop_loss: TriggerLeg {
                stop_price: sl.to_string(),
                order_type: "stop_market".into(),
            },
        }
    }
}

/// Deterministic internal client order id (FNV-1a 64 of strategy|pair|seq).
/// CoinDCX futures docs do not document `client_order_id`, so this id is used
/// for local tracking / reconciliation and is not sent to the exchange.
pub fn client_order_id(strategy: &str, pair: &str, seq: u64) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in strategy
        .bytes()
        .chain(*b"|")
        .chain(pair.bytes())
        .chain(*b"|")
        .chain(seq.to_le_bytes())
    {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("qe-{h:016x}")
}

/// The live gate moved to `crate::live` (owned by the live-execution work);
/// re-exported here so existing callers keep compiling.
pub use crate::live::{GateError, LiveGate};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deterministic_ids() {
        assert_eq!(
            client_order_id("s", "B-BTC_USDT", 1),
            client_order_id("s", "B-BTC_USDT", 1)
        );
        assert_ne!(
            client_order_id("s", "B-BTC_USDT", 1),
            client_order_id("s", "B-BTC_USDT", 2)
        );
        assert!(client_order_id("s", "p", 0).len() <= 36);
    }
    #[test]
    fn bodies_serialise_to_documented_fields() {
        let b = UpdateLeverageBody::new("B-BTC_USDT", "USDT", 20.0);
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["leverage"], "20");
        assert_eq!(v["margin_currency_short_name"], "USDT");
        let t = serde_json::to_value(CreateTpslBody::new("pos1", 101.5, 99.0)).unwrap();
        assert_eq!(t["take_profit"]["order_type"], "take_profit_market");
        assert_eq!(t["stop_loss"]["order_type"], "stop_market");
        let o = CreateOrderBody {
            timestamp: 1,
            order: FuturesOrder {
                side: Side::Buy,
                pair: "B-BTC_USDT".into(),
                order_type: "market_order".into(),
                price: None,
                stop_price: None,
                total_quantity: "0.001".into(),
                leverage: "20".into(),
                notification: "no_notification".into(),
                margin_currency_short_name: "USDT".into(),
                position_margin_type: "isolated".into(),
            },
        };
        let v = serde_json::to_value(o).unwrap();
        assert_eq!(v["order"]["side"], "buy");
        assert!(v["order"]["price"].is_null());
    }
}
