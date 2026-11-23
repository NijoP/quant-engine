//! Engine.IO v4 / Socket.IO v5 text-frame codec, limited to what the CoinDCX
//! public stream uses.
//!
//! Verified live against `wss://stream.coindcx.com/socket.io/?EIO=4&transport=websocket`
//! on 2026-11-23: the server opens with `0{"sid":..,"upgrades":[],"pingInterval":25000,
//! "pingTimeout":20000,"maxPayload":1000000}`, the client connects the default namespace
//! with `40`, the server acks with `40{"sid":..}`, channels are joined with
//! `42["join",{"channelName":"<channel>"}]`, the server sends Engine.IO ping `2` and the
//! client must answer pong `3`. Every market event arrives as
//! `42["<event>",{"event":"<event>","data":"<JSON string>"}]` where `data` is itself a
//! JSON document encoded as a string.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

/// Engine.IO `open` packet payload.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenInfo {
    pub sid: String,
    #[serde(default)]
    pub upgrades: Vec<String>,
    pub ping_interval: u64,
    pub ping_timeout: u64,
    #[serde(default)]
    pub max_payload: u64,
}

/// A decoded text frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    /// Engine.IO open (`0{...}`).
    Open(OpenInfo),
    /// Engine.IO close (`1`).
    Close,
    /// Engine.IO ping (`2`); answer with [`PONG`].
    Ping,
    /// Engine.IO pong (`3`).
    Pong,
    /// Engine.IO noop (`6`).
    Noop,
    /// Socket.IO connect ack for the default namespace (`40{...}`).
    SioConnect,
    /// Socket.IO disconnect (`41`).
    SioDisconnect,
    /// Socket.IO connect error (`44{...}`).
    SioConnectError(String),
    /// Socket.IO event (`42[...]`), name plus the first argument.
    Event { name: String, arg: Value },
    /// Anything else (binary events, upgrades, other namespaces).
    Other(String),
}

/// Engine.IO pong frame.
pub const PONG: &str = "3";
/// Socket.IO connect to the default namespace.
pub const SIO_CONNECT: &str = "40";
/// Socket.IO path appended to the stream endpoint.
pub const SOCKET_IO_PATH: &str = "/socket.io/?EIO=4&transport=websocket";

/// Decode one text frame.
pub fn decode(frame: &str) -> Result<Frame> {
    let Some(eio) = frame.chars().next() else {
        bail!("empty frame")
    };
    let rest = &frame[eio.len_utf8()..];
    Ok(match eio {
        '0' => Frame::Open(serde_json::from_str(rest).context("engine.io open payload")?),
        '1' => Frame::Close,
        '2' => Frame::Ping,
        '3' => Frame::Pong,
        '6' => Frame::Noop,
        '4' => decode_sio(rest)?,
        _ => Frame::Other(frame.to_string()),
    })
}

fn decode_sio(s: &str) -> Result<Frame> {
    let Some(kind) = s.chars().next() else {
        bail!("empty socket.io packet")
    };
    let mut body = &s[kind.len_utf8()..];
    // Optional namespace: "/ns," (we only use the default namespace "/").
    if body.starts_with('/') {
        match body.find(',') {
            Some(i) => {
                if &body[..i] != "/" {
                    return Ok(Frame::Other(format!("4{s}")));
                }
                body = &body[i + 1..];
            }
            None => body = "",
        }
    }
    Ok(match kind {
        '0' => Frame::SioConnect,
        '1' => Frame::SioDisconnect,
        '4' => Frame::SioConnectError(body.to_string()),
        '2' => {
            // Optional ack id digits before the JSON array.
            let body = body.trim_start_matches(|c: char| c.is_ascii_digit());
            let v: Value = serde_json::from_str(body).context("socket.io event array")?;
            let arr = v
                .as_array()
                .ok_or_else(|| anyhow!("socket.io event is not an array"))?;
            let name = arr
                .first()
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("socket.io event without a name"))?
                .to_string();
            let arg = arr.get(1).cloned().unwrap_or(Value::Null);
            Frame::Event { name, arg }
        }
        _ => Frame::Other(format!("4{s}")),
    })
}

/// The `data` member of a CoinDCX event argument, decoded. CoinDCX sends it as a
/// JSON string; a direct object is accepted too.
pub fn event_data(arg: &Value) -> Result<Value> {
    match arg.get("data") {
        Some(Value::String(s)) => serde_json::from_str(s).context("event data string is not JSON"),
        Some(v) => Ok(v.clone()),
        None => bail!("event argument has no data member"),
    }
}

/// The raw `data` text, used for duplicate detection.
pub fn event_data_raw(arg: &Value) -> Option<&str> {
    arg.get("data").and_then(Value::as_str)
}

/// `42["join",{"channelName":"<channel>"}]` (public channels need no signature).
pub fn encode_join(channel: &str) -> String {
    format!(
        "42{}",
        serde_json::json!(["join", { "channelName": channel }])
    )
}

/// `42["leave",{"channelName":"<channel>"}]`.
pub fn encode_leave(channel: &str) -> String {
    format!(
        "42{}",
        serde_json::json!(["leave", { "channelName": channel }])
    )
}

/// Application-level keepalive shown in the official sample code
/// (`sio.emit('ping', {'data': 'Ping message'})` every 25 s).
pub fn encode_app_ping() -> String {
    format!(
        "42{}",
        serde_json::json!(["ping", { "data": "Ping message" }])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_live_handshake() {
        let f = decode(r#"0{"sid":"2rKp_janQD2dXIxTAKS2","upgrades":[],"pingInterval":25000,"pingTimeout":20000,"maxPayload":1000000}"#).unwrap();
        match f {
            Frame::Open(o) => {
                assert_eq!(o.ping_interval, 25000);
                assert_eq!(o.ping_timeout, 20000);
                assert_eq!(o.max_payload, 1_000_000);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            decode(r#"40{"sid":"Lvw0ju9XrXelsDdDAIU5"}"#).unwrap(),
            Frame::SioConnect
        );
        assert_eq!(decode("2").unwrap(), Frame::Ping);
        assert_eq!(decode("41").unwrap(), Frame::SioDisconnect);
    }

    #[test]
    fn decodes_event_with_string_data() {
        let f = decode(r#"42["price-change",{"event":"price-change","data":"{\"T\":1795429282716,\"p\":\"61325.8\",\"pr\":\"f\"}"}]"#).unwrap();
        let Frame::Event { name, arg } = f else {
            panic!()
        };
        assert_eq!(name, "price-change");
        let d = event_data(&arg).unwrap();
        assert_eq!(d["p"], "61325.8");
        assert_eq!(d["T"], 1795429282716i64);
    }

    #[test]
    fn decodes_event_with_ack_id_and_namespace() {
        let f = decode(r#"4212["x",{"data":{"a":1}}]"#).unwrap();
        assert!(matches!(f, Frame::Event { ref name, .. } if name == "x"));
        let f = decode(r#"42/,["y",{}]"#).unwrap();
        assert!(matches!(f, Frame::Event { ref name, .. } if name == "y"));
        assert!(matches!(
            decode(r#"42/admin,["z",{}]"#).unwrap(),
            Frame::Other(_)
        ));
    }

    #[test]
    fn encodes_join_exactly_as_documented() {
        assert_eq!(
            encode_join("B-ID_USDT@trades-futures"),
            r#"42["join",{"channelName":"B-ID_USDT@trades-futures"}]"#
        );
        assert_eq!(
            encode_leave("currentPrices@futures@rt"),
            r#"42["leave",{"channelName":"currentPrices@futures@rt"}]"#
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode("").is_err());
        assert!(decode("42not-json").is_err());
        assert!(decode("0{").is_err());
    }
}
