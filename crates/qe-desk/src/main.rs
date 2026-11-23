//! qe-desk: live CoinDCX public-data desk exposing `/api/state`
//! (docs/DASHBOARD_API.md). Execution mode is always paper; no order is ever sent.
//!
//! Loops: universe discovery (60 s), futures RT prices (1 s), spot ticker (5 s),
//! order books for the top candidates (2 s). The scanner and forecast run on
//! every update using deterministic qe-math functions.
use axum::{routing::get, Json, Router};
use qe_coindcx::{types::Book, PublicClient};
use qe_math::{micro, prob, stats};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

const HORIZON_S: f64 = 120.0;
const K_STOP: f64 = 1.0; // SL = K_STOP * sigma_120 (uncalibrated prior)
const K_TARGET: f64 = 1.5; // TP = K_TARGET * sigma_120 (uncalibrated prior)
const LAMBDA_MOM: f64 = 0.2; // momentum persistence prior
const GAMMA_OBI: f64 = 0.05; // order-book-imbalance drift prior
const TOP_N: usize = 8;

fn now_ms() -> i64 {
    qe_coindcx::auth::request_timestamp_ms()
}

type Meta = (Option<f64>, Option<f64>, Option<f64>, Option<f64>); // mark, funding, pct change, vol24

#[derive(Default)]
struct Desk {
    started_ms: i64,
    last_universe_ms: Option<i64>,
    last_rt_ms: Option<i64>,
    last_spot_ms: Option<i64>,
    rt_polls: u64,
    errors: u64,
    last_error: Option<String>,
    fut_usdt: HashSet<String>,
    fut_inr: HashSet<String>,
    spot_active: usize,
    spot: HashMap<String, (f64, f64, f64)>,
    hist: HashMap<String, VecDeque<(i64, f64, f64)>>,
    rt_meta: HashMap<String, Meta>,
    books: HashMap<String, Book>,
    scanner: Vec<Value>,
    opportunities: VecDeque<Value>,
    audit: VecDeque<Value>,
}

impl Desk {
    fn audit(&mut self, kind: &str, msg: String, data: Value) {
        eprintln!("[{kind}] {msg}");
        self.audit
            .push_front(json!({"ts_ms": now_ms(), "kind": kind, "message": msg, "data": data}));
        self.audit.truncate(200);
    }
    fn err(&mut self, e: String) {
        self.errors += 1;
        self.audit("error", e.clone(), Value::Null);
        self.last_error = Some(e);
    }
}

type S = Arc<RwLock<Desk>>;

fn ret_over(h: &VecDeque<(i64, f64, f64)>, w_ms: i64) -> Option<f64> {
    let (t, p, _) = *h.back()?;
    let past = h.iter().rev().find(|x| x.0 <= t - w_ms)?;
    Some((p / past.1).ln())
}

/// Per-second volatility from 1-step log returns, scaled by mean sample spacing:
/// sigma_1s = RV_step / sqrt(mean dt).
fn sigma_per_s(h: &VecDeque<(i64, f64, f64)>) -> Option<f64> {
    if h.len() < 30 {
        return None;
    }
    let p: Vec<f64> = h.iter().map(|x| x.1).collect();
    let r = stats::log_returns(&p);
    let dt = (h.back()?.0 - h.front()?.0) as f64 / 1000.0 / (h.len() - 1) as f64;
    let rv = stats::realized_vol(&r)?;
    if rv <= 0.0 || dt <= 0.0 {
        return None;
    }
    Some(rv / dt.sqrt())
}

fn scan(d: &mut Desk) {
    let mut rows = vec![];
    for (pair, h) in &d.hist {
        let Some(&(_, last, _)) = h.back() else {
            continue;
        };
        let sig = sigma_per_s(h);
        let r10 = ret_over(h, 10_000);
        let z10 = match (r10, sig) {
            (Some(r), Some(s)) if s > 0.0 => Some(r / (s * 10f64.sqrt())),
            _ => None,
        };
        // volume z: latest 24h-volume delta vs distribution of deltas (approximation)
        let v: Vec<f64> = h.iter().map(|x| x.2).collect();
        let dv: Vec<f64> = v.windows(2).map(|w| (w[1] - w[0]).max(0.0)).collect();
        let zv = dv.last().and_then(|x| stats::zscore(*x, &dv));
        let (mark, fr, pc, vol) = d.rt_meta.get(pair).cloned().unwrap_or_default();
        let base = pair.trim_start_matches("B-").replace('_', "");
        let basis = d.spot.get(&base).map(|s| (last - s.0) / s.0 * 1e4);
        let book = d.books.get(pair).filter(|b| b.is_sane());
        let (obi, spread) = match book {
            Some(b) => (
                micro::obi(&b.bids, &b.asks, 10),
                Some(micro::spread_bps(
                    b.best_bid().unwrap(),
                    b.best_ask().unwrap(),
                )),
            ),
            None => (None, None),
        };
        let mut reasons = vec![];
        if z10.is_some_and(|z| z.abs() > 3.0) {
            reasons.push("price_jump_z>3");
        }
        if zv.is_some_and(|z| z > 3.0) {
            reasons.push("volume_spike_z>3");
        }
        if spread.is_some_and(|s| s > 30.0) {
            reasons.push("wide_spread>30bps");
        }
        let liq = (1.0 + vol.unwrap_or(0.0) * last).ln();
        let score =
            z10.map_or(0.0, f64::abs) + 0.1 * liq + if reasons.is_empty() { 0.0 } else { 1.0 };
        rows.push(json!({
            "pair": pair, "margin": if d.fut_inr.contains(pair) { "INR" } else { "USDT" },
            "last": last, "mark": mark, "funding_rate": fr, "change_24h_pct": pc, "volume_24h": vol,
            "ret_10s": r10, "ret_30s": ret_over(h, 30_000), "ret_60s": ret_over(h, 60_000), "ret_300s": ret_over(h, 300_000),
            "vol_1s": sig, "vol_1m_candle": null, "z_ret_10s": z10, "z_volume": zv, "basis_bps": basis,
            "obi": obi, "spread_bps": spread, "anomaly": !reasons.is_empty(), "anomaly_reasons": reasons,
            "regime": null, "score": score,
        }));
    }
    rows.sort_by(|a, b| {
        b["score"]
            .as_f64()
            .partial_cmp(&a["score"].as_f64())
            .unwrap()
    });
    d.scanner = rows;
}

/// Forecast and EV for a candidate with a fresh, sane book.
/// mu = LAMBDA * r_60/60 + GAMMA * OBI * sigma; TP/SL = k * sigma * sqrt(120);
/// first-passage lattice gives P(target), P(stop), P(neither);
/// EV = Pw W - Pl L + Pn E[R|neither] - C, C = 2 * taker fee + spread + 2 * 1 bp slippage.
/// Taker fee 0.059% is the live value reported for the sampled contracts; wiring
/// per-instrument metadata is listed as unfinished.
fn evaluate(d: &mut Desk, pair: &str) {
    let Some(h) = d.hist.get(pair) else { return };
    let Some(sig) = sigma_per_s(h) else { return };
    let Some(b) = d
        .books
        .get(pair)
        .filter(|b| b.is_sane() && now_ms() - b.ts < 5_000)
    else {
        return;
    };
    let (bid, ask) = (b.best_bid().unwrap(), b.best_ask().unwrap());
    let obi = micro::obi(&b.bids, &b.asks, 10).unwrap_or(0.0);
    let m60 = ret_over(h, 60_000).unwrap_or(0.0) / 60.0;
    let mu = LAMBDA_MOM * m60 + GAMMA_OBI * obi * sig;
    let s120 = sig * HORIZON_S.sqrt();
    let (tp, sl) = (K_TARGET * s120, K_STOP * s120);
    let fee = 0.00059;
    let cost = 2.0 * fee + (ask - bid) / ((ask + bid) / 2.0) + 2.0 * 0.0001;
    let p_up = prob::norm_cdf(mu * HORIZON_S / s120);
    let mut out = vec![];
    for (side, m) in [("long", mu), ("short", -mu)] {
        let fp = prob::first_passage(m, sig, tp, sl, HORIZON_S, 0.5);
        let gross = prob::bracket_ev(&fp, tp, sl, 0.0);
        let net = gross - cost;
        let reason = if net <= 0.0 {
            format!(
                "EV net {:.2} bps <= 0 after costs {:.2} bps",
                net * 1e4,
                cost * 1e4
            )
        } else {
            "positive EV but model uncalibrated and paper lifecycle not wired: no entry".to_string()
        };
        out.push(json!({"pair": pair, "side": side, "p_win": fp.p_target, "p_up_horizon": p_up, "p_stop": fp.p_stop,
            "p_neither": fp.p_neither, "tp_pct": tp * 100.0, "sl_pct": sl * 100.0, "ev_gross_bps": gross * 1e4,
            "cost_bps": cost * 1e4, "ev_net_bps": net * 1e4, "horizon_s": HORIZON_S, "qty": 0.0, "notional": 0.0,
            "leverage": 0.0, "decision": "rejected", "reason": reason, "calibrated": false, "ts_ms": now_ms(),
            "inputs": {"mu_per_s": mu, "sigma_per_s": sig, "obi": obi, "bid": bid, "ask": ask,
                "e_t_target": fp.e_t_target, "e_t_stop": fp.e_t_stop}}));
    }
    for o in out {
        d.opportunities.push_front(o);
    }
    d.opportunities.truncate(100);
}

async fn universe_loop(c: PublicClient, s: S) {
    loop {
        let (u, i, sp) = tokio::join!(
            c.futures_active("USDT"),
            c.futures_active("INR"),
            c.spot_markets_details()
        );
        let mut d = s.write().await;
        match (u, i, sp) {
            (Ok(u), Ok(i), Ok(sp)) => {
                let u: HashSet<String> = u.into_iter().collect();
                let i: HashSet<String> = i.into_iter().collect();
                if !d.fut_usdt.is_empty() {
                    let listed: Vec<_> = u.difference(&d.fut_usdt).cloned().collect();
                    let delisted: Vec<_> = d.fut_usdt.difference(&u).cloned().collect();
                    if !listed.is_empty() || !delisted.is_empty() {
                        d.audit(
                            "universe",
                            "listing change".into(),
                            json!({"listed": listed, "delisted": delisted}),
                        );
                    }
                }
                let active = sp.iter().filter(|m| m.status == "active").count();
                d.audit(
                    "universe",
                    format!(
                        "futures USDT {} INR {} spot active {}",
                        u.len(),
                        i.len(),
                        active
                    ),
                    Value::Null,
                );
                d.fut_usdt = u;
                d.fut_inr = i;
                d.spot_active = active;
                d.last_universe_ms = Some(now_ms());
            }
            (a, b, c) => {
                let e = format!(
                    "universe refresh failed: {:?} {:?} {:?}",
                    a.err(),
                    b.err(),
                    c.err()
                );
                d.err(e)
            }
        }
        drop(d);
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

async fn rt_loop(c: PublicClient, s: S) {
    loop {
        match c.futures_rt().await {
            Ok(rt) => {
                let mut d = s.write().await;
                let ts = rt.ts;
                for (pair, p) in rt.prices {
                    let Some(ls) = p.ls.filter(|x| *x > 0.0) else {
                        continue;
                    };
                    if !d.fut_usdt.contains(&pair) && !d.fut_inr.contains(&pair) {
                        continue; // only instruments present in the API universe
                    }
                    let h = d.hist.entry(pair.clone()).or_default();
                    if h.back().is_none_or(|x| x.0 < ts) {
                        h.push_back((ts, ls, p.v.unwrap_or(0.0)));
                        while h.len() > 600 {
                            h.pop_front();
                        }
                    }
                    d.rt_meta.insert(pair, (p.mp, p.fr, p.pc, p.v));
                }
                d.last_rt_ms = Some(now_ms());
                d.rt_polls += 1;
                scan(&mut d);
            }
            Err(e) => s.write().await.err(format!("rt: {e}")),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn spot_loop(c: PublicClient, s: S) {
    loop {
        match c.spot_ticker().await {
            Ok(t) => {
                let mut d = s.write().await;
                for x in t {
                    if let (Some(l), Some(b), Some(a)) = (x.last_price, x.bid, x.ask) {
                        d.spot.insert(x.market, (l, b, a));
                    }
                }
                d.last_spot_ms = Some(now_ms());
            }
            Err(e) => s.write().await.err(format!("spot ticker: {e}")),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn book_loop(c: PublicClient, s: S) {
    loop {
        let top: Vec<String> = {
            let d = s.read().await;
            d.scanner
                .iter()
                .filter(|r| r["vol_1s"].is_number())
                .take(TOP_N)
                .filter_map(|r| r["pair"].as_str().map(String::from))
                .collect()
        };
        for p in top {
            match c.futures_book(&p, 20).await {
                Ok(b) => {
                    let mut d = s.write().await;
                    d.books.insert(p.clone(), b);
                    evaluate(&mut d, &p);
                }
                Err(e) => s.write().await.err(format!("book {p}: {e}")),
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn state(s: axum::extract::State<S>) -> Json<Value> {
    let d = s.read().await;
    let now = now_ms();
    let stale = d.last_rt_ms.is_none_or(|t| now - t > 5_000);
    Json(json!({
        "server_time_ms": now, "mode": "paper",
        "live_orders_enabled": qe_coindcx::orders::LiveGate::default().enabled(),
        "health": {"started_ms": d.started_ms, "last_universe_ms": d.last_universe_ms, "last_rt_ms": d.last_rt_ms,
            "last_spot_ticker_ms": d.last_spot_ms, "rt_polls": d.rt_polls, "errors": d.errors,
            "last_error": d.last_error, "stale": stale},
        "universe": {"futures_active": d.fut_usdt.union(&d.fut_inr).count(), "futures_usdt": d.fut_usdt.len(),
            "futures_inr": d.fut_inr.len(), "futures_tracked": d.hist.len(), "spot_active": d.spot_active,
            "spot_with_ticker": d.spot.len(), "refreshed_ms": d.last_universe_ms},
        "scanner": d.scanner.iter().take(60).collect::<Vec<_>>(),
        "opportunities": d.opportunities,
        "positions": [], "fills": [], "closed": [],
        "account": {"currency": "INR", "paper": true, "starting_equity": 100.0, "equity": 100.0,
            "realized_pnl": 0.0, "unrealized_pnl": 0.0, "fees_paid": 0.0, "day_pnl": 0.0, "used_margin": 0.0,
            "available": 100.0, "trades": 0, "wins": 0, "losses": 0, "halted": true,
            "halt_reason": "paper lifecycle not wired in this build: entries disabled",
            "policy": {"tier": "growth", "risk_per_trade_frac": 0.01, "max_leverage": 0.0, "max_positions": 0,
                "daily_loss_limit_frac": 0.05, "kelly_fraction": 0.25, "max_hold_s": 120}},
        "audit": d.audit,
    }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::var("QE_DESK_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let s: S = Arc::new(RwLock::new(Desk {
        started_ms: now_ms(),
        ..Default::default()
    }));
    let c = PublicClient::new();
    tokio::spawn(universe_loop(c.clone(), s.clone()));
    tokio::spawn(rt_loop(c.clone(), s.clone()));
    tokio::spawn(spot_loop(c.clone(), s.clone()));
    tokio::spawn(book_loop(c.clone(), s.clone()));
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dashboard/dist");
    let app = Router::new()
        .route("/api/state", get(state))
        .fallback_service(tower_http::services::ServeDir::new(dist))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(s);
    eprintln!("qe-desk listening on http://{addr} (paper mode; live orders disabled)");
    let l = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(l, app).await?;
    Ok(())
}
