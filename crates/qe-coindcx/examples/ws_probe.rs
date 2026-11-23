//! Live probe of the public CoinDCX futures stream (no credentials).
//!
//!   cargo run -p qe-coindcx --example ws_probe -- [seconds] [instrument ...]
//!
//! Joins currentPrices + trades/prices/order book (depth 20)/1m candles for each
//! instrument, forces one reconnect at the midpoint to exercise subscription
//! restoration, then prints per-channel counts, duplicates, gaps, latency
//! (local receive − exchange timestamp) and stale flags, plus a JSON summary.

use qe_coindcx::ws::{self, Channel, Depth, RestBootstrap, WsConfig, WsEvent};
use qe_coindcx::PublicClient;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let mut instruments: Vec<String> = args.collect();
    if instruments.is_empty() {
        instruments = [
            "B-BTC_USDT",
            "B-ETH_USDT",
            "B-SOL_USDT",
            "B-XRP_USDT",
            "B-DOGE_USDT",
        ]
        .map(String::from)
        .to_vec();
    }
    let mut channels = vec![Channel::CurrentPrices];
    for i in &instruments {
        channels.extend(Channel::all_for(i, Depth::D20));
    }
    println!(
        "probe: {} channels on {} instruments for {secs}s",
        channels.len(),
        instruments.len()
    );
    // QE_WS_NO_BOOTSTRAP=1 disables the REST order-book bootstrap.
    let rest_bootstrap = std::env::var_os("QE_WS_NO_BOOTSTRAP")
        .is_none()
        .then(|| RestBootstrap(PublicClient::new()));
    println!("rest bootstrap: {}", rest_bootstrap.is_some());
    let h = ws::spawn(WsConfig {
        channels,
        rest_bootstrap,
        ..WsConfig::default()
    })?;
    let mut rx = h.subscribe();
    let t0 = Instant::now();
    let end = t0 + Duration::from_secs(secs);
    let mid = t0 + Duration::from_secs(secs / 2);
    let mut reconnected = false;
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut outcomes: BTreeMap<String, u64> = BTreeMap::new();
    let mut lagged = 0u64;
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    let at = |t0: Instant| t0.elapsed().as_secs();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(end.into()) => break,
            _ = tick.tick() => {
                let hl = h.health();
                let msgs: u64 = hl.channels.values().map(|c| c.messages).sum();
                println!("t+{:>3}s connected={} session={} msgs={msgs} healthy={}",
                    at(t0), hl.connected, hl.session, hl.healthy());
                if !reconnected && Instant::now() >= mid {
                    println!("t+{:>3}s forcing reconnect", at(t0));
                    h.reconnect()?;
                    reconnected = true;
                }
            }
            ev = rx.recv() => match ev {
                Ok(e) => {
                    let k = match &e {
                        WsEvent::Trade(_) => "trade",
                        WsEvent::Price(_) => "price-change",
                        WsEvent::Book(b) => {
                            *outcomes.entry(format!("{:?}", b.outcome)).or_default() += 1;
                            "book"
                        }
                        WsEvent::CurrentPrices(_) => "currentPrices",
                        WsEvent::Candle(_) => "candle",
                        WsEvent::Connected { session, channels } => {
                            println!("t+{:>3}s CONNECTED session={session} joined={channels}", at(t0));
                            "connected"
                        }
                        WsEvent::Disconnected { reason, retry_in_ms } => {
                            println!("t+{:>3}s DISCONNECTED ({reason}); retry in {retry_in_ms} ms", at(t0));
                            "disconnected"
                        }
                        WsEvent::Stale { channel, silent_ms } => {
                            println!("t+{:>3}s STALE {channel} silent {silent_ms} ms", at(t0));
                            "stale"
                        }
                        WsEvent::Recovered { channel } => {
                            println!("t+{:>3}s RECOVERED {channel}", at(t0));
                            "recovered"
                        }
                    };
                    *counts.entry(k).or_default() += 1;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => lagged += n,
                Err(_) => break,
            }
        }
    }
    let hl = h.health();
    println!(
        "\n{:<42} {:>7} {:>5} {:>5} {:>9} {:>9} {:>6}",
        "channel", "msgs", "dups", "gaps", "mean_lat", "max_lat", "stale"
    );
    for (name, c) in &hl.channels {
        println!(
            "{:<42} {:>7} {:>5} {:>5} {:>9} {:>9} {:>6}",
            name,
            c.messages,
            c.duplicates,
            c.gaps,
            c.mean_latency_ms()
                .map_or("-".into(), |m| format!("{m:.0}ms")),
            c.max_latency_ms.map_or("-".into(), |m| format!("{m}ms")),
            c.stale
        );
    }
    println!("\nevents: {counts:?}");
    println!("book outcomes: {outcomes:?}");
    println!(
        "sessions={} disconnects={} engine_pings={} parse_errors={} unattributed_price_ticks={} unattributed_depth={} rest_bootstraps={} rest_bootstrap_errors={} lagged={lagged}",
        hl.session, hl.disconnects, hl.engine_pings, hl.parse_errors, hl.unattributed_price_ticks, hl.unattributed_depth, hl.rest_bootstraps, hl.rest_bootstrap_errors
    );
    for i in &instruments {
        match h.book(i) {
            Some(b) => println!(
                "book {i}: vs={} bid={:?} ask={:?} spread={:.2}bps levels={}/{}",
                b.version,
                b.best_bid(),
                b.best_ask(),
                b.spread_bps().unwrap_or(f64::NAN),
                b.bids.len(),
                b.asks.len()
            ),
            None => println!("book {i}: not live"),
        }
    }
    println!(
        "\nJSON {}",
        serde_json::json!({ "seconds": secs, "events": counts, "book_outcomes": outcomes, "health": hl, "lagged": lagged })
    );
    h.shutdown();
    tokio::time::sleep(Duration::from_millis(200)).await;
    Ok(())
}
