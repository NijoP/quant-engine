//! Typed CoinDCX futures stream channels and event payloads.
//!
//! Field names and units below are exactly those observed on the live stream on
//! 2026-11-23 (captures in `fixtures/`), cross-checked with the "Futures Sockets"
//! section of https://docs.coindcx.com/. Where the live stream differs from the
//! documentation the live behaviour wins and the difference is noted.

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// Order-book depth accepted by `{instrument}@orderbook@{depth}-futures` (docs: 10, 20, 50).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum Depth {
    D10,
    D20,
    D50,
}

impl Depth {
    pub fn levels(self) -> usize {
        match self {
            Depth::D10 => 10,
            Depth::D20 => 20,
            Depth::D50 => 50,
        }
    }
}

/// Candle resolutions listed in the docs for `{instrument}_{interval}-futures`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum CandleInterval {
    M1,
    M5,
    M15,
    M30,
    H1,
    H4,
    H8,
    D1,
    D3,
    W1,
    Mo1,
}

impl CandleInterval {
    pub const ALL: [CandleInterval; 11] = [
        Self::M1,
        Self::M5,
        Self::M15,
        Self::M30,
        Self::H1,
        Self::H4,
        Self::H8,
        Self::D1,
        Self::D3,
        Self::W1,
        Self::Mo1,
    ];

    /// The exchange's spelling (`1m`, ..., `1M`).
    pub fn code(self) -> &'static str {
        match self {
            Self::M1 => "1m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::M30 => "30m",
            Self::H1 => "1h",
            Self::H4 => "4h",
            Self::H8 => "8h",
            Self::D1 => "1d",
            Self::D3 => "3d",
            Self::W1 => "1w",
            Self::Mo1 => "1M",
        }
    }

    pub fn from_code(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|i| i.code() == s)
    }
}

/// A public futures channel. `instrument` is the futures pair as returned by
/// `active_instruments`, e.g. `B-BTC_USDT`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum Channel {
    /// `{instrument}@trades-futures`, event `new-trade`.
    Trades(String),
    /// `{instrument}@prices-futures`, event `price-change` (see [`PriceTick`] caveat).
    Prices(String),
    /// `{instrument}@orderbook@{depth}-futures`, events `depth-snapshot` + `depth-update`.
    OrderBook(String, Depth),
    /// `{instrument}_{interval}-futures`, event `candlestick`.
    Candles(String, CandleInterval),
    /// `currentPrices@futures@rt`, events `currentPrices@futures#snapshot` / `#update`.
    CurrentPrices,
}

pub const CURRENT_PRICES_CHANNEL: &str = "currentPrices@futures@rt";

impl Channel {
    /// The exact channel name sent in `join`.
    pub fn name(&self) -> String {
        match self {
            Channel::Trades(i) => format!("{i}@trades-futures"),
            Channel::Prices(i) => format!("{i}@prices-futures"),
            Channel::OrderBook(i, d) => format!("{i}@orderbook@{}-futures", d.levels()),
            Channel::Candles(i, iv) => format!("{i}_{}-futures", iv.code()),
            Channel::CurrentPrices => CURRENT_PRICES_CHANNEL.to_string(),
        }
    }

    pub fn instrument(&self) -> Option<&str> {
        match self {
            Channel::Trades(i)
            | Channel::Prices(i)
            | Channel::OrderBook(i, _)
            | Channel::Candles(i, _) => Some(i),
            Channel::CurrentPrices => None,
        }
    }

    pub fn kind(&self) -> ChannelKind {
        match self {
            Channel::Trades(_) => ChannelKind::Trades,
            Channel::Prices(_) => ChannelKind::Prices,
            Channel::OrderBook(..) => ChannelKind::OrderBook,
            Channel::Candles(..) => ChannelKind::Candles,
            Channel::CurrentPrices => ChannelKind::CurrentPrices,
        }
    }

    /// Every public channel for one instrument (trades, prices, book, 1m candles).
    pub fn all_for(instrument: &str, depth: Depth) -> Vec<Channel> {
        vec![
            Channel::Trades(instrument.to_string()),
            Channel::Prices(instrument.to_string()),
            Channel::OrderBook(instrument.to_string(), depth),
            Channel::Candles(instrument.to_string(), CandleInterval::M1),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
pub enum ChannelKind {
    Trades,
    Prices,
    OrderBook,
    Candles,
    CurrentPrices,
}

/// Depth messages identify the instrument only by its exchange symbol
/// (`s: "BTCUSDT"` for `B-BTC_USDT`). This is the mapping used to attribute them.
pub fn depth_symbol(instrument: &str) -> String {
    let core = instrument
        .split_once('-')
        .map(|(_, r)| r)
        .unwrap_or(instrument);
    core.replace('_', "")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Side {
    Buy,
    Sell,
}

/// `new-trade`: `{"T":ms,"RT":..,"p":"61326.1","q":"0.002","m":1,"s":"B-BTC_USDT","pr":"f"}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trade {
    pub instrument: String,
    pub price: f64,
    pub quantity: f64,
    /// `T`, exchange trade time, ms since epoch.
    pub exchange_ts_ms: i64,
    /// `m`. Docs: "is maker". Empirically (2026-11-23, 719 BTC/ETH trades matched to
    /// the live book) `m=1` prints at the bid and `m=0` at the ask in ~97% of cases,
    /// i.e. `m` means *buyer is maker* (Binance convention).
    pub buyer_is_maker: bool,
    /// Local receive time, ms since epoch.
    pub recv_ts_ms: i64,
}

impl Trade {
    /// Aggressor side derived from `m` (see field note).
    pub fn aggressor(&self) -> Side {
        if self.buyer_is_maker {
            Side::Sell
        } else {
            Side::Buy
        }
    }
}

/// `price-change`: `{"T":ms,"p":"61325.8","pr":"f"}`.
///
/// The payload carries **no instrument identifier**. It can only be attributed when
/// exactly one `@prices-futures` channel is joined on the connection; otherwise
/// `instrument` is `None` and consumers should take last price from [`Trade`]s.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PriceTick {
    pub instrument: Option<String>,
    pub price: f64,
    pub exchange_ts_ms: i64,
    pub recv_ts_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DepthKind {
    Snapshot,
    Update,
}

/// `depth-snapshot` / `depth-update`.
///
/// Live semantics (differs from the docs FAQ, which says "snapshot updates only"):
/// the channel sends full `depth-snapshot`s (exactly `depth` levels a side) every
/// ~0.5–3 s, interleaved with incremental `depth-update`s whose quantities replace
/// the level and `"0.0"` deletes it. Snapshots and updates share one per-instrument
/// version `vs` that increases by exactly 1 per message.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DepthMsg {
    pub kind: DepthKind,
    /// `s`, exchange symbol, e.g. `BTCUSDT`.
    pub symbol: String,
    /// `vs`.
    pub version: u64,
    /// `ts`, ms.
    pub exchange_ts_ms: i64,
    /// `E`, upstream event time, ms (updates only).
    pub event_ts_ms: Option<i64>,
    /// `pts`, publish time, ms.
    pub publish_ts_ms: Option<i64>,
    /// (price, quantity), unordered as received.
    pub bids: Vec<(f64, f64)>,
    pub asks: Vec<(f64, f64)>,
    pub recv_ts_ms: i64,
}

/// One instrument entry of `currentPrices@futures#snapshot|#update`. Updates are
/// partial: only fields that changed are present (some updates carry only `mp`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PriceEntry {
    /// `mp` mark price.
    pub mark_price: Option<f64>,
    /// `ls` last price.
    pub last_price: Option<f64>,
    /// `v` 24h volume.
    pub volume_24h: Option<f64>,
    /// `pc` 24h change, percent.
    pub change_pct_24h: Option<f64>,
    /// `h` / `l` 24h high / low (snapshot).
    pub high_24h: Option<f64>,
    pub low_24h: Option<f64>,
    /// `fr` funding rate, `efr` estimated funding rate (snapshot).
    pub funding_rate: Option<f64>,
    pub est_funding_rate: Option<f64>,
    /// `btST` upstream tick send time, `bmST` upstream mark-price send time, ms.
    pub tick_send_ts_ms: Option<i64>,
    pub mark_send_ts_ms: Option<i64>,
}

impl PriceEntry {
    /// Overlay the fields present in `upd`.
    pub fn merge(&mut self, upd: &PriceEntry) {
        macro_rules! m {
            ($($f:ident),*) => { $( if upd.$f.is_some() { self.$f = upd.$f; } )* };
        }
        m!(
            mark_price,
            last_price,
            volume_24h,
            change_pct_24h,
            high_24h,
            low_24h,
            funding_rate,
            est_funding_rate,
            tick_send_ts_ms,
            mark_send_ts_ms
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PricesKind {
    Snapshot,
    Update,
}

/// `currentPrices@futures#snapshot|#update` (all futures instruments, ~1 s cadence).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CurrentPrices {
    pub kind: PricesKind,
    /// `vs`, +1 per message.
    pub version: u64,
    /// `ts`, ms.
    pub exchange_ts_ms: i64,
    /// `pST`, ms (updates).
    pub send_ts_ms: Option<i64>,
    pub prices: BTreeMap<String, PriceEntry>,
    pub recv_ts_ms: i64,
}

/// `candlestick` on `{instrument}_{interval}-futures`. The stream pushes the
/// **in-progress** candle repeatedly; it has no "closed" flag. A candle is complete
/// only once [`Candle::is_complete_at`] holds (its period has ended).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candle {
    pub channel: String,
    pub instrument: String,
    pub interval: Option<CandleInterval>,
    /// `open_time` is seconds on the wire; converted to ms here.
    pub open_time_ms: i64,
    /// `close_time` is fractional seconds on the wire (`...319.999`); ms here.
    pub close_time_ms: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub quote_volume: Option<f64>,
    /// `Ets`, ms.
    pub exchange_ts_ms: i64,
    pub recv_ts_ms: i64,
}

impl Candle {
    pub fn is_complete_at(&self, now_ms: i64) -> bool {
        now_ms > self.close_time_ms
    }
}

/// A decoded market event, before order-book reconstruction.
#[derive(Debug, Clone, PartialEq)]
pub enum MarketMsg {
    Trade(Trade),
    Price(PriceTick),
    Depth(DepthMsg),
    CurrentPrices(CurrentPrices),
    Candle(Candle),
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.parse::<f64>().ok().map(|f| f as i64),
        _ => None,
    }
}

fn req_num(d: &Value, k: &str) -> Result<f64> {
    d.get(k)
        .and_then(num)
        .ok_or_else(|| anyhow!("missing/invalid numeric field {k}"))
}

fn req_int(d: &Value, k: &str) -> Result<i64> {
    d.get(k)
        .and_then(int)
        .ok_or_else(|| anyhow!("missing/invalid integer field {k}"))
}

fn levels(d: &Value, k: &str) -> Result<Vec<(f64, f64)>> {
    let Some(obj) = d.get(k) else {
        return Ok(Vec::new());
    };
    let obj = obj
        .as_object()
        .ok_or_else(|| anyhow!("{k} is not an object"))?;
    obj.iter()
        .map(|(p, q)| {
            let price: f64 = p.parse().with_context(|| format!("{k} price {p}"))?;
            let qty = num(q).ok_or_else(|| anyhow!("{k} qty for {p}"))?;
            if !price.is_finite() || !qty.is_finite() || qty < 0.0 || price <= 0.0 {
                bail!("{k} level {p}={qty} invalid");
            }
            Ok((price, qty))
        })
        .collect()
}

fn price_entry(v: &Value) -> PriceEntry {
    let f = |k: &str| v.get(k).and_then(num);
    let i = |k: &str| v.get(k).and_then(int);
    PriceEntry {
        mark_price: f("mp"),
        last_price: f("ls"),
        volume_24h: f("v"),
        change_pct_24h: f("pc"),
        high_24h: f("h"),
        low_24h: f("l"),
        funding_rate: f("fr"),
        est_funding_rate: f("efr"),
        tick_send_ts_ms: i("btST"),
        mark_send_ts_ms: i("bmST"),
    }
}

/// Decode one event's `data` document. Returns `Ok(None)` for events that are not
/// market data (unknown names are ignored, never guessed).
pub fn parse_event(name: &str, d: &Value, recv_ts_ms: i64) -> Result<Option<MarketMsg>> {
    Ok(Some(match name {
        "new-trade" => MarketMsg::Trade(Trade {
            instrument: d
                .get("s")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("trade without s"))?
                .to_string(),
            price: req_num(d, "p")?,
            quantity: req_num(d, "q")?,
            exchange_ts_ms: req_int(d, "T")?,
            buyer_is_maker: match d.get("m") {
                Some(Value::Bool(b)) => *b,
                Some(v) => int(v).ok_or_else(|| anyhow!("trade m invalid"))? != 0,
                None => bail!("trade without m"),
            },
            recv_ts_ms,
        }),
        "price-change" => MarketMsg::Price(PriceTick {
            instrument: None,
            price: req_num(d, "p")?,
            exchange_ts_ms: req_int(d, "T")?,
            recv_ts_ms,
        }),
        "depth-snapshot" | "depth-update" => MarketMsg::Depth(DepthMsg {
            kind: if name == "depth-snapshot" {
                DepthKind::Snapshot
            } else {
                DepthKind::Update
            },
            symbol: d
                .get("s")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("depth without s"))?
                .to_string(),
            version: req_int(d, "vs")?
                .try_into()
                .context("negative depth version")?,
            exchange_ts_ms: req_int(d, "ts")?,
            event_ts_ms: d.get("E").and_then(int),
            publish_ts_ms: d.get("pts").and_then(int),
            bids: levels(d, "bids")?,
            asks: levels(d, "asks")?,
            recv_ts_ms,
        }),
        "currentPrices@futures#snapshot" | "currentPrices@futures#update" => {
            let prices = d
                .get("prices")
                .and_then(Value::as_object)
                .ok_or_else(|| anyhow!("currentPrices without prices"))?
                .iter()
                .map(|(k, v)| (k.clone(), price_entry(v)))
                .collect();
            MarketMsg::CurrentPrices(CurrentPrices {
                kind: if name.ends_with("#snapshot") {
                    PricesKind::Snapshot
                } else {
                    PricesKind::Update
                },
                version: req_int(d, "vs")?
                    .try_into()
                    .context("negative prices version")?,
                exchange_ts_ms: req_int(d, "ts")?,
                send_ts_ms: d.get("pST").and_then(int),
                prices,
                recv_ts_ms,
            })
        }
        "candlestick" => {
            let c = d
                .get("data")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .ok_or_else(|| anyhow!("candlestick without data[0]"))?;
            let channel = d
                .get("channel")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let open_s = c
                .get("open_time")
                .and_then(num)
                .ok_or_else(|| anyhow!("candle open_time"))?;
            let close_s = c
                .get("close_time")
                .and_then(num)
                .ok_or_else(|| anyhow!("candle close_time"))?;
            MarketMsg::Candle(Candle {
                instrument: c
                    .get("pair")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("candle pair"))?
                    .to_string(),
                interval: d
                    .get("i")
                    .or_else(|| c.get("duration"))
                    .and_then(Value::as_str)
                    .and_then(CandleInterval::from_code),
                channel,
                open_time_ms: (open_s * 1000.0).round() as i64,
                close_time_ms: (close_s * 1000.0).round() as i64,
                open: req_num(c, "open")?,
                high: req_num(c, "high")?,
                low: req_num(c, "low")?,
                close: req_num(c, "close")?,
                volume: req_num(c, "volume")?,
                quote_volume: c.get("quote_volume").and_then(num),
                exchange_ts_ms: req_int(d, "Ets")?,
                recv_ts_ms,
            })
        }
        _ => return Ok(None),
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ws::protocol::{decode, event_data, Frame};

    pub(crate) fn parse_line(line: &str) -> Option<MarketMsg> {
        let Frame::Event { name, arg } = decode(line).unwrap() else {
            return None;
        };
        parse_event(&name, &event_data(&arg).unwrap(), 0).unwrap()
    }

    #[test]
    fn channel_names_match_docs() {
        let i = "B-ID_USDT";
        assert_eq!(Channel::Trades(i.into()).name(), "B-ID_USDT@trades-futures");
        assert_eq!(Channel::Prices(i.into()).name(), "B-ID_USDT@prices-futures");
        assert_eq!(
            Channel::OrderBook(i.into(), Depth::D50).name(),
            "B-ID_USDT@orderbook@50-futures"
        );
        assert_eq!(
            Channel::Candles("B-BTC_USDT".into(), CandleInterval::M1).name(),
            "B-BTC_USDT_1m-futures"
        );
        assert_eq!(
            Channel::Candles(i.into(), CandleInterval::Mo1).name(),
            "B-ID_USDT_1M-futures"
        );
        assert_eq!(Channel::CurrentPrices.name(), "currentPrices@futures@rt");
        assert_eq!(depth_symbol("B-BTC_USDT"), "BTCUSDT");
        assert_eq!(depth_symbol("B-1000PEPE_USDT"), "1000PEPEUSDT");
    }

    #[test]
    fn parses_captured_trades_prices_candles() {
        let fx = include_str!("fixtures/stream_basic.txt");
        let msgs: Vec<_> = fx.lines().filter_map(parse_line).collect();
        let trades: Vec<_> = msgs
            .iter()
            .filter_map(|m| match m {
                MarketMsg::Trade(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(trades.len(), 3);
        assert_eq!(trades[0].instrument, "B-BTC_USDT");
        assert!(trades[0].exchange_ts_ms > 1_700_000_000_000);
        assert!(trades[0].price > 1000.0 && trades[0].quantity > 0.0);
        assert_eq!(
            trades[0].aggressor(),
            if trades[0].buyer_is_maker {
                Side::Sell
            } else {
                Side::Buy
            }
        );
        assert!(!trades[1].buyer_is_maker);
        assert_eq!(trades[1].aggressor(), Side::Buy);

        let price = msgs
            .iter()
            .find_map(|m| match m {
                MarketMsg::Price(p) => Some(p),
                _ => None,
            })
            .unwrap();
        assert!(price.instrument.is_none());
        assert!(price.price > 0.0);

        let c = msgs
            .iter()
            .find_map(|m| match m {
                MarketMsg::Candle(c) => Some(c),
                _ => None,
            })
            .unwrap();
        assert_eq!(c.channel, format!("{}_1m-futures", c.instrument));
        assert_eq!(c.interval, Some(CandleInterval::M1));
        assert_eq!(c.open_time_ms % 60_000, 0);
        assert_eq!(c.close_time_ms, c.open_time_ms + 59_999);
        assert!(c.low <= c.open && c.open <= c.high && c.low <= c.close && c.close <= c.high);
        assert!(!c.is_complete_at(c.exchange_ts_ms));
        assert!(c.is_complete_at(c.close_time_ms + 1));
    }

    #[test]
    fn parses_captured_depth() {
        let fx = include_str!("fixtures/depth_btc.txt");
        let msgs: Vec<_> = fx.lines().filter_map(parse_line).collect();
        assert!(msgs.len() > 10);
        let mut saw_snapshot = false;
        for m in &msgs {
            let MarketMsg::Depth(d) = m else { panic!() };
            assert_eq!(d.symbol, "BTCUSDT");
            if d.kind == DepthKind::Snapshot {
                saw_snapshot = true;
                assert_eq!(d.bids.len(), 20);
                assert_eq!(d.asks.len(), 20);
            } else {
                assert!(d.event_ts_ms.is_some());
            }
        }
        assert!(saw_snapshot);
    }

    #[test]
    fn parses_captured_current_prices() {
        let fx = include_str!("fixtures/current_prices.txt");
        let msgs: Vec<_> = fx.lines().filter_map(parse_line).collect();
        let MarketMsg::CurrentPrices(snap) = &msgs[0] else {
            panic!()
        };
        assert_eq!(snap.kind, PricesKind::Snapshot);
        let natgas = &snap.prices["B-NATGAS_USDT"];
        assert!(natgas.mark_price.is_some() && natgas.last_price.is_some());
        assert!(natgas.funding_rate.is_some());
        let MarketMsg::CurrentPrices(upd) = &msgs[1] else {
            panic!()
        };
        assert_eq!(upd.kind, PricesKind::Update);
        assert!(upd.send_ts_ms.is_some());
        if let Some(u) = upd.prices.get("B-NATGAS_USDT") {
            let mut merged = natgas.clone();
            merged.merge(u);
            assert_eq!(merged.mark_price, u.mark_price);
            assert_eq!(merged.last_price, natgas.last_price.or(u.last_price));
        }
    }

    #[test]
    fn unknown_events_are_ignored_and_bad_payloads_rejected() {
        assert!(parse_event("df-order-update", &serde_json::json!({}), 0)
            .unwrap()
            .is_none());
        assert!(parse_event("new-trade", &serde_json::json!({"p":"1"}), 0).is_err());
        assert!(parse_event(
            "depth-update",
            &serde_json::json!({"s":"X","vs":1,"ts":1,"asks":{"1":"-2"}}),
            0
        )
        .is_err());
    }
}
