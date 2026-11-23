//! Request bodies and response records for authenticated futures endpoints.
//!
//! Field names are copied from the official docs (docs.coindcx.com, "Futures
//! End Points", re-read 2026-11-23). Where the request table and the official
//! example disagree we follow the example and say so:
//! * create-order `order_type`: table lists `market, limit, ...`; the example
//!   and every documented response use `market_order` / `limit_order`, and the
//!   live PUBLIC instrument metadata (`order_types`, fetched 2026-11-23 for
//!   B-BTC_USDT) lists `limit_order, market_order, stop_limit,
//!   take_profit_limit, stop_market, take_profit_market`. We send
//!   `market_order` / `limit_order`.
//! * create-order `price`/`stop_price`: table says Integer, example sends
//!   strings; `total_quantity`/`leverage`: example sends numbers. We mirror the
//!   example.
//! * create_tpsl `order_type`: table says ONLY `take_profit_market` and
//!   `stop_market` are supported although the example uses `*_limit`. We send
//!   only the market variants.
//! * responses mix numbers, numeric strings and `null`/"None", so every numeric
//!   field is parsed leniently into `Option<f64>`.
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

pub const PATH_WALLETS: &str = "/exchange/v1/derivatives/futures/wallets";
pub const PATH_POSITIONS: &str = "/exchange/v1/derivatives/futures/positions";
pub const PATH_LIST_ORDERS: &str = "/exchange/v1/derivatives/futures/orders";
pub const PATH_CREATE_ORDER: &str = "/exchange/v1/derivatives/futures/orders/create";
pub const PATH_CANCEL_ORDER: &str = "/exchange/v1/derivatives/futures/orders/cancel";
pub const PATH_UPDATE_LEVERAGE: &str = "/exchange/v1/derivatives/futures/positions/update_leverage";
pub const PATH_CREATE_TPSL: &str = "/exchange/v1/derivatives/futures/positions/create_tpsl";
pub const PATH_EXIT_POSITION: &str = "/exchange/v1/derivatives/futures/positions/exit";
pub const PATH_CANCEL_ALL_FOR_POSITION: &str =
    "/exchange/v1/derivatives/futures/positions/cancel_all_open_orders_for_position";

/// Every status the list-orders endpoint documents.
pub const PATH_CANCEL_ALL_OPEN_ORDERS: &str =
    "/exchange/v1/derivatives/futures/positions/cancel_all_open_orders";
/// Spot wallet balances (read-only use: withdrawal reconciliation).
pub const PATH_SPOT_BALANCES: &str = "/exchange/v1/users/balances";

pub const ALL_ORDER_STATUSES: &str =
    "open,filled,partially_filled,partially_cancelled,cancelled,rejected,untriggered";

fn de_num<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    })
}

fn de_str<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::String(s)) if s != "None" && !s.is_empty() => Some(s),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    MarketOrder,
    LimitOrder,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MarginType {
    Isolated,
    Crossed,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TimeInForce {
    GoodTillCancel,
    FillOrKill,
    ImmediateOrCancel,
}

// ---------------------------------------------------------------- requests

#[derive(Debug, Clone, Serialize)]
pub struct TimestampOnly {
    pub timestamp: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ListPositionsBody {
    pub timestamp: i64,
    pub page: String,
    pub size: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pairs: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_ids: Option<String>,
    pub margin_currency_short_name: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ListOrdersBody {
    pub timestamp: i64,
    pub status: String,
    pub side: Side,
    pub page: String,
    pub size: String,
    pub margin_currency_short_name: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewOrder {
    pub side: Side,
    pub pair: String,
    pub order_type: OrderType,
    /// Must be null for market orders (docs).
    pub price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_price: Option<String>,
    pub total_quantity: f64,
    /// Should match the position leverage; set it first via update_leverage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leverage: Option<u32>,
    pub notification: String,
    /// Must be omitted for market orders (docs NOTE).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_in_force: Option<TimeInForce>,
    pub margin_currency_short_name: String,
    pub position_margin_type: MarginType,
}

impl NewOrder {
    /// Market order with isolated margin (cross is USDT-only per docs).
    pub fn market(side: Side, pair: &str, qty: f64, leverage: u32, margin_ccy: &str) -> Self {
        Self {
            side,
            pair: pair.into(),
            order_type: OrderType::MarketOrder,
            price: None,
            stop_price: None,
            total_quantity: qty,
            leverage: Some(leverage),
            notification: "no_notification".into(),
            time_in_force: None,
            margin_currency_short_name: margin_ccy.into(),
            position_margin_type: MarginType::Isolated,
        }
    }

    /// Local validation of documented constraints before anything is signed.
    pub fn validate(&self) -> Result<(), String> {
        if !(self.total_quantity.is_finite() && self.total_quantity > 0.0) {
            return Err("total_quantity must be > 0".into());
        }
        if self.margin_currency_short_name != "USDT" && self.margin_currency_short_name != "INR" {
            return Err("margin_currency_short_name must be USDT or INR".into());
        }
        if self.position_margin_type == MarginType::Crossed
            && self.margin_currency_short_name != "USDT"
        {
            return Err("cross margin is only supported on USDT-margined futures".into());
        }
        match self.order_type {
            OrderType::MarketOrder if self.price.is_some() || self.time_in_force.is_some() => {
                Err("market orders must have null price and no time_in_force".into())
            }
            OrderType::LimitOrder if self.price.is_none() => {
                Err("limit orders need a price".into())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateOrderBody {
    pub timestamp: i64,
    pub order: NewOrder,
}

#[derive(Debug, Clone, Serialize)]
pub struct CancelAllBody {
    pub timestamp: i64,
    pub margin_currency_short_name: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IdBody {
    pub timestamp: i64,
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateLeverageBody {
    pub timestamp: i64,
    /// String per docs.
    pub leverage: String,
    pub pair: String,
    pub margin_currency_short_name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TriggerLeg {
    pub stop_price: String,
    pub order_type: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateTpslBody {
    pub timestamp: i64,
    pub id: String,
    pub take_profit: TriggerLeg,
    pub stop_loss: TriggerLeg,
}

impl CreateTpslBody {
    pub fn new(timestamp: i64, position_id: &str, take_profit: f64, stop_loss: f64) -> Self {
        Self {
            timestamp,
            id: position_id.into(),
            take_profit: TriggerLeg {
                stop_price: fmt_price(take_profit),
                order_type: "take_profit_market",
            },
            stop_loss: TriggerLeg {
                stop_price: fmt_price(stop_loss),
                order_type: "stop_market",
            },
        }
    }
}

/// Plain decimal without exponent (f64 Display never uses exponents).
pub fn fmt_price(p: f64) -> String {
    format!("{p}")
}

// --------------------------------------------------------------- responses

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct FuturesWallet {
    #[serde(default, deserialize_with = "de_str")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "de_str")]
    pub currency_short_name: Option<String>,
    #[serde(default, deserialize_with = "de_num")]
    pub balance: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub locked_balance: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub cross_order_margin: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub cross_user_margin: Option<f64>,
}

impl FuturesWallet {
    /// Docs: "Total wallet balance = balance + locked_balance".
    pub fn total(&self) -> Option<f64> {
        Some(self.balance? + self.locked_balance.unwrap_or(0.0))
    }
}

/// Docs example: `[{"currency":"BTC","balance":1.167,"locked_balance":2.1}]`.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct SpotBalance {
    #[serde(default, deserialize_with = "de_str")]
    pub currency: Option<String>,
    #[serde(default, deserialize_with = "de_num")]
    pub balance: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub locked_balance: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct FuturesPosition {
    #[serde(default, deserialize_with = "de_str")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "de_str")]
    pub pair: Option<String>,
    /// Negative for shorts.
    #[serde(default, deserialize_with = "de_num")]
    pub active_pos: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub inactive_pos_buy: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub inactive_pos_sell: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub avg_price: Option<f64>,
    /// Only meaningful for isolated margin (docs).
    #[serde(default, deserialize_with = "de_num")]
    pub liquidation_price: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub locked_margin: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub locked_user_margin: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub locked_order_margin: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub take_profit_trigger: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub stop_loss_trigger: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub leverage: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub maintenance_margin: Option<f64>,
    /// Not real-time (docs).
    #[serde(default, deserialize_with = "de_num")]
    pub mark_price: Option<f64>,
    #[serde(default, deserialize_with = "de_str")]
    pub margin_type: Option<String>,
    #[serde(default, deserialize_with = "de_str")]
    pub margin_currency_short_name: Option<String>,
    #[serde(default, deserialize_with = "de_num")]
    pub updated_at: Option<f64>,
}

impl FuturesPosition {
    pub fn is_open(&self) -> bool {
        self.active_pos.is_some_and(|q| q != 0.0)
    }
    pub fn has_pending_orders(&self) -> bool {
        self.inactive_pos_buy.is_some_and(|q| q != 0.0)
            || self.inactive_pos_sell.is_some_and(|q| q != 0.0)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct FuturesOrderRecord {
    #[serde(default, deserialize_with = "de_str")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "de_str")]
    pub pair: Option<String>,
    #[serde(default, deserialize_with = "de_str")]
    pub side: Option<String>,
    #[serde(default, deserialize_with = "de_str")]
    pub status: Option<String>,
    #[serde(default, deserialize_with = "de_str")]
    pub order_type: Option<String>,
    #[serde(default, deserialize_with = "de_num")]
    pub leverage: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub maker_fee: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub taker_fee: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub fee_amount: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub price: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub stop_price: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub avg_price: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub total_quantity: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub remaining_quantity: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub cancelled_quantity: Option<f64>,
    #[serde(default, deserialize_with = "de_str")]
    pub stage: Option<String>,
    #[serde(default, deserialize_with = "de_str")]
    pub group_id: Option<String>,
    #[serde(default, deserialize_with = "de_str")]
    pub position_margin_type: Option<String>,
    #[serde(default, deserialize_with = "de_str")]
    pub margin_currency_short_name: Option<String>,
    #[serde(default, deserialize_with = "de_num")]
    pub settlement_currency_conversion_price: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub created_at: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub updated_at: Option<f64>,
}

impl FuturesOrderRecord {
    pub fn filled_quantity(&self) -> Option<f64> {
        Some(
            self.total_quantity?
                - self.remaining_quantity.unwrap_or(0.0)
                - self.cancelled_quantity.unwrap_or(0.0),
        )
    }
}

/// `{"message":"success","status":200,"code":200,"data":{...}}`
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct Ack {
    #[serde(default, deserialize_with = "de_str")]
    pub message: Option<String>,
    #[serde(default, deserialize_with = "de_num")]
    pub status: Option<f64>,
    #[serde(default, deserialize_with = "de_num")]
    pub code: Option<f64>,
    #[serde(default)]
    pub data: Option<Value>,
}

impl Ack {
    pub fn is_success(&self) -> bool {
        self.message.as_deref() == Some("success")
            || self.status == Some(200.0)
            || self.code == Some(200.0)
    }
    /// `data.group_id` from positions/exit.
    pub fn group_id(&self) -> Option<String> {
        self.data
            .as_ref()?
            .get("group_id")?
            .as_str()
            .map(str::to_string)
    }
}

/// Outcome of one create_tpsl leg: an order record, or `{success:false,error}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum TpslLeg {
    Placed(FuturesOrderRecord),
    Failed(String),
    Missing,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TpslResult {
    pub take_profit: TpslLeg,
    pub stop_loss: TpslLeg,
}

impl TpslResult {
    pub fn from_value(v: &Value) -> Self {
        let leg = |k: &str| match v.get(k) {
            None | Some(Value::Null) => TpslLeg::Missing,
            Some(l) if l.get("success").and_then(Value::as_bool) == Some(false) => TpslLeg::Failed(
                l.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unspecified")
                    .to_string(),
            ),
            Some(l) => match serde_json::from_value::<FuturesOrderRecord>(l.clone()) {
                Ok(o) if o.id.is_some() => TpslLeg::Placed(o),
                _ => TpslLeg::Failed("unrecognised leg".into()),
            },
        };
        Self {
            take_profit: leg("take_profit"),
            stop_loss: leg("stop_loss"),
        }
    }
    /// A position is protected only when BOTH legs were accepted.
    pub fn fully_protected(&self) -> bool {
        matches!(self.take_profit, TpslLeg::Placed(_))
            && matches!(self.stop_loss, TpslLeg::Placed(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_wallet_and_position() {
        let w: Vec<FuturesWallet> = serde_json::from_str(
            r#"[{"id":"c5f039dd-4e11-4304-8f91-e9c1f62d754d","currency_short_name":"USDT",
            "balance":"6.1693226","locked_balance":"0.0","cross_order_margin":"0.0",
            "cross_user_margin":"0.68534648"}]"#,
        )
        .unwrap();
        assert_eq!(w[0].total(), Some(6.1693226));
        let p: Vec<FuturesPosition> = serde_json::from_str(
            r#"[{"id":"c7ae392e","pair":"B-BTC_USDT","active_pos":0.0,"inactive_pos_buy":0.0,
            "inactive_pos_sell":0.0,"avg_price":0.0,"liquidation_price":0.0,"locked_margin":0.0,
            "locked_user_margin":0.0,"locked_order_margin":0.0,"take_profit_trigger":null,
            "stop_loss_trigger":null,"leverage":null,"maintenance_margin":null,"mark_price":null,
            "margin_type":"crossed","settlement_currency_avg_price":0.0,
            "margin_currency_short_name":"USDT","updated_at":1709548678689}]"#,
        )
        .unwrap();
        assert!(!p[0].is_open());
        assert_eq!(p[0].leverage, None);
    }

    #[test]
    fn parses_documented_order_and_tpsl() {
        let o: Vec<FuturesOrderRecord> = serde_json::from_str(
            r#"[{"id":"ffb261ae","pair":"B-ID_USDT","side":"buy","status":"filled",
            "order_type":"market_order","leverage":10.0,"fee_amount":0.011181375,"price":0.3312,
            "avg_price":0.3313,"total_quantity":45.0,"remaining_quantity":0.0,
            "cancelled_quantity":0.0,"group_id":"None","created_at":1705565061504}]"#,
        )
        .unwrap();
        assert_eq!(o[0].filled_quantity(), Some(45.0));
        assert_eq!(o[0].group_id, None);
        let v: Value = serde_json::from_str(
            r#"{"stop_loss":{"id":"8f8ee959","pair":"B-ID_USDT","side":"sell","status":"untriggered",
            "order_type":"stop_limit","stop_price":0.271,"stage":"tpsl_exit"},
            "take_profit":{"success":false,"error":"TP already exists"}}"#,
        )
        .unwrap();
        let t = TpslResult::from_value(&v);
        assert!(matches!(t.stop_loss, TpslLeg::Placed(_)));
        assert_eq!(t.take_profit, TpslLeg::Failed("TP already exists".into()));
        assert!(!t.fully_protected());
    }

    #[test]
    fn request_bodies_match_documented_fields() {
        let o = NewOrder::market(Side::Buy, "B-BTC_USDT", 0.001, 20, "USDT");
        o.validate().unwrap();
        let v = serde_json::to_value(CreateOrderBody {
            timestamp: 1,
            order: o.clone(),
        })
        .unwrap();
        assert_eq!(v["order"]["order_type"], "market_order");
        assert_eq!(v["order"]["position_margin_type"], "isolated");
        assert!(v["order"]["price"].is_null());
        assert!(v["order"].get("time_in_force").is_none());
        let mut bad = o.clone();
        bad.time_in_force = Some(TimeInForce::GoodTillCancel);
        assert!(bad.validate().is_err());
        let mut cross_inr = NewOrder::market(Side::Sell, "B-BTC_USDT", 1.0, 5, "INR");
        cross_inr.position_margin_type = MarginType::Crossed;
        assert!(cross_inr.validate().is_err());
        assert!(NewOrder::market(Side::Buy, "B-BTC_USDT", 0.0, 5, "USDT")
            .validate()
            .is_err());

        let t = serde_json::to_value(CreateTpslBody::new(1, "pos1", 101.5, 99.0)).unwrap();
        assert_eq!(t["take_profit"]["order_type"], "take_profit_market");
        assert_eq!(t["stop_loss"]["order_type"], "stop_market");
        assert_eq!(t["stop_loss"]["stop_price"], "99");
        assert_eq!(fmt_price(0.00001234), "0.00001234");
    }

    #[test]
    fn ack_parses_exit_response() {
        let a: Ack = serde_json::from_str(
            r#"{"message":"success","status":200,"code":200,"data":{"group_id":"baf926e6B-ID_USDT1705647709"}}"#,
        )
        .unwrap();
        assert!(a.is_success());
        assert_eq!(a.group_id().as_deref(), Some("baf926e6B-ID_USDT1705647709"));
    }
}
