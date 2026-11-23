//! Private (account) socket events: join payload and event parsing.
//!
//! Docs ("Futures Sockets" -> ACCOUNT, re-read 2026-11-23): connect to
//! `wss://stream.coindcx.com` with Socket.IO (the docs require socket.io-client
//! v2, i.e. Engine.IO v3), then emit
//! `join {channelName: "coindcx", authSignature: hex(HMAC(secret, '{"channel":"coindcx"}')), apiKey}`
//! and listen for `df-position-update`, `df-order-update`, `balance-update`;
//! each handler reads `response["data"]`.
//!
//! This module owns the payload and the parsers only. The Socket.IO transport
//! (connect, ping/pong, reconnect, re-join) lives with the public socket
//! engine; it must call [`join_frame`] after every (re)connect. Private events
//! are never a prerequisite for public scanning. The frame contains the API
//! key, so [`JoinFrame`]'s Debug output is redacted: never log `.0`.
use super::types::{FuturesOrderRecord, FuturesPosition};
use crate::auth::{compact_body, Credentials};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const PRIVATE_CHANNEL: &str = "coindcx";
pub const EVENT_POSITION: &str = "df-position-update";
pub const EVENT_ORDER: &str = "df-order-update";
pub const EVENT_BALANCE: &str = "balance-update";

/// Engine.IO v3 / Socket.IO v2 text frame `42["join",{...}]`.
pub struct JoinFrame(pub String);

impl std::fmt::Debug for JoinFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JoinFrame(<redacted>)")
    }
}

pub fn auth_signature(creds: &Credentials) -> String {
    let body = compact_body(&json!({"channel": PRIVATE_CHANNEL})).expect("static json");
    creds.sign(&body)
}

pub fn join_frame(creds: &Credentials) -> JoinFrame {
    let payload = json!({
        "channelName": PRIVATE_CHANNEL,
        "authSignature": auth_signature(creds),
        "apiKey": creds.api_key(),
    });
    JoinFrame(format!("42{}", json!(["join", payload])))
}

pub fn leave_frame() -> String {
    format!("42{}", json!(["leave", {"channelName": PRIVATE_CHANNEL}]))
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
pub struct BalanceUpdate {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub balance: Option<Value>,
    #[serde(default)]
    pub locked_balance: Option<Value>,
    #[serde(default)]
    pub currency_id: Option<Value>,
    #[serde(default)]
    pub currency_short_name: Option<String>,
}

impl BalanceUpdate {
    fn num(v: &Option<Value>) -> Option<f64> {
        match v {
            Some(Value::String(s)) => s.parse().ok(),
            Some(Value::Number(n)) => n.as_f64(),
            _ => None,
        }
    }
    pub fn balance_f64(&self) -> Option<f64> {
        Self::num(&self.balance)
    }
    pub fn locked_f64(&self) -> Option<f64> {
        Self::num(&self.locked_balance)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", content = "records", rename_all = "snake_case")]
pub enum PrivateEvent {
    Positions(Vec<FuturesPosition>),
    Orders(Vec<FuturesOrderRecord>),
    Balances(Vec<BalanceUpdate>),
}

/// Parse one private event. `response` is the Socket.IO event argument; its
/// `data` may be an array or a JSON-encoded string of one.
pub fn parse_event(event: &str, response: &Value) -> Result<PrivateEvent, String> {
    let data = response.get("data").unwrap_or(response);
    let data: Value = match data {
        Value::String(s) => serde_json::from_str(s).map_err(|e| format!("data string: {e}"))?,
        v => v.clone(),
    };
    let data = match data {
        Value::Array(_) => data,
        v @ Value::Object(_) => Value::Array(vec![v]),
        other => return Err(format!("unexpected data: {other}")),
    };
    let e = |r: serde_json::Error| r.to_string();
    match event {
        EVENT_POSITION => serde_json::from_value(data)
            .map(PrivateEvent::Positions)
            .map_err(e),
        EVENT_ORDER => serde_json::from_value(data)
            .map(PrivateEvent::Orders)
            .map_err(e),
        EVENT_BALANCE => serde_json::from_value(data)
            .map(PrivateEvent::Balances)
            .map_err(e),
        other => Err(format!("not a private event: {other}")),
    }
}

/// Split an Engine.IO v3 `42[...]` text frame into (event, argument).
pub fn split_frame(frame: &str) -> Option<(String, Value)> {
    let v: Value = serde_json::from_str(frame.strip_prefix("42")?).ok()?;
    let a = v.as_array()?;
    Some((
        a.first()?.as_str()?.to_string(),
        a.get(1).cloned().unwrap_or(Value::Null),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_frame_matches_documented_signature() {
        let c = Credentials::new("KEY".into(), "YYYY".into()).unwrap();
        assert_eq!(
            auth_signature(&c),
            "c8f94ab0ff0372da5b20dfa616c8a1abd06341fa2c0ea19559decc4c8485b989"
        );
        let f = join_frame(&c);
        assert_eq!(format!("{f:?}"), "JoinFrame(<redacted>)");
        let (ev, arg) = split_frame(&f.0).unwrap();
        assert_eq!(ev, "join");
        assert_eq!(arg["channelName"], "coindcx");
        assert_eq!(arg["apiKey"], "KEY");
        assert_eq!(split_frame(&leave_frame()).unwrap().0, "leave");
    }

    #[test]
    fn parses_documented_events() {
        let pos = json!({"data": [{"id":"571eae12","pair":"B-BNB_USDT","active_pos":0,
            "inactive_pos_buy":0,"inactive_pos_sell":0,"avg_price":0,"liquidation_price":0,
            "leverage":10,"margin_type":"isolated","margin_currency_short_name":"INR",
            "updated_at":1717754279737_i64}]});
        match parse_event(EVENT_POSITION, &pos).unwrap() {
            PrivateEvent::Positions(p) => assert_eq!(p[0].leverage, Some(10.0)),
            e => panic!("{e:?}"),
        }
        let ord = json!({"data": r#"[{"id":"ff5a645f","pair":"B-ID_USDT","side":"sell","status":"cancelled",
            "order_type":"take_profit_limit","stop_price":1,"trades":[],"group_id":null}]"#});
        match parse_event(EVENT_ORDER, &ord).unwrap() {
            PrivateEvent::Orders(o) => assert_eq!(o[0].status.as_deref(), Some("cancelled")),
            e => panic!("{e:?}"),
        }
        let bal = json!({"data": [{"id":"12345","balance":"1.0221449","locked_balance":"0.99478995",
            "currency_id":"123","currency_short_name":"USDT"}]});
        match parse_event(EVENT_BALANCE, &bal).unwrap() {
            PrivateEvent::Balances(b) => assert_eq!(b[0].balance_f64(), Some(1.0221449)),
            e => panic!("{e:?}"),
        }
        assert!(parse_event("new-trade", &bal).is_err());
    }
}
