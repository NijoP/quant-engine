//! CoinDCX public futures WebSocket stream (`wss://stream.coindcx.com`).
//!
//! Public data only: no API key, no signature, no private channel is ever joined.
//!
//! # API for the desk
//!
//! ```no_run
//! use qe_coindcx::ws::{self, Channel, Depth, WsConfig, WsEvent};
//! # async fn demo() -> anyhow::Result<()> {
//! let mut channels = vec![Channel::CurrentPrices];
//! channels.extend(Channel::all_for("B-BTC_USDT", Depth::D20));
//! let handle = ws::spawn(WsConfig { channels, ..WsConfig::default() })?;
//! let mut rx = handle.subscribe(); // tokio::sync::broadcast::Receiver<WsEvent>
//! while let Ok(ev) = rx.recv().await {
//!     match ev {
//!         WsEvent::Trade(t) => { /* t.instrument, t.price, t.quantity, t.aggressor() */ }
//!         WsEvent::Book(b) => { /* b.view: Some(Arc<BookView>) only while Live */ }
//!         WsEvent::CurrentPrices(cp) => { /* cp.prices[instrument].mark_price / last_price */ }
//!         WsEvent::Candle(c) => { /* in-progress candle; c.is_complete_at(now) */ }
//!         WsEvent::Stale { channel, .. } => { /* stop using that channel's data */ }
//!         _ => {}
//!     }
//! }
//! handle.join(Channel::Trades("B-ETH_USDT".into()))?; // also rejoined after reconnects
//! let health = handle.health();                       // ws::WsHealth, serde-serialisable
//! let book = handle.book("B-BTC_USDT");                // Option<BookView>, None unless Live
//! handle.shutdown();
//! # Ok(()) }
//! ```
//!
//! * [`spawn`] starts one background task (one socket). It connects with the
//!   Socket.IO v5 / Engine.IO v4 protocol, joins every channel, answers Engine.IO
//!   pings, sends the documented 25 s `ping` event, and reconnects with
//!   exponential backoff (500 ms doubling to 30 s, 50–100 % jitter, reset after a
//!   60 s session), rejoining every channel. A socket with no frame for
//!   `pingInterval + pingTimeout` (45 s) is treated as dead.
//! * Every event carries the exchange timestamp (`exchange_ts_ms`) and the local
//!   processing timestamp (`recv_ts_ms`).
//! * Duplicate protection: order books and `currentPrices` by version `vs`;
//!   trades, price ticks and candles by exact payload over a recent window.
//! * Integrity: books follow [`book`] rules (snapshot + strict `vs+1` deltas,
//!   rebuild on gap/crossed/disconnect/stale). [`WsHandle::book`] and
//!   `BookEvent::view` never expose a book that is not `Live`.
//! * Staleness: per channel kind ([`StaleThresholds`]); a stale channel raises
//!   [`WsEvent::Stale`], sets `ChannelHealth::stale`, makes
//!   [`WsHealth::healthy`] false, and invalidates its book. [`WsEvent::Recovered`]
//!   follows when data resumes.
//! * Slow consumers get `broadcast::error::RecvError::Lagged(n)` rather than
//!   stalling the socket; treat it like a gap.
//!
//! # Live-verified protocol facts (2026-11-23) that differ from or extend the docs
//!
//! * Order-book channels send `depth-snapshot` **and** incremental `depth-update`
//!   (zero quantity deletes a level), sharing a per-instrument `vs` that grows by
//!   1 per message. The docs FAQ says "snapshot updates only"; the live feed does not.
//! * Depth payloads name the instrument only as `s: "BTCUSDT"`; see
//!   [`events::depth_symbol`]. So only one depth per instrument can be joined.
//! * `price-change` carries no instrument at all; it is attributed only when
//!   exactly one prices channel is joined (otherwise `instrument: None`, counted
//!   in `unattributed_price_ticks`). Use trades for per-instrument last price.
//! * `new-trade.m == 1` means buyer is maker (seller-initiated), established by
//!   matching trades to the live book.
//! * Candle `open_time`/`close_time` are seconds on the wire (converted to ms);
//!   the feed pushes the in-progress candle and has no closed flag.
//!
//! Captured payloads used by the unit tests live in `src/ws/fixtures/`.

pub mod book;
pub mod client;
pub mod events;
pub mod protocol;

pub use book::{ApplyOutcome, BookState, BookView, OrderBook};
pub use client::{
    backoff_delay, now_ms, spawn, BookEvent, ChannelHealth, RestBootstrap, StaleThresholds,
    WsConfig, WsEvent, WsHandle, WsHealth, DEFAULT_URL,
};
pub use events::{
    Candle, CandleInterval, Channel, ChannelKind, CurrentPrices, Depth, PriceEntry, PriceTick,
    Side, Trade,
};
