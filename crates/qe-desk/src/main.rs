//! qe-desk: live CoinDCX public-data desk with the full paper pipeline
//! (docs/DASHBOARD_API.md). Execution mode is always paper; no order is ever
//! sent and no credential is read.
//!
//! Loops (each its own task): universe discovery (60 s), futures RT prices (1 s),
//! spot ticker (5 s), order books + forecast + governor for the top candidates
//! (3 s), books for open paper positions (0.5 s), the independent 120 s
//! lifecycle timer (250 ms), instrument metadata (1 s, one instrument per tick),
//! multi-timeframe candles (per candidate, every 2 min) and the system monitor (10 s).
mod desk;
mod store;

use axum::{
    body::Bytes,
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, State as AxState,
    },
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use desk::*;
use qe_coindcx::PublicClient;
use qe_math::{features, timeframe::ContextTf};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

type S = Arc<RwLock<Desk>>;

#[derive(Clone)]
struct App {
    s: S,
    c: PublicClient,
}

// ---------------------------------------------------------------- config

fn load_settings() -> Settings {
    let mut st = Settings::default();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs");
    let read = |f: &str| -> Option<toml::Table> {
        std::fs::read_to_string(dir.join(f))
            .ok()?
            .parse::<toml::Table>()
            .ok()
    };
    if let Some(t) = read("risk-limits.toml") {
        let f = |k: &str| t.get(k).and_then(|v| v.as_float());
        if let Some(m) = t.get("execution_mode").and_then(|v| v.as_str()) {
            // Only "paper" is honoured; anything else is refused and logged.
            if m != "paper" {
                eprintln!("[config] execution_mode={m} refused: this build is paper-only");
            }
        }
        if let Some(v) = f("paper_start_equity_inr") {
            st.paper_start_equity_inr = v;
        }
        if let Some(v) = f("stage1_target_inr") {
            st.stage1_target_inr = v;
        }
        if let Some(v) = f("stage2_target_inr") {
            st.stage2_target_inr = v;
        }
        if let Some(v) = f("paper_slippage_bps") {
            st.paper_slippage_bps = v;
        }
    }
    let overlay = |mut p: qe_risk::RiskPolicy, t: Option<toml::Table>| {
        if let Some(t) = t {
            let f = |k: &str| t.get(k).and_then(|v| v.as_float());
            let i = |k: &str| t.get(k).and_then(|v| v.as_integer());
            if let Some(v) = f("risk_fraction") {
                p.risk_fraction = v;
            }
            if let Some(v) = f("max_session_loss_fraction") {
                p.max_session_loss_fraction = v;
            }
            if let Some(v) = f("max_drawdown_from_hwm") {
                p.max_drawdown_from_hwm = v;
            }
            if let Some(v) = f("loss_step_down") {
                p.loss_step_down = v;
            }
            if let Some(v) = f("min_expected_net_edge") {
                p.min_expected_net_edge = v.max(0.0);
            }
            if let Some(v) = i("max_open_positions") {
                p.max_open_positions = (v.max(0) as usize).min(1);
            }
            if let Some(v) = f("liquidation_buffer") {
                p.liquidation_buffer = v.max(p.liquidation_buffer);
            }
            if let Some(v) = i("exit_lead_ms") {
                p.exit_lead_ms = v.max(0) as u64;
            }
        }
        // hard ceilings: config can only tighten
        p.clamped()
    };
    st.growth = overlay(qe_risk::RiskPolicy::growth(), read("growth-mode.toml"));
    st.protective = overlay(
        qe_risk::RiskPolicy::protective(),
        read("protective-mode.toml"),
    );
    st
}

// ---------------------------------------------------------------- loops

async fn universe_loop(c: PublicClient, s: S) {
    loop {
        if !s.read().await.controls.scanner_running {
            s.write().await.agent_stopped("universe_scanner");
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        let t0 = now_ms();
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
                for (m, new, old) in [
                    ("USDT", &u, d.fut_usdt.clone()),
                    ("INR", &i, d.fut_inr.clone()),
                ] {
                    if old.is_empty() {
                        continue;
                    }
                    let listed: Vec<_> = new.difference(&old).cloned().collect();
                    let delisted: Vec<_> = old.difference(new).cloned().collect();
                    if !listed.is_empty() || !delisted.is_empty() {
                        let ev = json!({"ts_ms": now_ms(), "kind": "listing_change", "margin": m, "listed": listed, "delisted": delisted});
                        d.events.push_front(ev.clone());
                        d.audit("universe", format!("{m} listing change"), ev);
                    }
                }
                let active = sp.iter().filter(|m| m.status == "active").count();
                let msg = format!(
                    "futures USDT {} INR {} spot active {}",
                    u.len(),
                    i.len(),
                    active
                );
                d.fut_usdt = u;
                d.fut_inr = i;
                d.spot_active = active;
                d.spot_details = sp;
                d.last_universe_ms = Some(now_ms());
                d.agent_ok("universe_scanner", t0, msg);
            }
            (a, b, c) => {
                let e = format!(
                    "universe refresh failed: {:?} {:?} {:?}",
                    a.err(),
                    b.err(),
                    c.err()
                );
                d.agent_err("universe_scanner", t0, e.clone());
                d.err(e)
            }
        }
        drop(d);
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

async fn rt_loop(c: PublicClient, s: S) {
    loop {
        if !s.read().await.controls.scanner_running {
            s.write().await.agent_stopped("opportunity_scanner");
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        let t0 = now_ms();
        match c.futures_rt().await {
            Ok(rt) => {
                let mut d = s.write().await;
                let ts = rt.ts;
                let mut dup = 0;
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
                    } else {
                        dup += 1; // duplicate / out-of-order snapshot: ignored
                    }
                    d.rt_meta.insert(pair, (p.mp, p.fr, p.pc, p.v));
                }
                d.last_rt_ms = Some(now_ms());
                d.rt_polls += 1;
                let now = now_ms();
                let Desk { calib, hist, .. } = &mut *d;
                calib.resolve(hist, now);
                scan(&mut d);
                let n = d.scanner.len();
                d.agent_ok(
                    "opportunity_scanner",
                    t0,
                    format!("ranked {n} futures (exchange ts {ts}, {dup} stale/dup ignored)"),
                );
            }
            Err(e) => {
                let mut d = s.write().await;
                d.agent_err("opportunity_scanner", t0, e.to_string());
                d.err(format!("rt: {e}"));
            }
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
                        d.spot.insert(x.market, (l, b, a, x.volume.unwrap_or(0.0)));
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
            if !d.controls.scanner_running {
                vec![]
            } else {
                d.scanner
                    .iter()
                    .filter(|r| r["vol_1s"].is_number())
                    .take(TOP_N)
                    .filter_map(|r| r["pair"].as_str().map(String::from))
                    .collect()
            }
        };
        for p in top {
            let t0 = now_ms();
            match c.futures_book(&p, 20).await {
                Ok(b) => {
                    let mut d = s.write().await;
                    d.books.insert(p.clone(), b);
                    let before = d.seq;
                    evaluate(&mut d, &p);
                    let out = if d.seq != before {
                        d.decisions
                            .front()
                            .map(|x| {
                                format!(
                                    "{} {} {} EV net {:.2} bps: {}",
                                    x["pair"].as_str().unwrap_or(""),
                                    x["side"].as_str().unwrap_or(""),
                                    x["decision"].as_str().unwrap_or(""),
                                    x["ev_net_bps"].as_f64().unwrap_or(f64::NAN),
                                    x["reason"].as_str().unwrap_or("")
                                )
                            })
                            .unwrap_or_default()
                    } else {
                        format!("{p}: no forecast (insufficient data)")
                    };
                    d.agent_ok("quant_brain", t0, out.clone());
                    d.agent_ok("risk_governor", t0, out);
                }
                Err(e) => {
                    let mut d = s.write().await;
                    d.agent_err("quant_brain", t0, format!("book {p}: {e}"));
                    d.err(format!("book {p}: {e}"));
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Books for open paper positions, independent of the scanner switch.
async fn position_book_loop(c: PublicClient, s: S) {
    loop {
        let pairs: Vec<String> = {
            let d = s.read().await;
            let mut v: Vec<String> = d
                .positions
                .iter()
                .map(|p| p.pos.order.pair.clone())
                .collect();
            v.dedup();
            v
        };
        for p in pairs {
            match c.futures_book(&p, 20).await {
                Ok(b) => {
                    s.write().await.books.insert(p, b);
                }
                Err(e) => s.write().await.err(format!("position book {p}: {e}")),
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The independent 120 s execution timer (server side, not the browser).
async fn lifecycle_loop(s: S) {
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tick.tick().await;
        let t0 = now_ms();
        let mut d = s.write().await;
        let open = d.positions.len();
        lifecycle_tick(&mut d);
        let msg = format!(
            "{open} open paper position(s); {} closed this session",
            d.closed.len()
        );
        d.agent_ok("execution_manager", t0, msg);
    }
}

/// Instrument metadata: one request per second. Top candidates first (refresh
/// after 10 min), then a round-robin over the whole futures universe.
async fn meta_loop(c: PublicClient, s: S) {
    loop {
        let next: Option<(String, String)> = {
            let mut d = s.write().await;
            let now = now_ms();
            let stale =
                |d: &Desk, k: &str| d.instruments.get(k).is_none_or(|x| now - x.0 > 600_000);
            let top = d
                .scanner
                .iter()
                .take(TOP_N * 2)
                .filter_map(|r| r["pair"].as_str().map(String::from))
                .find(|p| stale(&d, &format!("{p}|{TRADE_MARGIN}")));
            match top {
                Some(p) => Some((p, TRADE_MARGIN.to_string())),
                None => {
                    let mut all: Vec<(String, String)> = d
                        .fut_inr
                        .iter()
                        .map(|p| (p.clone(), "INR".to_string()))
                        .chain(d.fut_usdt.iter().map(|p| (p.clone(), "USDT".to_string())))
                        .filter(|(p, m)| !d.instruments.contains_key(&format!("{p}|{m}")))
                        .collect();
                    all.sort();
                    if all.is_empty() {
                        None
                    } else {
                        d.meta_cursor = (d.meta_cursor + 1) % all.len();
                        Some(all[d.meta_cursor].clone())
                    }
                }
            }
        };
        if let Some((p, m)) = next {
            let t0 = now_ms();
            match c.futures_instrument(&p, &m).await {
                Ok(i) => {
                    let mut d = s.write().await;
                    let key = format!("{p}|{m}");
                    if let Some((_, old)) = d.instruments.get(&key) {
                        if old.exit_only != i.exit_only || old.status != i.status {
                            let ev = json!({"ts_ms": now_ms(), "kind": "instrument_status_change", "pair": p, "margin": m,
                                "status": i.status, "exit_only": i.exit_only});
                            d.events.push_front(ev.clone());
                            d.audit("universe", format!("{p} {m} status/exit-only change"), ev);
                        }
                    }
                    d.instruments.insert(key, (now_ms(), i));
                    let n = d.instruments.len();
                    d.agent_ok(
                        "metadata",
                        t0,
                        format!("{p} ({m}) metadata; {n} instrument records cached"),
                    );
                }
                Err(e) => {
                    let mut d = s.write().await;
                    d.agent_err("metadata", t0, format!("{p} {m}: {e}"));
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

const MTF: [(&str, ContextTf, i64, i64); 6] = [
    ("1", ContextTf::M1, 60_000, 180),
    ("5", ContextTf::M5, 300_000, 200),
    ("15", ContextTf::M15, 900_000, 200),
    ("60", ContextTf::H1, 3_600_000, 200),
    ("1D", ContextTf::D1, 86_400_000, 60),
    ("3D", ContextTf::D3, 259_200_000, 40),
];

/// Multi-timeframe context (3d, 2d, 1d, 1h, 15m, 5m, 1m) per candidate, from
/// exchange candles (completed candles only; 2d aggregated from two completed 1d).
async fn mtf_loop(c: PublicClient, s: S) {
    loop {
        let next: Option<String> = {
            let d = s.read().await;
            let now = now_ms();
            if !d.controls.scanner_running {
                None
            } else {
                d.scanner
                    .iter()
                    .take(TOP_N)
                    .filter_map(|r| r["pair"].as_str().map(String::from))
                    .filter(|p| d.mtf.get(p).is_none_or(|x| now - x.0 > 120_000))
                    .min_by_key(|p| d.mtf.get(p).map_or(0, |x| x.0))
            }
        };
        let Some(pair) = next else {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        };
        let t0 = now_ms();
        let now_s = now_ms() / 1000;
        let mut ctx: BTreeMap<ContextTf, features::TfContext> = BTreeMap::new();
        let mut summary = serde_json::Map::new();
        let mut errs = vec![];
        let mut daily = vec![];
        for (res, tf, w, n) in MTF {
            match c
                .futures_candles(&pair, res, now_s - n * w / 1000, now_s)
                .await
            {
                Ok(cs) => {
                    let mc = to_mcandles(&cs, w, now_ms());
                    let done = mc.iter().filter(|x| x.complete).count();
                    if tf == ContextTf::D1 {
                        daily = mc.clone();
                    }
                    match features::tf_context(tf, &mc) {
                        Some(x) => {
                            summary.insert(tf.label().into(), json!({"completed": done, "ctx": x}));
                            ctx.insert(tf, x);
                        }
                        None => {
                            summary.insert(tf.label().into(), json!({"completed": done, "ctx": null, "note": "fewer than 3 completed candles"}));
                        }
                    }
                }
                Err(e) => errs.push(format!("{res}: {e}")),
            }
        }
        let d2 = qe_math::timeframe::aggregate_2d(&daily);
        match features::tf_context(ContextTf::D2, &d2) {
            Some(x) => {
                summary.insert("2d".into(), json!({"completed": d2.len(), "ctx": x, "source": "aggregated from two completed 1d candles"}));
                ctx.insert(ContextTf::D2, x);
            }
            None => {
                summary.insert("2d".into(), json!({"completed": d2.len(), "ctx": null}));
            }
        }
        let mut d = s.write().await;
        let n = ctx.len();
        d.ctx.insert(pair.clone(), ctx);
        let now = now_ms();
        d.mtf.insert(
            pair.clone(),
            (
                now,
                json!({"pair": pair, "computed_ms": now, "timeframes": summary, "errors": errs}),
            ),
        );
        if errs.is_empty() {
            d.agent_ok(
                "instrument_analyst",
                t0,
                format!("{pair}: {n}/7 timeframe contexts"),
            );
        } else {
            d.agent_err(
                "instrument_analyst",
                t0,
                format!("{pair}: {}", errs.join("; ")),
            );
        }
        drop(d);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn monitor_loop(c: PublicClient, s: S) {
    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let t0 = now_ms();
        let breaker = c.breaker_open().await;
        let mut d = s.write().await;
        let now = now_ms();
        refresh_sim(&mut d, now);
        let st = d.calib.status();
        let calib_msg = format!(
            "calibration n={} brier={} ece={}: {}",
            st["n_resolved"],
            st["brier"],
            st["ece_10bin"],
            st["verdict"].as_str().unwrap_or("")
        );
        d.agent_ok("strategy_critic", t0, calib_msg);
        let eq = d.cap.equity;
        let stage = format!("{:?}", d.cap.stage);
        d.agent_ok(
            "capital_manager",
            t0,
            format!("paper equity INR {eq:.4}, stage {stage}"),
        );
        let stale = !d.rt_fresh();
        let msg = format!(
            "rt {} · breaker {} · errors {} · store written {} errors {}",
            if stale { "STALE" } else { "fresh" },
            if breaker { "OPEN" } else { "closed" },
            d.errors,
            d.store.written,
            d.store.errors
        );
        if stale || breaker {
            d.agent_err("system_monitor", t0, msg);
        } else {
            d.agent_ok("system_monitor", t0, msg);
        }
        d.store.append(
            "equity",
            &json!({"ts_ms": now, "equity": eq, "paper": true, "kind": "heartbeat"}),
        );
    }
}

// ---------------------------------------------------------------- views

fn controls_json(d: &Desk) -> Value {
    json!({"scanner_running": d.controls.scanner_running, "paper_running": d.controls.paper_running,
        "strategy_enabled": d.controls.strategy_enabled, "entries_paused": d.gov.entries_paused,
        "killed": d.gov.killed})
}

fn health_json(d: &Desk) -> Value {
    let now = now_ms();
    json!({"started_ms": d.started_ms, "last_universe_ms": d.last_universe_ms, "last_rt_ms": d.last_rt_ms,
        "last_spot_ticker_ms": d.last_spot_ms, "rt_polls": d.rt_polls, "errors": d.errors,
        "last_error": d.last_error, "stale": d.last_rt_ms.is_none_or(|t| now - t > 5_000),
        "feed": d.feed, "instruments_cached": d.instruments.len(), "store_dir": d.store.dir.display().to_string(),
        "store_written": d.store.written, "store_errors": d.store.errors})
}

fn universe_counts(d: &Desk) -> Value {
    json!({"futures_active": d.fut_usdt.union(&d.fut_inr).count(), "futures_usdt": d.fut_usdt.len(),
        "futures_inr": d.fut_inr.len(), "futures_tracked": d.hist.len(), "spot_active": d.spot_active,
        "spot_with_ticker": d.spot.len(), "refreshed_ms": d.last_universe_ms,
        "instrument_records": d.instruments.len()})
}

/// Readiness flags, each from an observed runtime condition (not file existence).
fn readiness(d: &Desk) -> Value {
    let fut = d.fut_usdt.union(&d.fut_inr).count();
    let mtf_full = d.ctx.values().any(|c| c.len() >= 6);
    let gov_seen = d.decisions.iter().any(|x| x["instrument"].is_object());
    json!({
        "COINDCX_PUBLIC_DATA_READY": d.rt_fresh() && d.last_universe_ms.is_some(),
        "ALL_INSTRUMENT_SCANNER_READY": fut > 0 && d.scanner.len() * 10 >= fut * 9,
        "MULTI_TIMEFRAME_ENGINE_READY": mtf_full,
        "FORECAST_ENGINE_READY": !d.forecasts.is_empty(),
        "FORECAST_CALIBRATED": d.calib.calibrated(),
        "MATHEMATICAL_DECISION_ENGINE_READY": !d.decisions.is_empty(),
        "RISK_GOVERNOR_READY": gov_seen,
        "PAPER_EXECUTION_READY": d.agents.get("execution_manager").is_some_and(|a| a.status == "ok"),
        "AUTHENTICATED_ACCOUNT_READY": false,
        "LIVE_ORDER_ADAPTER_READY": false,
        "LIVE_EXECUTION_VALIDATED": false,
        "notes": "authenticated endpoints are never called by this build; live execution is refused",
    })
}

fn state_json(d: &Desk) -> Value {
    let now = now_ms();
    json!({
        "server_time_ms": now, "mode": "paper",
        "live_orders_enabled": qe_coindcx::orders::LiveGate::default().enabled(),
        "health": health_json(d), "universe": universe_counts(d),
        "scanner": d.scanner.iter().take(60).collect::<Vec<_>>(),
        "opportunities": d.opportunities,
        "positions": d.positions.iter().map(|p| position_json(p, now)).collect::<Vec<_>>(),
        "fills": d.fills.iter().take(50).collect::<Vec<_>>(),
        "closed": d.closed.iter().take(50).collect::<Vec<_>>(),
        "account": account_json(d), "target": target_json(d, now),
        "agents": d.agents.values().collect::<Vec<_>>(), "controls": controls_json(d),
        "calibration": d.calib.status(), "readiness": readiness(d),
        "audit": d.audit,
    })
}

async fn h_state(a: AxState<App>) -> Json<Value> {
    Json(state_json(&*a.s.read().await))
}

async fn h_status(a: AxState<App>) -> Json<Value> {
    let d = a.s.read().await;
    Json(json!({"health": health_json(&d), "mode": "paper",
        "live_orders_enabled": qe_coindcx::orders::LiveGate::default().enabled(),
        "readiness": readiness(&d), "calibration": d.calib.status(), "controls": controls_json(&d)}))
}

fn eligibility(d: &Desk, pair: &str, m: &str) -> (Option<bool>, String) {
    let Some((_, i)) = d.instruments.get(&format!("{pair}|{m}")) else {
        return (None, "metadata not fetched yet".into());
    };
    if !i.is_tradable() {
        return (
            Some(false),
            format!("status {} exit_only {}", i.status, i.exit_only),
        );
    }
    if m != TRADE_MARGIN {
        return (
            Some(false),
            format!("paper account is INR; trades {TRADE_MARGIN}-margined contracts only"),
        );
    }
    let Some(h) = d.hist.get(pair) else {
        return (None, "no price yet".into());
    };
    let (Some(sig), Some(fx), Some(&(_, last, _))) = (sigma_per_s(h), d.fx_usdtinr(), h.back())
    else {
        return (None, "volatility/fx not yet estimable".into());
    };
    let c = capability(i);
    let stop = K_STOP * sig * HORIZON_S.sqrt();
    let cost = 2.0 * c.taker_fee + 2.0 * d.settings.paper_slippage_bps / 1e4;
    let min_notional = c.min_notional.max(c.min_quantity * last);
    let risk_at_min = min_notional * (stop + cost) * fx;
    let budget = d.cap.equity * d.gov.effective_risk_fraction();
    if risk_at_min > budget {
        (
            Some(false),
            format!("min-size risk INR {risk_at_min:.3} > risk budget INR {budget:.3}"),
        )
    } else {
        (
            Some(true),
            format!("min-size risk INR {risk_at_min:.3} <= budget INR {budget:.3}"),
        )
    }
}

async fn h_universe(a: AxState<App>) -> Json<Value> {
    let d = a.s.read().await;
    let rank: HashMap<&str, (usize, &Value)> = d
        .scanner
        .iter()
        .enumerate()
        .filter_map(|(i, r)| Some((r["pair"].as_str()?, (i + 1, r))))
        .collect();
    let spot: Vec<Value> = d
        .spot_details
        .iter()
        .filter(|m| m.status == "active")
        .map(|m| {
            let t = d.spot.get(&m.coindcx_name);
            let spread = t.and_then(|x| {
                (x.1 > 0.0 && x.2 > x.1).then(|| qe_math::micro::spread_bps(x.1, x.2))
            });
            json!({"market": m.coindcx_name, "pair": m.pair, "base": m.base_currency_short_name,
                "quote": m.target_currency_short_name, "status": m.status, "min_notional": m.min_notional,
                "last_price": t.map(|x| x.0), "spread_bps": spread, "volume": t.map(|x| x.3)})
        })
        .collect();
    let mut fut = vec![];
    for (m, set) in [("USDT", &d.fut_usdt), ("INR", &d.fut_inr)] {
        let mut v: Vec<&String> = set.iter().collect();
        v.sort();
        for p in v {
            let meta = d.instruments.get(&format!("{p}|{m}")).map(|x| &x.1);
            let r = rank.get(p.as_str());
            let (elig, why) = eligibility(&d, p, m);
            fut.push(json!({"pair": p, "margin": m, "status": meta.map(|i| i.status.clone()),
                "last": r.map(|x| &x.1["last"]), "volume_24h": r.map(|x| &x.1["volume_24h"]),
                "spread_bps": r.map(|x| &x.1["spread_bps"]), "vol_1s": r.map(|x| &x.1["vol_1s"]),
                "obi": r.map(|x| &x.1["obi"]), "max_leverage_long": meta.map(|i| i.max_leverage_long),
                "max_leverage_short": meta.map(|i| i.max_leverage_short), "min_notional": meta.map(|i| i.min_notional),
                "taker_fee": meta.map(|i| i.taker_fee), "maker_fee": meta.map(|i| i.maker_fee),
                "exit_only": meta.map(|i| i.exit_only), "eligible": elig, "eligibility": why,
                "rank": r.map(|x| x.0), "score": r.map(|x| &x.1["score"])}));
        }
    }
    Json(json!({"spot": spot, "futures": fut, "events": d.events}))
}

async fn h_scanner(a: AxState<App>) -> Json<Value> {
    Json(json!(a.s.read().await.scanner))
}
async fn h_opps(a: AxState<App>) -> Json<Value> {
    Json(json!(a.s.read().await.opportunities))
}
async fn h_forecasts(a: AxState<App>) -> Json<Value> {
    let d = a.s.read().await;
    let mut v: Vec<&Value> = d.forecasts.values().collect();
    v.sort_by_key(|f| -f["ts_ms"].as_i64().unwrap_or(0));
    Json(json!(v))
}
async fn h_calibration(a: AxState<App>) -> Json<Value> {
    Json(a.s.read().await.calib.status())
}
async fn h_decisions(a: AxState<App>) -> Json<Value> {
    Json(json!(a.s.read().await.decisions))
}

#[derive(serde::Deserialize)]
struct Lim {
    limit: Option<usize>,
}
async fn h_audit(a: AxState<App>, q: Query<Lim>) -> Json<Value> {
    let d = a.s.read().await;
    let n = q.limit.unwrap_or(200);
    Json(json!(d.audit.iter().take(n).collect::<Vec<_>>()))
}
async fn h_positions(a: AxState<App>) -> Json<Value> {
    let d = a.s.read().await;
    let now = now_ms();
    Json(
        json!({"open": d.positions.iter().map(|p| position_json(p, now)).collect::<Vec<_>>(),
        "closed": d.closed, "fills": d.fills, "incidents": d.incidents}),
    )
}
async fn h_capital(a: AxState<App>) -> Json<Value> {
    let d = a.s.read().await;
    let mut acct = account_json(&d);
    acct["target"] = target_json(&d, now_ms());
    acct["equity_curve"] = json!(d.equity_curve);
    Json(acct)
}
async fn h_agents(a: AxState<App>) -> Json<Value> {
    Json(json!(a.s.read().await.agents.values().collect::<Vec<_>>()))
}
async fn h_risk(a: AxState<App>) -> Json<Value> {
    let d = a.s.read().await;
    let g = &d.gov;
    Json(
        json!({"mode": format!("{:?}", g.policy.mode), "policy": policy_json(&d), "ceilings": {
            "growth": d.settings.growth, "protective": d.settings.protective},
        "effective_risk_fraction": g.effective_risk_fraction(), "stage_start_equity": g.stage_start_equity,
        "session_realised_pnl": g.session_realised_pnl,
        "session_loss_limit": g.policy.max_session_loss_fraction * g.stage_start_equity,
        "high_water_mark": g.high_water_mark,
        "hwm_floor": g.high_water_mark * (1.0 - g.policy.max_drawdown_from_hwm),
        "consecutive_losses": g.consecutive_losses, "open_positions": g.open_positions,
        "entries_paused": g.entries_paused, "killed": g.killed, "final_target": g.final_target,
        "note": "ceilings are hard limits: config may only tighten them (RiskPolicy::clamped); no endpoint loosens them"}),
    )
}

#[derive(serde::Deserialize)]
struct InstQ {
    margin: Option<String>,
    resolution: Option<String>,
}

async fn h_instrument(a: AxState<App>, Path(pair): Path<String>, q: Query<InstQ>) -> Json<Value> {
    let d = a.s.read().await;
    let m = q.margin.clone().unwrap_or_else(|| TRADE_MARGIN.into());
    let meta = d.instruments.get(&format!("{pair}|{m}")).map(|x| &x.1);
    let book = d.books.get(&pair).map(|b| {
        json!({"ts": b.ts, "vs": b.vs, "bids": b.bids.iter().take(10).collect::<Vec<_>>(),
            "asks": b.asks.iter().take(10).collect::<Vec<_>>(), "sane": b.is_sane()})
    });
    let (elig, why) = eligibility(&d, &pair, &m);
    let inst = meta
        .map(|i| serde_json::to_value(i).unwrap_or_default())
        .unwrap_or(json!({"pair": pair, "margin": m, "note": "metadata not fetched yet"}));
    Json(json!({
        "instrument": inst, "capability": meta.map(capability), "eligible": elig, "eligibility": why,
        "features": d.scanner.iter().find(|r| r["pair"] == pair.as_str()),
        "forecast": d.forecasts.get(&pair), "book": book, "mtf": d.mtf.get(&pair).map(|x| &x.1),
        "regime": d.regimes.get(&pair),
        "decisions": d.decisions.iter().filter(|x| x["pair"] == pair.as_str()).take(20).collect::<Vec<_>>(),
    }))
}

async fn h_candles(
    a: AxState<App>,
    Path(pair): Path<String>,
    q: Query<InstQ>,
) -> impl IntoResponse {
    let res = q.resolution.clone().unwrap_or_else(|| "1".into());
    let (api_res, w, n): (&str, i64, i64) = match res.as_str() {
        "1" => ("1", 60_000, 240),
        "5" => ("5", 300_000, 240),
        "15" => ("15", 900_000, 240),
        "60" => ("60", 3_600_000, 240),
        "1D" => ("1D", 86_400_000, 120),
        "2D" => ("1D", 86_400_000, 240),
        "3D" => ("3D", 259_200_000, 120),
        _ => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(
                    json!({"error": "resolution must be 1|5|15|60|1D|2D|3D (1s candles need the trade stream)"}),
                ),
            )
        }
    };
    let key = format!("{pair}|{res}");
    if let Some((t, v)) = a.s.read().await.candle_cache.get(&key) {
        if now_ms() - t < 15_000 {
            return (axum::http::StatusCode::OK, Json(v.clone()));
        }
    }
    let now = now_ms();
    match a
        .c
        .futures_candles(&pair, api_res, now / 1000 - n * w / 1000, now / 1000)
        .await
    {
        Ok(cs) => {
            let mut mc = to_mcandles(&cs, w, now);
            if res == "2D" {
                mc = qe_math::timeframe::aggregate_2d(&mc);
            }
            let v = json!(mc
                .iter()
                .map(
                    |c| json!({"time": c.open_ms, "open": c.open, "high": c.high, "low": c.low,
                    "close": c.close, "volume": c.volume, "complete": c.complete})
                )
                .collect::<Vec<_>>());
            a.s.write().await.candle_cache.insert(key, (now, v.clone()));
            (axum::http::StatusCode::OK, Json(v))
        }
        Err(e) => (
            axum::http::StatusCode::BAD_GATEWAY,
            Json(json!({"error": e.to_string()})),
        ),
    }
}

// ---------------------------------------------------------------- controls

fn body(b: &Bytes) -> Value {
    serde_json::from_slice(b).unwrap_or(Value::Null)
}

fn ctl(d: &mut Desk, ok: bool, message: String) -> Json<Value> {
    d.audit("control", message.clone(), json!({"ok": ok}));
    Json(json!({"ok": ok, "message": message, "state": controls_json(d)}))
}

async fn c_scanner(a: AxState<App>, b: Bytes) -> Json<Value> {
    let mut d = a.s.write().await;
    let Some(r) = body(&b)["running"].as_bool() else {
        return ctl(&mut d, false, "body must be {\"running\": bool}".into());
    };
    d.controls.scanner_running = r;
    ctl(&mut d, true, format!("scanner running = {r}"))
}
async fn c_paper(a: AxState<App>, b: Bytes) -> Json<Value> {
    let mut d = a.s.write().await;
    let Some(r) = body(&b)["running"].as_bool() else {
        return ctl(&mut d, false, "body must be {\"running\": bool}".into());
    };
    d.controls.paper_running = r;
    let open = d.positions.len();
    let what = if r { "allowed" } else { "stopped" };
    ctl(
        &mut d,
        true,
        format!("paper trading running = {r} (new entries {what}; {open} open position(s) still managed to exit)"),
    )
}
async fn c_strategy(a: AxState<App>, b: Bytes) -> Json<Value> {
    let mut d = a.s.write().await;
    let Some(r) = body(&b)["enabled"].as_bool() else {
        return ctl(&mut d, false, "body must be {\"enabled\": bool}".into());
    };
    d.controls.strategy_enabled = r;
    ctl(&mut d, true, format!("strategy evaluation enabled = {r}"))
}
async fn c_pause(a: AxState<App>, b: Bytes) -> Json<Value> {
    let mut d = a.s.write().await;
    let Some(p) = body(&b)["paused"].as_bool() else {
        return ctl(&mut d, false, "body must be {\"paused\": bool}".into());
    };
    let r = d.gov.pause_entries(p, now_ms() as u64);
    d.controls.entries_paused = d.gov.entries_paused;
    ctl(&mut d, r.ok, r.message)
}
async fn c_cancel(a: AxState<App>) -> Json<Value> {
    let mut d = a.s.write().await;
    let snapshot: Vec<qe_risk::Position> = d.positions.iter().map(|p| p.pos.clone()).collect();
    let n = qe_risk::cancel_entry_actions(&snapshot).actions.len();
    for idx in 0..d.positions.len() {
        if matches!(
            d.positions[idx].pos.state,
            qe_risk::State::EntrySubmitted | qe_risk::State::Filling
        ) {
            let more = d.positions[idx].pos.on_entry_done_partial();
            run_actions(&mut d, idx, more);
        }
    }
    sweep(&mut d);
    ctl(
        &mut d,
        true,
        format!("cancel entries: {n} outstanding paper entry order(s) cancelled (paper entries fill immediately, so usually 0)"),
    )
}
async fn c_flatten(a: AxState<App>) -> Json<Value> {
    let mut d = a.s.write().await;
    let n = flatten(&mut d, "operator");
    let left = d.positions.len();
    ctl(
        &mut d,
        left == 0,
        format!("emergency flatten: {n} exit action(s); {left} position(s) still open"),
    )
}
async fn c_kill(a: AxState<App>) -> Json<Value> {
    let mut d = a.s.write().await;
    let r = d.gov.kill_switch(now_ms() as u64);
    d.controls.killed = true;
    d.controls.entries_paused = true;
    let n = flatten(&mut d, "kill switch");
    ctl(&mut d, r.ok, format!("{}; {n} exit action(s)", r.message))
}
/// Record a withdrawal at the stage-1 checkpoint only with explicit user
/// confirmation and the reconciled remaining balance. Paper: this moves the paper
/// ledger to stage 2; it never touches a CoinDCX wallet.
async fn c_withdrawal(a: AxState<App>, b: Bytes) -> Json<Value> {
    let v = body(&b);
    let mut d = a.s.write().await;
    let (Some(amount), Some(bal)) = (v["amount"].as_f64(), v["confirmed_balance"].as_f64()) else {
        return ctl(
            &mut d,
            false,
            "body must be {amount, confirmed_balance, user_confirmed}".into(),
        );
    };
    let confirmed = v["user_confirmed"].as_bool().unwrap_or(false);
    match d
        .cap
        .confirm_withdrawal(amount, confirmed, bal, now_ms() as u64)
    {
        Ok(eq) => {
            let (paused, killed) = (d.gov.entries_paused, d.gov.killed);
            let pol = d.settings.protective.clone();
            let t2 = d.settings.stage2_target_inr;
            d.gov = qe_risk::RiskGovernor::new(pol, eq, t2);
            d.gov.entries_paused = paused;
            d.gov.killed = killed;
            d.equity_curve.push((now_ms(), eq));
            ctl(
                &mut d,
                true,
                format!("withdrawal INR {amount} recorded; stage 2 protective from INR {eq} (paper ledger); entries remain paused until resumed"),
            )
        }
        Err(e) => ctl(&mut d, false, e),
    }
}

// ---------------------------------------------------------------- ws

async fn h_ws(a: AxState<App>, ws: WebSocketUpgrade) -> impl IntoResponse {
    let app = a.0.clone();
    ws.on_upgrade(move |sock| ws_push(sock, app))
}

async fn ws_push(mut sock: WebSocket, a: App) {
    loop {
        let msg = {
            let d = a.s.read().await;
            json!({"type": "state", "data": state_json(&d)}).to_string()
        };
        if sock.send(Message::Text(msg)).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr = std::env::var("QE_DESK_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let settings = load_settings();
    let s: S = Arc::new(RwLock::new(Desk::new(settings)));
    {
        let mut d = s.write().await;
        let st = serde_json::to_value(&d.settings).unwrap_or_default();
        let dir = d.store.dir.display().to_string();
        d.audit(
            "startup",
            format!("desk started (paper only); store {dir}"),
            st,
        );
    }
    let c = PublicClient::new();
    tokio::spawn(universe_loop(c.clone(), s.clone()));
    tokio::spawn(rt_loop(c.clone(), s.clone()));
    tokio::spawn(spot_loop(c.clone(), s.clone()));
    tokio::spawn(book_loop(c.clone(), s.clone()));
    tokio::spawn(position_book_loop(c.clone(), s.clone()));
    tokio::spawn(lifecycle_loop(s.clone()));
    tokio::spawn(meta_loop(c.clone(), s.clone()));
    tokio::spawn(mtf_loop(c.clone(), s.clone()));
    tokio::spawn(monitor_loop(c.clone(), s.clone()));
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dashboard/dist");
    let app = Router::new()
        .route("/api/state", get(h_state))
        .route("/api/status", get(h_status))
        .route("/api/universe", get(h_universe))
        .route("/api/scanner", get(h_scanner))
        .route("/api/opportunities", get(h_opps))
        .route("/api/instrument/:pair", get(h_instrument))
        .route("/api/instrument/:pair/candles", get(h_candles))
        .route("/api/forecasts", get(h_forecasts))
        .route("/api/calibration", get(h_calibration))
        .route("/api/decisions", get(h_decisions))
        .route("/api/audit", get(h_audit))
        .route("/api/positions", get(h_positions))
        .route("/api/capital", get(h_capital))
        .route("/api/agents", get(h_agents))
        .route("/api/risk", get(h_risk))
        .route("/api/control/scanner", post(c_scanner))
        .route("/api/control/paper", post(c_paper))
        .route("/api/control/strategy", post(c_strategy))
        .route("/api/control/pause_entries", post(c_pause))
        .route("/api/control/cancel_entries", post(c_cancel))
        .route("/api/control/flatten", post(c_flatten))
        .route("/api/control/kill", post(c_kill))
        .route("/api/control/confirm_withdrawal", post(c_withdrawal))
        .route("/ws", get(h_ws))
        .fallback_service(tower_http::services::ServeDir::new(dist))
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(App { s, c });
    eprintln!("qe-desk listening on http://{addr} (paper mode; live orders disabled)");
    let l = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(l, app).await?;
    Ok(())
}
