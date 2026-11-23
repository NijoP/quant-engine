//! Recorder: captures live **public** CoinDCX futures market data to a local
//! JSONL store (gitignored; never committed). No credentials are read or used.
//!
//! Files written under `out_dir` (one JSON object per line, append-only):
//! * `rt.jsonl`     — every RT poll of `market_data/v3/current_prices/futures/rt`:
//!   `{"recv_ms", "ts", "vs", "p": {pair: [last, mark, vol24]}}` restricted to pairs
//!   present in the active INR/USDT futures universe.
//! * `books.jsonl`  — order-book snapshots (depth 20) for the most liquid pairs:
//!   `{"recv_ms", "pair", "ts", "vs", "bids": [[p,q]..], "asks": [[p,q]..]}`.
//! * `trades.jsonl` — recent public trades per pair (raw; de-duplicated at load):
//!   `{"recv_ms", "pair", "trades": [{price, quantity, timestamp, is_maker}]}`.
//! * `instruments.jsonl` — instrument metadata (fees, increments) for book pairs.
//!
//! `recv_ms` is the local receive time; `ts` is the exchange timestamp. The
//! evaluator orders by exchange time and only ever uses data with ts <= decision time.
//!
//! Request budget: RT 1/s + books ~3/s + trades ~0.5/s, all through the shared
//! `PublicClient` limiter (<= 8 req/s, documented limit 16/s).
use anyhow::Result;
use qe_coindcx::PublicClient;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

fn now_ms() -> i64 {
    qe_coindcx::auth::request_timestamp_ms()
}

fn append(path: &Path, v: &Value) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{v}")?;
    Ok(())
}

#[derive(Default)]
struct Shared {
    universe: HashSet<String>,
    /// pairs whose books and trades are sampled (top by 24h quote volume)
    focus: Vec<String>,
    rt_polls: u64,
    books: u64,
    trade_polls: u64,
    errors: u64,
}

pub struct RecordConfig {
    pub out_dir: PathBuf,
    pub minutes: f64,
    pub focus_pairs: usize,
}

/// Run the recorder until `minutes` elapse. Returns a JSON summary.
pub async fn run(cfg: RecordConfig) -> Result<Value> {
    std::fs::create_dir_all(&cfg.out_dir)?;
    let c = PublicClient::new();
    let s = Arc::new(RwLock::new(Shared::default()));
    let mut uni = HashSet::new();
    for m in ["USDT", "INR"] {
        uni.extend(c.futures_active(m).await?);
    }
    eprintln!("[record] universe {} futures pairs", uni.len());
    s.write().await.universe = uni;
    let deadline = now_ms() + (cfg.minutes * 60_000.0) as i64;
    let out = cfg.out_dir.clone();

    let rt = {
        let (c, s, out) = (c.clone(), s.clone(), out.clone());
        let focus_n = cfg.focus_pairs;
        tokio::spawn(async move {
            let mut last_ts = 0i64;
            while now_ms() < deadline {
                match c.futures_rt().await {
                    Ok(rt) if rt.ts > last_ts => {
                        last_ts = rt.ts;
                        let mut g = s.write().await;
                        let mut p = BTreeMap::new();
                        let mut by_qv = vec![];
                        for (pair, x) in &rt.prices {
                            if !g.universe.contains(pair) {
                                continue;
                            }
                            let Some(ls) = x.ls.filter(|v| *v > 0.0) else {
                                continue;
                            };
                            p.insert(pair.clone(), json!([ls, x.mp, x.v]));
                            if pair.ends_with("_USDT") {
                                by_qv.push((x.v.unwrap_or(0.0) * ls, pair.clone()));
                            }
                        }
                        if g.focus.is_empty() {
                            by_qv.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
                            g.focus = by_qv.into_iter().take(focus_n).map(|x| x.1).collect();
                            eprintln!("[record] focus pairs {:?}", g.focus);
                        }
                        g.rt_polls += 1;
                        drop(g);
                        let line = json!({"recv_ms": now_ms(), "ts": rt.ts, "vs": rt.vs, "p": p});
                        if let Err(e) = append(&out.join("rt.jsonl"), &line) {
                            eprintln!("[record] write rt: {e}");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        s.write().await.errors += 1;
                        eprintln!("[record] rt: {e}");
                    }
                }
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        })
    };

    let books = {
        let (c, s, out) = (c.clone(), s.clone(), out.clone());
        tokio::spawn(async move {
            let mut i = 0usize;
            let mut meta_done = HashSet::new();
            while now_ms() < deadline {
                let focus = s.read().await.focus.clone();
                if focus.is_empty() {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
                let pair = focus[i % focus.len()].clone();
                i += 1;
                if meta_done.insert(pair.clone()) {
                    match c.futures_instrument(&pair, "USDT").await {
                        Ok(m) => {
                            let _ = append(
                                &out.join("instruments.jsonl"),
                                &json!({"recv_ms": now_ms(), "instrument": m}),
                            );
                        }
                        Err(e) => eprintln!("[record] instrument {pair}: {e}"),
                    }
                }
                match c.futures_book(&pair, 20).await {
                    Ok(b) => {
                        s.write().await.books += 1;
                        let line = json!({"recv_ms": now_ms(), "pair": pair, "ts": b.ts, "vs": b.vs,
                            "bids": b.bids, "asks": b.asks});
                        let _ = append(&out.join("books.jsonl"), &line);
                    }
                    Err(e) => {
                        s.write().await.errors += 1;
                        eprintln!("[record] book {pair}: {e}");
                    }
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
    };

    let trades = {
        let (c, s, out) = (c.clone(), s.clone(), out.clone());
        tokio::spawn(async move {
            let mut i = 0usize;
            while now_ms() < deadline {
                let focus = s.read().await.focus.clone();
                if focus.is_empty() {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
                let pair = focus[i % focus.len()].clone();
                i += 1;
                match c.futures_trades(&pair).await {
                    Ok(t) => {
                        s.write().await.trade_polls += 1;
                        let line = json!({"recv_ms": now_ms(), "pair": pair, "trades": t});
                        let _ = append(&out.join("trades.jsonl"), &line);
                    }
                    Err(e) => {
                        s.write().await.errors += 1;
                        eprintln!("[record] trades {pair}: {e}");
                    }
                }
                tokio::time::sleep(Duration::from_millis(2000)).await;
            }
        })
    };

    let mut last_report = now_ms();
    while now_ms() < deadline {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if now_ms() - last_report >= 60_000 {
            last_report = now_ms();
            let g = s.read().await;
            eprintln!(
                "[record] rt_polls={} books={} trade_polls={} errors={}",
                g.rt_polls, g.books, g.trade_polls, g.errors
            );
        }
    }
    let _ = tokio::join!(rt, books, trades);
    let g = s.read().await;
    Ok(
        json!({"out_dir": cfg.out_dir, "universe": g.universe.len(), "focus": g.focus,
        "rt_polls": g.rt_polls, "books": g.books, "trade_polls": g.trade_polls, "errors": g.errors}),
    )
}
