//! Wire types for CoinDCX public endpoints. Field names are exactly those
//! observed in live responses; unknown fields are ignored.
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

fn de_f64_str<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        S(String),
        F(f64),
        N(Option<()>),
    }
    Ok(match V::deserialize(d)? {
        V::S(s) => s.parse().ok(),
        V::F(f) => Some(f),
        V::N(_) => None,
    })
}

/// GET https://api.coindcx.com/exchange/v1/markets_details (Spot universe).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpotMarketDetail {
    pub coindcx_name: String,
    pub symbol: String,
    pub pair: String,
    pub base_currency_short_name: String,
    pub target_currency_short_name: String,
    pub status: String,
    #[serde(default)]
    pub min_notional: Option<f64>,
    #[serde(default)]
    pub ecode: Option<String>,
}

/// GET https://api.coindcx.com/exchange/ticker
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpotTicker {
    pub market: String,
    #[serde(default, deserialize_with = "de_f64_str")]
    pub last_price: Option<f64>,
    #[serde(default, deserialize_with = "de_f64_str")]
    pub bid: Option<f64>,
    #[serde(default, deserialize_with = "de_f64_str")]
    pub ask: Option<f64>,
    #[serde(default, deserialize_with = "de_f64_str")]
    pub volume: Option<f64>,
    #[serde(default, deserialize_with = "de_f64_str")]
    pub change_24_hour: Option<f64>,
    #[serde(default)]
    pub timestamp: Option<i64>,
}

/// GET .../derivatives/futures/data/instrument?pair=..&margin_currency_short_name=..
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FuturesInstrument {
    pub pair: String,
    pub status: String,
    #[serde(default)]
    pub kind: Option<String>,
    pub margin_currency_short_name: String,
    pub settle_currency_short_name: String,
    pub quote_currency_short_name: String,
    pub underlying_currency_short_name: String,
    pub max_leverage_long: f64,
    pub max_leverage_short: f64,
    pub price_increment: f64,
    pub quantity_increment: f64,
    pub min_quantity: f64,
    pub max_quantity: f64,
    pub min_notional: f64,
    #[serde(default)]
    pub max_notional: f64,
    #[serde(default)]
    pub max_market_order_quantity: Option<f64>,
    /// Percent, e.g. 0.059 == 0.059%.
    pub maker_fee: f64,
    /// Percent, e.g. 0.059 == 0.059%.
    pub taker_fee: f64,
    #[serde(default)]
    pub funding_frequency: Option<f64>,
    #[serde(default)]
    pub safety_percentage: Option<f64>,
    #[serde(default)]
    pub liquidation_fee: Option<f64>,
    #[serde(default)]
    pub exit_only: bool,
    #[serde(default)]
    pub order_types: Vec<String>,
    #[serde(default)]
    pub time_in_force_options: Vec<String>,
    /// leverage (string key) -> max position notional
    #[serde(default)]
    pub dynamic_position_leverage_details: BTreeMap<String, f64>,
    #[serde(default)]
    pub dynamic_safety_margin_details: BTreeMap<String, f64>,
}

impl FuturesInstrument {
    /// Leverage tiers as (leverage, max_notional) sorted by leverage.
    pub fn leverage_tiers(&self) -> Vec<(f64, f64)> {
        let mut v: Vec<(f64, f64)> = self
            .dynamic_position_leverage_details
            .iter()
            .filter_map(|(k, n)| k.parse::<f64>().ok().map(|l| (l, *n)))
            .collect();
        v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        v
    }
    /// Taker fee as a fraction (0.059 % -> 0.00059).
    pub fn taker_fee_frac(&self) -> f64 {
        self.taker_fee / 100.0
    }
    pub fn is_tradable(&self) -> bool {
        self.status == "active" && !self.exit_only
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct InstrumentEnvelope {
    pub instrument: FuturesInstrument,
}

/// One entry of GET public.coindcx.com/market_data/v3/current_prices/futures/rt
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RtPrice {
    /// last traded price (absent for some entries in live data)
    #[serde(default)]
    pub ls: Option<f64>,
    /// mark price
    #[serde(default)]
    pub mp: Option<f64>,
    /// funding rate
    #[serde(default)]
    pub fr: Option<f64>,
    /// 24h volume
    #[serde(default)]
    pub v: Option<f64>,
    /// 24h percent change
    #[serde(default)]
    pub pc: Option<f64>,
    #[serde(default)]
    pub h: Option<f64>,
    #[serde(default)]
    pub l: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RtPrices {
    pub ts: i64,
    #[serde(default)]
    pub vs: Option<i64>,
    pub prices: BTreeMap<String, RtPrice>,
}

/// GET public.coindcx.com/market_data/v3/orderbook/{pair}-futures/{10|20|50}
/// Snapshot: price (string) -> quantity (string).
#[derive(Debug, Clone, Deserialize)]
pub struct RawBook {
    pub ts: i64,
    #[serde(default)]
    pub vs: Option<i64>,
    #[serde(default)]
    pub asks: BTreeMap<String, String>,
    #[serde(default)]
    pub bids: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Book {
    pub ts: i64,
    pub vs: Option<i64>,
    /// best first (descending)
    pub bids: Vec<(f64, f64)>,
    /// best first (ascending)
    pub asks: Vec<(f64, f64)>,
}

impl From<RawBook> for Book {
    fn from(r: RawBook) -> Self {
        let parse = |m: &BTreeMap<String, String>| -> Vec<(f64, f64)> {
            m.iter()
                .filter_map(|(p, q)| Some((p.parse::<f64>().ok()?, q.parse::<f64>().ok()?)))
                .filter(|(_, q)| *q > 0.0)
                .collect()
        };
        let mut bids = parse(&r.bids);
        let mut asks = parse(&r.asks);
        bids.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        asks.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        Book {
            ts: r.ts,
            vs: r.vs,
            bids,
            asks,
        }
    }
}

impl Book {
    pub fn best_bid(&self) -> Option<f64> {
        self.bids.first().map(|l| l.0)
    }
    pub fn best_ask(&self) -> Option<f64> {
        self.asks.first().map(|l| l.0)
    }
    /// A crossed or empty book fails integrity and must not be used.
    pub fn is_sane(&self) -> bool {
        matches!((self.best_bid(), self.best_ask()), (Some(b), Some(a)) if b < a)
    }
}

/// GET public.coindcx.com/market_data/candlesticks?...&pcode=f
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Candle {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    /// candle open time, ms
    pub time: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CandleResponse {
    pub s: String,
    #[serde(default)]
    pub data: Vec<Candle>,
}

/// GET .../derivatives/futures/data/trades?pair=..
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FuturesTrade {
    pub price: f64,
    pub quantity: f64,
    pub timestamp: f64,
    #[serde(default)]
    pub is_maker: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_live_instrument_shape() {
        let j = r#"{"instrument":{"settle_currency_short_name":"USDT","quote_currency_short_name":"USDT","position_currency_short_name":"BTC","underlying_currency_short_name":"BTC","status":"active","pair":"B-BTC_USDT","kind":"perpetual","settlement":"never","max_leverage_long":20.0,"max_leverage_short":20.0,"unit_contract_value":1.0,"price_increment":0.1,"quantity_increment":0.001,"min_trade_size":0.001,"min_price":584.64,"max_price":614503.0,"min_quantity":0.001,"max_quantity":950.0,"min_notional":60.0,"maker_fee":0.0236,"taker_fee":0.059,"safety_percentage":1.5,"max_market_order_quantity":120.0,"funding_frequency":8,"max_notional":0.0,"exit_only":false,"liquidation_fee":1.0,"time_in_force_options":["good_till_cancel"],"order_types":["limit_order","market_order"],"dynamic_position_leverage_details":{"2":200000000,"20":50000000,"100":20000},"margin_currency_short_name":"USDT"}}"#;
        let i: InstrumentEnvelope = serde_json::from_str(j).unwrap();
        let t = i.instrument.leverage_tiers();
        assert_eq!(t[0], (2.0, 200_000_000.0));
        assert_eq!(t[2], (100.0, 20_000.0));
        assert!((i.instrument.taker_fee_frac() - 0.00059).abs() < 1e-12);
        assert!(i.instrument.is_tradable());
    }
    #[test]
    fn parses_book_and_ticker() {
        let b: RawBook = serde_json::from_str(r#"{"ts":1,"vs":2,"asks":{"101":"1","100.5":"2"},"bids":{"99":"3","100":"0.5","98":"0"}}"#).unwrap();
        let b = Book::from(b);
        assert_eq!(b.best_ask(), Some(100.5));
        assert_eq!(b.best_bid(), Some(100.0));
        assert_eq!(b.bids.len(), 2); // zero qty dropped
        assert!(b.is_sane());
        let t: SpotTicker = serde_json::from_str(r#"{"market":"USDTINR","last_price":"104.95","bid":"104.9","ask":"105","volume":"1","change_24_hour":"-0.5","timestamp":1}"#).unwrap();
        assert_eq!(t.last_price, Some(104.95));
    }
}
