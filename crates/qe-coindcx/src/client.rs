//! Public (unauthenticated) CoinDCX REST client with a conservative global rate
//! limiter, HTTP 429 detection and a circuit breaker.
//!
//! Documented limits: FAQ "16/sec, 960/min". We pace all public requests to at
//! most 8/s (one every 125 ms). On HTTP 429 or 5 consecutive failures the
//! breaker opens for 30 s and every call fails fast; callers mark data stale.
use crate::types::*;
use anyhow::{anyhow, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub const API: &str = "https://api.coindcx.com";
pub const PUBLIC: &str = "https://public.coindcx.com";
const MIN_INTERVAL: Duration = Duration::from_millis(125);
const BREAKER_COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Limiter {
    last: Option<Instant>,
    open_until: Option<Instant>,
    consecutive_failures: u32,
}

#[derive(Clone)]
pub struct PublicClient {
    http: reqwest::Client,
    lim: Arc<Mutex<Limiter>>,
}

impl Default for PublicClient {
    fn default() -> Self {
        Self::new()
    }
}

impl PublicClient {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent("quant-engine-desk/0.1")
            .build()
            .expect("http client");
        Self {
            http,
            lim: Arc::new(Mutex::new(Limiter::default())),
        }
    }

    pub async fn breaker_open(&self) -> bool {
        matches!(self.lim.lock().await.open_until, Some(t) if t > Instant::now())
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        {
            let mut l = self.lim.lock().await;
            if let Some(t) = l.open_until {
                if t > Instant::now() {
                    return Err(anyhow!("circuit open (exchange throttled/failing)"));
                }
                l.open_until = None;
            }
            if let Some(last) = l.last {
                let el = last.elapsed();
                if el < MIN_INTERVAL {
                    tokio::time::sleep(MIN_INTERVAL - el).await;
                }
            }
            l.last = Some(Instant::now());
        }
        let res = self.http.get(url).send().await;
        let mut l = self.lim.lock().await;
        let out = match res {
            Ok(r) if r.status().as_u16() == 429 => {
                l.open_until = Some(Instant::now() + BREAKER_COOLDOWN);
                Err(anyhow!("HTTP 429 from {url}; breaker open 30s"))
            }
            Ok(r) if !r.status().is_success() => Err(anyhow!("HTTP {} from {url}", r.status())),
            Ok(r) => r
                .json::<T>()
                .await
                .map_err(|e| anyhow!("decode {url}: {e}")),
            Err(e) => Err(anyhow!("request {url}: {e}")),
        };
        if out.is_ok() {
            l.consecutive_failures = 0;
        } else {
            l.consecutive_failures += 1;
            if l.consecutive_failures >= 5 {
                l.open_until = Some(Instant::now() + BREAKER_COOLDOWN);
                l.consecutive_failures = 0;
            }
        }
        out
    }

    /// Spot universe A (metadata incl. status).
    pub async fn spot_markets_details(&self) -> Result<Vec<SpotMarketDetail>> {
        self.get(&format!("{API}/exchange/v1/markets_details"))
            .await
    }
    pub async fn spot_ticker(&self) -> Result<Vec<SpotTicker>> {
        self.get(&format!("{API}/exchange/ticker")).await
    }
    /// Futures universes B (INR) / C (USDT): list of active pair ids.
    pub async fn futures_active(&self, margin: &str) -> Result<Vec<String>> {
        self.get(&format!(
            "{API}/exchange/v1/derivatives/futures/data/active_instruments?margin_currency_short_name[]={margin}"
        ))
        .await
    }
    pub async fn futures_instrument(&self, pair: &str, margin: &str) -> Result<FuturesInstrument> {
        let e: InstrumentEnvelope = self
            .get(&format!(
                "{API}/exchange/v1/derivatives/futures/data/instrument?pair={pair}&margin_currency_short_name={margin}"
            ))
            .await?;
        Ok(e.instrument)
    }
    pub async fn futures_rt(&self) -> Result<RtPrices> {
        self.get(&format!(
            "{PUBLIC}/market_data/v3/current_prices/futures/rt"
        ))
        .await
    }
    /// depth must be 10, 20 or 50 (documented values).
    pub async fn futures_book(&self, pair: &str, depth: u32) -> Result<Book> {
        let d = match depth {
            10 | 20 | 50 => depth,
            _ => 20,
        };
        let raw: RawBook = self
            .get(&format!(
                "{PUBLIC}/market_data/v3/orderbook/{pair}-futures/{d}"
            ))
            .await?;
        Ok(raw.into())
    }
    /// resolution: "1", "5", "15", "60", "1D" ... ; from/to in seconds.
    pub async fn futures_candles(
        &self,
        pair: &str,
        resolution: &str,
        from_s: i64,
        to_s: i64,
    ) -> Result<Vec<Candle>> {
        let r: CandleResponse = self
            .get(&format!(
                "{PUBLIC}/market_data/candlesticks?pair={pair}&from={from_s}&to={to_s}&resolution={resolution}&pcode=f"
            ))
            .await?;
        if r.s != "ok" {
            return Err(anyhow!("candles status {}", r.s));
        }
        Ok(r.data)
    }
    pub async fn futures_trades(&self, pair: &str) -> Result<Vec<FuturesTrade>> {
        self.get(&format!(
            "{API}/exchange/v1/derivatives/futures/data/trades?pair={pair}"
        ))
        .await
    }
}
