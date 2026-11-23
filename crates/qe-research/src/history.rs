//! Historical loader for **public** CoinDCX futures candles
//! (`GET public.coindcx.com/market_data/candlesticks?...&pcode=f`).
//!
//! Resolutions verified against the live endpoint on 2026-11-23 (BTC, 30-day
//! window): `1, 5, 15, 30, 60, 240, 480, 1D, 3D, 1M` return `s = "ok"`; `2D` and
//! `1W` return no data. A single request returns at most 30 000 candles and the
//! response starts at `from`, so 1m history is requested in windows of
//! <= 20 000 minutes that end at "now".
//!
//! 2d candles are **not** requested: they are built locally from two completed,
//! contiguous 1d candles with `qe_math::timeframe::aggregate_2d`.
//! Every candle is marked `complete` only when `open + width <= fetch time`;
//! incomplete candles are stored with `complete = false` and the evaluator
//! never uses them as observations.
//!
//! Store layout: `out_dir/{pair}/{res}.json` = `[HCandle]` (times in Unix ms).
use anyhow::{anyhow, Result};
use qe_coindcx::PublicClient;
use qe_math::timeframe::{aggregate_2d, mark_completeness, Candle};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// (resolution label sent to CoinDCX, candle width ms, default lookback seconds)
pub const RESOLUTIONS: [(&str, i64, i64); 7] = [
    ("1", 60_000, 7 * 86_400),
    ("5", 300_000, 30 * 86_400),
    ("15", 900_000, 30 * 86_400),
    ("60", 3_600_000, 90 * 86_400),
    ("240", 14_400_000, 180 * 86_400),
    ("1D", 86_400_000, 365 * 86_400),
    ("3D", 259_200_000, 720 * 86_400),
];

pub struct HistoryConfig {
    pub out_dir: PathBuf,
    /// number of USDT-margined pairs, ranked by 24h quote volume from the RT feed
    pub pairs: usize,
    /// 1m lookback in days (overrides the default 7)
    pub days_1m: f64,
}

fn now_ms() -> i64 {
    qe_coindcx::auth::request_timestamp_ms()
}

/// Stored candle (qe-math's `Candle` is not `Deserialize`, so the store keeps
/// its own serde shape and converts losslessly).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HCandle {
    pub open_ms: i64,
    pub width_ms: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub complete: bool,
}

impl From<&Candle> for HCandle {
    fn from(c: &Candle) -> Self {
        HCandle {
            open_ms: c.open_ms,
            width_ms: c.width_ms,
            open: c.open,
            high: c.high,
            low: c.low,
            close: c.close,
            volume: c.volume,
            complete: c.complete,
        }
    }
}

impl HCandle {
    pub fn to_math(&self) -> Candle {
        Candle {
            open_ms: self.open_ms,
            width_ms: self.width_ms,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
            buy_volume: 0.0,
            sell_volume: 0.0,
            trades: 0,
            complete: self.complete,
        }
    }
}

fn save(path: &Path, v: &[Candle]) -> Result<()> {
    let h: Vec<HCandle> = v.iter().map(HCandle::from).collect();
    std::fs::write(path, serde_json::to_vec(&h)?)?;
    Ok(())
}

pub fn to_candle(c: &qe_coindcx::types::Candle, width_ms: i64) -> Candle {
    Candle {
        open_ms: c.time,
        width_ms,
        open: c.open,
        high: c.high,
        low: c.low,
        close: c.close,
        volume: c.volume,
        buy_volume: 0.0,
        sell_volume: 0.0,
        trades: 0,
        complete: false,
    }
}

/// Fetch `[from_s, to_s)` in chunks of at most `max_n` candles, de-duplicated and sorted.
async fn fetch_range(
    c: &PublicClient,
    pair: &str,
    res: &str,
    width_ms: i64,
    from_s: i64,
    to_s: i64,
) -> Result<Vec<Candle>> {
    let chunk_s = 20_000 * width_ms / 1000;
    let mut out: Vec<Candle> = vec![];
    let mut a = from_s;
    while a < to_s {
        let b = (a + chunk_s).min(to_s);
        let v = c.futures_candles(pair, res, a, b).await?;
        out.extend(v.iter().map(|x| to_candle(x, width_ms)));
        a = b;
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    out.sort_by_key(|c| c.open_ms);
    out.dedup_by_key(|c| c.open_ms);
    Ok(out)
}

pub async fn run(cfg: HistoryConfig) -> Result<Value> {
    let c = PublicClient::new();
    let rt = c.futures_rt().await?;
    let mut uni = std::collections::HashSet::new();
    uni.extend(c.futures_active("USDT").await?);
    let mut ranked: Vec<(f64, String)> = rt
        .prices
        .iter()
        .filter(|(p, _)| uni.contains(*p))
        .filter_map(|(p, x)| Some((x.v? * x.ls?, p.clone())))
        .collect();
    ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
    let pairs: Vec<String> = ranked.into_iter().take(cfg.pairs).map(|x| x.1).collect();
    let mut counts = serde_json::Map::new();
    let mut errors = vec![];
    for pair in &pairs {
        let dir = cfg.out_dir.join(pair);
        std::fs::create_dir_all(&dir)?;
        let now = now_ms();
        let mut per = serde_json::Map::new();
        for (res, w, look) in RESOLUTIONS {
            let look = if res == "1" {
                (cfg.days_1m * 86_400.0) as i64
            } else {
                look
            };
            match fetch_range(&c, pair, res, w, now / 1000 - look, now / 1000).await {
                Ok(mut v) => {
                    mark_completeness(&mut v, now);
                    per.insert(res.into(), json!(v.len()));
                    save(&dir.join(format!("{res}.json")), &v)?;
                    if res == "1D" {
                        let d2 = aggregate_2d(&v);
                        per.insert("2D(local)".into(), json!(d2.len()));
                        save(&dir.join("2D.local.json"), &d2)?;
                    }
                }
                Err(e) => errors.push(format!("{pair} {res}: {e}")),
            }
        }
        eprintln!("[history] {pair} {}", Value::Object(per.clone()));
        counts.insert(pair.clone(), Value::Object(per));
    }
    Ok(json!({"out_dir": cfg.out_dir, "pairs": pairs, "counts": counts, "errors": errors}))
}

/// Load stored candles for one pair/resolution.
pub fn load(dir: &Path, pair: &str, res: &str) -> Result<Vec<Candle>> {
    let p = dir.join(pair).join(format!("{res}.json"));
    let b = std::fs::read(&p).map_err(|e| anyhow!("{}: {e}", p.display()))?;
    let h: Vec<HCandle> = serde_json::from_slice(&b)?;
    Ok(h.iter().map(HCandle::to_math).collect())
}
