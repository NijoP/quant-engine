//! The stream connection task: connect, handshake, join, read, reconnect.

use super::book::{ApplyOutcome, BookState, BookView, OrderBook};
use super::events::{
    depth_symbol, parse_event, Candle, Channel, ChannelKind, CurrentPrices, Depth, MarketMsg,
    PriceTick, Trade, CURRENT_PRICES_CHANNEL,
};
use super::protocol::{self, Frame};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message;

pub const DEFAULT_URL: &str = "wss://stream.coindcx.com";

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Per-kind silence after which a channel is flagged stale. Order books get a
/// snapshot every few seconds on liquid instruments, so they are tight; trades and
/// price-change are naturally bursty on illiquid instruments.
#[derive(Debug, Clone)]
pub struct StaleThresholds {
    pub order_book: Duration,
    pub current_prices: Duration,
    pub candles: Duration,
    pub trades: Duration,
    pub prices: Duration,
}

impl Default for StaleThresholds {
    fn default() -> Self {
        Self {
            order_book: Duration::from_secs(10),
            current_prices: Duration::from_secs(10),
            candles: Duration::from_secs(30),
            trades: Duration::from_secs(120),
            prices: Duration::from_secs(120),
        }
    }
}

impl StaleThresholds {
    pub fn for_kind(&self, k: ChannelKind) -> Duration {
        match k {
            ChannelKind::OrderBook => self.order_book,
            ChannelKind::CurrentPrices => self.current_prices,
            ChannelKind::Candles => self.candles,
            ChannelKind::Trades => self.trades,
            ChannelKind::Prices => self.prices,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WsConfig {
    /// Stream endpoint; the Socket.IO path is appended.
    pub url: String,
    pub channels: Vec<Channel>,
    pub stale: StaleThresholds,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    pub connect_timeout: Duration,
    /// Broadcast buffer; slow receivers see `RecvError::Lagged` rather than
    /// blocking the reader.
    pub broadcast_capacity: usize,
    /// Socket.IO `ping` event cadence from the official sample (None disables).
    pub app_ping_interval: Option<Duration>,
    /// Recent-payload window for duplicate suppression of trades/prices/candles.
    pub dedup_window: usize,
    /// Optional REST bootstrap for books awaiting a snapshot (thin instruments may
    /// go many seconds between stream snapshots). Pass the desk's shared
    /// `PublicClient` so its global rate limiter and circuit breaker apply.
    pub rest_bootstrap: Option<RestBootstrap>,
    /// Minimum time between REST bootstrap attempts for one instrument.
    pub bootstrap_min_interval: Duration,
}

/// Wrapper so the config stays `Debug`.
#[derive(Clone)]
pub struct RestBootstrap(pub crate::PublicClient);

impl std::fmt::Debug for RestBootstrap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RestBootstrap(PublicClient)")
    }
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            url: DEFAULT_URL.into(),
            channels: vec![Channel::CurrentPrices],
            stale: StaleThresholds::default(),
            backoff_initial: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            broadcast_capacity: 8192,
            app_ping_interval: Some(Duration::from_secs(25)),
            dedup_window: 4096,
            rest_bootstrap: None,
            bootstrap_min_interval: Duration::from_secs(5),
        }
    }
}

/// Order-book event: the outcome of applying one depth message, and the book
/// after it when (and only when) the book is `Live`.
#[derive(Debug, Clone, Serialize)]
pub struct BookEvent {
    pub instrument: String,
    pub outcome: ApplyOutcome,
    pub state: BookState,
    pub view: Option<Arc<BookView>>,
}

/// Everything the stream emits, in arrival order.
#[derive(Debug, Clone, Serialize)]
pub enum WsEvent {
    Trade(Trade),
    Price(PriceTick),
    Book(BookEvent),
    CurrentPrices(Arc<CurrentPrices>),
    Candle(Candle),
    /// Socket connected and channels (re)joined.
    Connected {
        session: u64,
        channels: usize,
    },
    /// Socket lost; every book is invalidated until its next snapshot.
    Disconnected {
        reason: String,
        retry_in_ms: u64,
    },
    /// A channel went silent beyond its threshold (books are invalidated).
    Stale {
        channel: String,
        silent_ms: u64,
    },
    /// A previously stale channel delivered data again.
    Recovered {
        channel: String,
    },
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ChannelHealth {
    pub kind: Option<ChannelKind>,
    pub messages: u64,
    pub duplicates: u64,
    /// Version gaps (order book / currentPrices).
    pub gaps: u64,
    pub last_recv_ms: Option<i64>,
    pub last_exchange_ts_ms: Option<i64>,
    /// recv − exchange timestamp, ms (includes clock offset between hosts).
    pub last_latency_ms: Option<i64>,
    pub latency_sum_ms: i64,
    pub latency_n: u64,
    pub max_latency_ms: Option<i64>,
    pub stale: bool,
    pub stale_alerts: u64,
}

impl ChannelHealth {
    pub fn mean_latency_ms(&self) -> Option<f64> {
        (self.latency_n > 0).then(|| self.latency_sum_ms as f64 / self.latency_n as f64)
    }

    fn observe(&mut self, recv: i64, exch: i64) {
        self.messages += 1;
        self.last_recv_ms = Some(recv);
        self.last_exchange_ts_ms = Some(exch);
        let lat = recv - exch;
        self.last_latency_ms = Some(lat);
        self.latency_sum_ms += lat;
        self.latency_n += 1;
        self.max_latency_ms = Some(self.max_latency_ms.map_or(lat, |m| m.max(lat)));
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WsHealth {
    pub connected: bool,
    pub session: u64,
    pub connected_since_ms: Option<i64>,
    pub connect_attempts: u64,
    pub disconnects: u64,
    pub last_frame_ms: Option<i64>,
    pub last_error: Option<String>,
    pub engine_pings: u64,
    pub parse_errors: u64,
    /// price-change events that could not be attributed to an instrument.
    pub unattributed_price_ticks: u64,
    /// Depth messages whose symbol matched no subscribed book.
    pub unattributed_depth: u64,
    /// REST order-book bootstraps requested / failed.
    pub rest_bootstraps: u64,
    pub rest_bootstrap_errors: u64,
    pub channels: BTreeMap<String, ChannelHealth>,
}

impl WsHealth {
    /// True only when connected and no subscribed channel is stale.
    pub fn healthy(&self) -> bool {
        self.connected && self.channels.values().all(|c| !c.stale)
    }
}

enum Command {
    Join(Channel),
    Leave(Channel),
    Reconnect,
    Shutdown,
}

/// Handle to a running stream. Cloneable; dropping all handles does not stop
/// the task, call [`WsHandle::shutdown`].
#[derive(Clone)]
pub struct WsHandle {
    events: broadcast::Sender<WsEvent>,
    health: Arc<Mutex<WsHealth>>,
    books: Arc<Mutex<HashMap<String, OrderBook>>>,
    cmd: mpsc::UnboundedSender<Command>,
}

impl WsHandle {
    /// A new receiver of every subsequent event.
    pub fn subscribe(&self) -> broadcast::Receiver<WsEvent> {
        self.events.subscribe()
    }

    /// Snapshot of connection and per-channel health.
    pub fn health(&self) -> WsHealth {
        self.health.lock().expect("health lock").clone()
    }

    /// Current book for an instrument, `None` unless `Live`.
    pub fn book(&self, instrument: &str) -> Option<BookView> {
        self.books
            .lock()
            .expect("books lock")
            .get(instrument)
            .and_then(OrderBook::view)
    }

    /// Join a channel now and after every reconnect.
    pub fn join(&self, ch: Channel) -> Result<()> {
        self.cmd
            .send(Command::Join(ch))
            .map_err(|_| anyhow!("stream task stopped"))
    }

    pub fn leave(&self, ch: Channel) -> Result<()> {
        self.cmd
            .send(Command::Leave(ch))
            .map_err(|_| anyhow!("stream task stopped"))
    }

    /// Drop the socket and reconnect (books invalidated, channels rejoined).
    pub fn reconnect(&self) -> Result<()> {
        self.cmd
            .send(Command::Reconnect)
            .map_err(|_| anyhow!("stream task stopped"))
    }

    pub fn shutdown(&self) {
        let _ = self.cmd.send(Command::Shutdown);
    }
}

/// Start the stream task on the current Tokio runtime.
pub fn spawn(cfg: WsConfig) -> Result<WsHandle> {
    validate_channels(&cfg.channels)?;
    let (tx, _) = broadcast::channel(cfg.broadcast_capacity.max(16));
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (boot_tx, boot_rx) = mpsc::unbounded_channel();
    let handle = WsHandle {
        events: tx,
        health: Arc::new(Mutex::new(WsHealth::default())),
        books: Arc::new(Mutex::new(HashMap::new())),
        cmd: cmd_tx,
    };
    let mut state = State::new(cfg, handle.clone(), boot_tx);
    tokio::spawn(async move { state.run(cmd_rx, boot_rx).await });
    Ok(handle)
}

/// One order-book depth per instrument: depth messages carry only the symbol, so
/// two depths for one instrument could not be told apart.
fn validate_channels<'a>(chs: impl IntoIterator<Item = &'a Channel>) -> Result<()> {
    let mut seen = HashMap::new();
    for c in chs {
        if let Channel::OrderBook(i, d) = c {
            if let Some(prev) = seen.insert(depth_symbol(i), *d) {
                if prev != *d {
                    bail!("two order-book depths subscribed for {i}");
                }
            }
        }
    }
    Ok(())
}

struct Dedup {
    set: HashSet<u64>,
    order: VecDeque<u64>,
    cap: usize,
}

impl Dedup {
    fn new(cap: usize) -> Self {
        Self {
            set: HashSet::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// True if this exact (event, raw payload) was seen within the window.
    fn seen(&mut self, event: &str, raw: &str) -> bool {
        let mut h = DefaultHasher::new();
        event.hash(&mut h);
        raw.hash(&mut h);
        let k = h.finish();
        if !self.set.insert(k) {
            return true;
        }
        self.order.push_back(k);
        if self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        false
    }
}

/// Exponential backoff `initial * 2^attempt`, capped at `max`, with jitter in
/// [50%, 100%] of that value.
pub fn backoff_delay(initial: Duration, max: Duration, attempt: u32) -> Duration {
    let base = initial.saturating_mul(1u32 << attempt.min(20)).min(max);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let frac = 0.5 + (nanos % 1000) as f64 / 2000.0;
    base.mul_f64(frac)
}

fn depth_of(levels: usize) -> Depth {
    match levels {
        10 => Depth::D10,
        20 => Depth::D20,
        _ => Depth::D50,
    }
}

struct State {
    cfg: WsConfig,
    h: WsHandle,
    subs: BTreeSet<Channel>,
    /// depth symbol -> instrument
    depth_map: HashMap<String, String>,
    dedup: Dedup,
    prices_version: Option<u64>,
    joined_at: HashMap<String, Instant>,
    boot_tx: mpsc::UnboundedSender<BootResult>,
    boot_inflight: HashSet<String>,
    boot_last: HashMap<String, Instant>,
}

type BootResult = (String, Result<crate::types::Book>, i64);

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

impl State {
    fn new(cfg: WsConfig, h: WsHandle, boot_tx: mpsc::UnboundedSender<BootResult>) -> Self {
        let subs: BTreeSet<Channel> = cfg.channels.iter().cloned().collect();
        let dedup = Dedup::new(cfg.dedup_window);
        let mut s = Self {
            cfg,
            h,
            subs,
            depth_map: HashMap::new(),
            dedup,
            prices_version: None,
            joined_at: HashMap::new(),
            boot_tx,
            boot_inflight: HashSet::new(),
            boot_last: HashMap::new(),
        };
        s.rebuild_maps();
        s
    }

    fn rebuild_maps(&mut self) {
        self.depth_map.clear();
        let mut books = self.h.books.lock().expect("books lock");
        let mut health = self.h.health.lock().expect("health lock");
        for c in &self.subs {
            if let Channel::OrderBook(i, d) = c {
                self.depth_map.insert(depth_symbol(i), i.clone());
                books
                    .entry(i.clone())
                    .or_insert_with(|| OrderBook::new(i.clone(), d.levels()));
            }
            health.channels.entry(c.name()).or_default().kind = Some(c.kind());
        }
        let live: HashSet<String> = self.subs.iter().map(Channel::name).collect();
        health.channels.retain(|k, _| live.contains(k));
        let book_instr: HashSet<&str> = self
            .subs
            .iter()
            .filter_map(|c| match c {
                Channel::OrderBook(i, _) => Some(i.as_str()),
                _ => None,
            })
            .collect();
        books.retain(|k, _| book_instr.contains(k.as_str()));
    }

    fn emit(&self, e: WsEvent) {
        // Err only means there are no receivers right now.
        let _ = self.h.events.send(e);
    }

    fn health<R>(&self, f: impl FnOnce(&mut WsHealth) -> R) -> R {
        f(&mut self.h.health.lock().expect("health lock"))
    }

    /// Apply a command that only changes the subscription set (used while
    /// disconnected; joins are sent on the next connect). Returns false on shutdown.
    fn apply_offline(&mut self, c: Option<Command>) -> bool {
        match c {
            None | Some(Command::Shutdown) => return false,
            Some(Command::Reconnect) => {}
            Some(Command::Join(ch)) => {
                if validate_channels(self.subs.iter().chain([&ch])).is_ok() {
                    self.subs.insert(ch);
                    self.rebuild_maps();
                }
            }
            Some(Command::Leave(ch)) => {
                self.subs.remove(&ch);
                self.rebuild_maps();
            }
        }
        true
    }

    async fn run(
        &mut self,
        mut cmd_rx: mpsc::UnboundedReceiver<Command>,
        mut boot_rx: mpsc::UnboundedReceiver<BootResult>,
    ) {
        let mut attempt: u32 = 0;
        let mut session: u64 = 0;
        loop {
            self.health(|h| h.connect_attempts += 1);
            let started = Instant::now();
            let result = self.session(&mut cmd_rx, &mut boot_rx, &mut session).await;
            self.on_disconnect();
            let reason = match result {
                Ok(true) => return,
                Ok(false) => "server closed the connection".to_string(),
                Err(e) => format!("{e:#}"),
            };
            if started.elapsed() > Duration::from_secs(60) {
                attempt = 0;
            }
            let delay = backoff_delay(self.cfg.backoff_initial, self.cfg.backoff_max, attempt);
            attempt = attempt.saturating_add(1);
            self.health(|h| {
                h.disconnects += 1;
                h.last_error = Some(reason.clone());
            });
            self.emit(WsEvent::Disconnected {
                reason,
                retry_in_ms: delay.as_millis() as u64,
            });
            let sleep = tokio::time::sleep(delay);
            tokio::pin!(sleep);
            loop {
                tokio::select! {
                    _ = &mut sleep => break,
                    c = cmd_rx.recv() => if !self.apply_offline(c) { return },
                    // Never make a book Live from REST while the stream is down.
                    Some((i, _, _)) = boot_rx.recv() => { self.boot_inflight.remove(&i); }
                }
            }
        }
    }

    fn on_disconnect(&mut self) {
        for b in self.h.books.lock().expect("books lock").values_mut() {
            b.invalidate();
        }
        self.prices_version = None;
        self.joined_at.clear();
        self.health(|h| {
            h.connected = false;
            h.connected_since_ms = None;
        });
    }

    /// One connection lifetime. Ok(true) = shutdown requested.
    async fn session(
        &mut self,
        cmd_rx: &mut mpsc::UnboundedReceiver<Command>,
        boot_rx: &mut mpsc::UnboundedReceiver<BootResult>,
        session: &mut u64,
    ) -> Result<bool> {
        let url = format!(
            "{}{}",
            self.cfg.url.trim_end_matches('/'),
            protocol::SOCKET_IO_PATH
        );
        let (mut ws, _) = tokio::time::timeout(
            self.cfg.connect_timeout,
            tokio_tungstenite::connect_async(url.as_str()),
        )
        .await
        .context("connect timeout")?
        .context("websocket connect")?;

        // Engine.IO open, then Socket.IO connect to the default namespace.
        let open = match next_text(&mut ws, self.cfg.connect_timeout).await? {
            Some(t) => match protocol::decode(&t)? {
                Frame::Open(o) => o,
                f => bail!("expected engine.io open, got {f:?}"),
            },
            None => bail!("closed before engine.io open"),
        };
        let liveness = Duration::from_millis(open.ping_interval + open.ping_timeout);
        ws.send(Message::text(protocol::SIO_CONNECT)).await?;
        loop {
            match next_text(&mut ws, self.cfg.connect_timeout).await? {
                Some(t) => match protocol::decode(&t)? {
                    Frame::SioConnect => break,
                    Frame::SioConnectError(e) => bail!("socket.io connect error: {e}"),
                    Frame::Ping => ws.send(Message::text(protocol::PONG)).await?,
                    _ => {}
                },
                None => bail!("closed before socket.io connect"),
            }
        }
        // Subscription restoration: every channel in the set is (re)joined.
        for c in &self.subs {
            ws.send(Message::text(protocol::encode_join(&c.name())))
                .await?;
        }
        *session += 1;
        let now = Instant::now();
        self.joined_at = self.subs.iter().map(|c| (c.name(), now)).collect();
        let n = self.subs.len();
        let sess = *session;
        self.health(|h| {
            h.connected = true;
            h.session = sess;
            h.connected_since_ms = Some(now_ms());
            h.last_error = None;
        });
        self.emit(WsEvent::Connected {
            session: sess,
            channels: n,
        });

        let mut stale_tick = tokio::time::interval(Duration::from_secs(1));
        let app_ping = self.cfg.app_ping_interval;
        let mut ping_tick = tokio::time::interval(app_ping.unwrap_or(Duration::from_secs(3600)));
        ping_tick.tick().await;
        let mut last_frame = Instant::now();

        loop {
            tokio::select! {
                msg = ws.next() => {
                    let Some(msg) = msg else { return Ok(false) };
                    let msg = msg.context("websocket read")?;
                    last_frame = Instant::now();
                    match msg {
                        Message::Text(t) => {
                            self.health(|h| h.last_frame_ms = Some(now_ms()));
                            match protocol::decode(t.as_str()) {
                                Ok(Frame::Ping) => {
                                    ws.send(Message::text(protocol::PONG)).await?;
                                    self.health(|h| h.engine_pings += 1);
                                }
                                Ok(Frame::Close) | Ok(Frame::SioDisconnect) => return Ok(false),
                                Ok(Frame::Event { name, arg }) => self.on_event(&name, &arg),
                                Ok(_) => {}
                                Err(_) => self.health(|h| h.parse_errors += 1),
                            }
                        }
                        Message::Close(_) => return Ok(false),
                        Message::Ping(p) => ws.send(Message::Pong(p)).await?,
                        _ => {}
                    }
                }
                c = cmd_rx.recv() => match c {
                    None | Some(Command::Shutdown) => {
                        let _ = ws.close(None).await;
                        return Ok(true);
                    }
                    Some(Command::Reconnect) => {
                        let _ = ws.close(None).await;
                        bail!("reconnect requested by operator");
                    }
                    Some(Command::Join(ch)) => {
                        if validate_channels(self.subs.iter().chain([&ch])).is_ok() {
                            ws.send(Message::text(protocol::encode_join(&ch.name()))).await?;
                            self.joined_at.insert(ch.name(), Instant::now());
                            self.subs.insert(ch);
                            self.rebuild_maps();
                        }
                    }
                    Some(Command::Leave(ch)) => {
                        ws.send(Message::text(protocol::encode_leave(&ch.name()))).await?;
                        self.joined_at.remove(&ch.name());
                        self.subs.remove(&ch);
                        self.rebuild_maps();
                    }
                },
                Some((instr, res, recv)) = boot_rx.recv() => self.on_bootstrap(instr, res, recv),
                _ = stale_tick.tick() => {
                    if last_frame.elapsed() > liveness {
                        bail!("no frames for {:?} (engine.io ping timeout)", last_frame.elapsed());
                    }
                    self.check_stale();
                }
                _ = ping_tick.tick(), if app_ping.is_some() => {
                    ws.send(Message::text(protocol::encode_app_ping())).await?;
                }
            }
        }
    }

    fn check_stale(&mut self) {
        let now = now_ms();
        let mut newly_stale = Vec::new();
        {
            let mut h = self.h.health.lock().expect("health lock");
            for c in &self.subs {
                let name = c.name();
                let thr = self.cfg.stale.for_kind(c.kind());
                let ch = h.channels.entry(name.clone()).or_default();
                let silent = match ch.last_recv_ms {
                    Some(t) => Duration::from_millis((now - t).max(0) as u64),
                    None => self
                        .joined_at
                        .get(&name)
                        .map(|t| t.elapsed())
                        .unwrap_or_default(),
                };
                if silent > thr && !ch.stale {
                    ch.stale = true;
                    ch.stale_alerts += 1;
                    newly_stale.push((c.clone(), silent));
                }
            }
        }
        for (c, silent) in newly_stale {
            if let Channel::OrderBook(i, _) = &c {
                if let Some(b) = self.h.books.lock().expect("books lock").get_mut(i) {
                    b.invalidate();
                }
            }
            self.emit(WsEvent::Stale {
                channel: c.name(),
                silent_ms: silent.as_millis() as u64,
            });
        }
    }

    /// Record a message on a channel; emits `Recovered` if it was stale.
    fn touch(&self, channel: &str, recv: i64, exch: i64, duplicate: bool) {
        let recovered = self.health(|h| {
            let ch = h.channels.entry(channel.to_string()).or_default();
            if duplicate {
                ch.duplicates += 1;
                return false;
            }
            ch.observe(recv, exch);
            std::mem::replace(&mut ch.stale, false)
        });
        if recovered {
            self.emit(WsEvent::Recovered {
                channel: channel.to_string(),
            });
        }
    }

    fn maybe_bootstrap(&mut self, instr: &str, depth: usize) {
        let Some(RestBootstrap(client)) = self.cfg.rest_bootstrap.clone() else {
            return;
        };
        if self.boot_inflight.contains(instr)
            || self
                .boot_last
                .get(instr)
                .is_some_and(|t| t.elapsed() < self.cfg.bootstrap_min_interval)
        {
            return;
        }
        self.boot_inflight.insert(instr.to_string());
        self.boot_last.insert(instr.to_string(), Instant::now());
        self.health(|h| h.rest_bootstraps += 1);
        let tx = self.boot_tx.clone();
        let instr = instr.to_string();
        tokio::spawn(async move {
            let res = client.futures_book(&instr, depth as u32).await;
            let _ = tx.send((instr, res, now_ms()));
        });
    }

    fn on_bootstrap(&mut self, instr: String, res: Result<crate::types::Book>, recv: i64) {
        self.boot_inflight.remove(&instr);
        let book = match res {
            Ok(b) => b,
            Err(_) => {
                self.health(|h| h.rest_bootstrap_errors += 1);
                return;
            }
        };
        let ev = {
            let mut books = self.h.books.lock().expect("books lock");
            let Some(b) = books.get_mut(&instr) else {
                return;
            };
            if b.state() == BookState::Live {
                return; // a stream snapshot won the race
            }
            let Some(outcome) = b.apply_rest(&book, recv) else {
                drop(books);
                self.health(|h| h.rest_bootstrap_errors += 1);
                return;
            };
            BookEvent {
                instrument: instr.clone(),
                outcome,
                state: b.state(),
                view: b.view().map(Arc::new),
            }
        };
        self.emit(WsEvent::Book(ev));
    }

    fn bump_gap(&self, channel: &str) {
        self.health(|h| {
            if let Some(c) = h.channels.get_mut(channel) {
                c.gaps += 1;
            }
        });
    }

    fn on_event(&mut self, name: &str, arg: &serde_json::Value) {
        let recv = now_ms();
        // Non-market events (e.g. replies to the app ping) carry no JSON data.
        let Ok(data) = protocol::event_data(arg) else {
            return;
        };
        let msg = match parse_event(name, &data, recv) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(_) => {
                self.health(|h| h.parse_errors += 1);
                return;
            }
        };
        let raw = protocol::event_data_raw(arg).unwrap_or_default();
        match msg {
            MarketMsg::Trade(t) => {
                let ch = Channel::Trades(t.instrument.clone()).name();
                let dup = self.dedup.seen(name, raw);
                self.touch(&ch, recv, t.exchange_ts_ms, dup);
                if !dup {
                    self.emit(WsEvent::Trade(t));
                }
            }
            MarketMsg::Price(mut p) => {
                let prices: Vec<String> = self
                    .subs
                    .iter()
                    .filter_map(|c| match c {
                        Channel::Prices(i) => Some(i.clone()),
                        _ => None,
                    })
                    .collect();
                let dup = self.dedup.seen(name, raw);
                if let [only] = prices.as_slice() {
                    p.instrument = Some(only.clone());
                }
                // Unattributable ticks still prove every prices channel is flowing.
                for i in &prices {
                    self.touch(
                        &Channel::Prices(i.clone()).name(),
                        recv,
                        p.exchange_ts_ms,
                        dup,
                    );
                }
                if !dup {
                    if p.instrument.is_none() {
                        self.health(|h| h.unattributed_price_ticks += 1);
                    }
                    self.emit(WsEvent::Price(p));
                }
            }
            MarketMsg::Depth(d) => {
                let Some(instr) = self.depth_map.get(&d.symbol).cloned() else {
                    self.health(|h| h.unattributed_depth += 1);
                    return;
                };
                let (outcome, state, view, ch_name, depth) = {
                    let mut books = self.h.books.lock().expect("books lock");
                    let Some(b) = books.get_mut(&instr) else {
                        return;
                    };
                    let outcome = b.apply(&d);
                    (
                        outcome,
                        b.state(),
                        b.view().map(Arc::new),
                        Channel::OrderBook(instr.clone(), depth_of(b.depth)).name(),
                        b.depth,
                    )
                };
                if state != BookState::Live {
                    self.maybe_bootstrap(&instr, depth);
                }
                let dup = outcome == ApplyOutcome::Duplicate;
                self.touch(&ch_name, recv, d.exchange_ts_ms, dup);
                if matches!(outcome, ApplyOutcome::Gap { .. }) {
                    self.bump_gap(&ch_name);
                }
                if !dup {
                    self.emit(WsEvent::Book(BookEvent {
                        instrument: instr,
                        outcome,
                        state,
                        view,
                    }));
                }
            }
            MarketMsg::CurrentPrices(cp) => {
                let dup = self.prices_version.is_some_and(|v| cp.version <= v);
                let gap = self.prices_version.is_some_and(|v| cp.version > v + 1);
                if !dup {
                    self.prices_version = Some(cp.version);
                }
                self.touch(CURRENT_PRICES_CHANNEL, recv, cp.exchange_ts_ms, dup);
                if gap {
                    self.bump_gap(CURRENT_PRICES_CHANNEL);
                }
                if !dup {
                    self.emit(WsEvent::CurrentPrices(Arc::new(cp)));
                }
            }
            MarketMsg::Candle(c) => {
                let ch = if !c.channel.is_empty() {
                    c.channel.clone()
                } else if let Some(iv) = c.interval {
                    Channel::Candles(c.instrument.clone(), iv).name()
                } else {
                    return;
                };
                let dup = self.dedup.seen(name, raw);
                self.touch(&ch, recv, c.exchange_ts_ms, dup);
                if !dup {
                    self.emit(WsEvent::Candle(c));
                }
            }
        }
    }
}

async fn next_text(ws: &mut WsStream, timeout: Duration) -> Result<Option<String>> {
    loop {
        let m = tokio::time::timeout(timeout, ws.next())
            .await
            .context("handshake timeout")?;
        match m {
            None => return Ok(None),
            Some(m) => match m? {
                Message::Text(t) => return Ok(Some(t.to_string())),
                Message::Close(_) => return Ok(None),
                _ => continue,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::events::CandleInterval;

    fn state(channels: Vec<Channel>) -> State {
        let (tx, _) = broadcast::channel(4096);
        let (cmd, _) = mpsc::unbounded_channel();
        let (boot_tx, _) = mpsc::unbounded_channel();
        let h = WsHandle {
            events: tx,
            health: Arc::new(Mutex::new(WsHealth::default())),
            books: Arc::new(Mutex::new(HashMap::new())),
            cmd,
        };
        State::new(
            WsConfig {
                channels,
                ..WsConfig::default()
            },
            h,
            boot_tx,
        )
    }

    fn feed(s: &mut State, line: &str) {
        if let Frame::Event { name, arg } = protocol::decode(line).unwrap() {
            s.on_event(&name, &arg);
        }
    }

    #[test]
    fn backoff_grows_and_caps() {
        let i = Duration::from_millis(500);
        let m = Duration::from_secs(30);
        for a in 0..30u32 {
            let d = backoff_delay(i, m, a);
            let base = i.saturating_mul(1 << a.min(20)).min(m);
            assert!(d >= base / 2 && d <= base, "{a}: {d:?}");
        }
    }

    #[test]
    fn rejects_two_depths_for_one_instrument() {
        assert!(validate_channels(&[
            Channel::OrderBook("B-BTC_USDT".into(), Depth::D10),
            Channel::OrderBook("B-BTC_USDT".into(), Depth::D50),
        ])
        .is_err());
        assert!(validate_channels(&Channel::all_for("B-BTC_USDT", Depth::D20)).is_ok());
    }

    #[test]
    fn replayed_capture_drives_books_health_and_dedup() {
        let mut s = state(vec![
            Channel::OrderBook("B-BTC_USDT".into(), Depth::D20),
            Channel::Trades("B-BTC_USDT".into()),
            Channel::Trades("B-ETH_USDT".into()),
            Channel::Prices("B-BTC_USDT".into()),
            Channel::Candles("B-BTC_USDT".into(), CandleInterval::M1),
            Channel::CurrentPrices,
        ]);
        let mut rx = s.h.subscribe();
        for line in include_str!("fixtures/depth_btc.txt").lines() {
            feed(&mut s, line);
        }
        let book = s.h.book("B-BTC_USDT").expect("live book");
        assert!(book.best_bid().unwrap().0 < book.best_ask().unwrap().0);

        let basic = include_str!("fixtures/stream_basic.txt");
        for line in basic.lines() {
            feed(&mut s, line);
        }
        // Replay again: every trade/price/candle payload is now a duplicate.
        for line in basic.lines() {
            feed(&mut s, line);
        }
        for line in include_str!("fixtures/current_prices.txt").lines() {
            feed(&mut s, line);
        }

        let h = s.h.health();
        let trades = &h.channels["B-BTC_USDT@trades-futures"];
        assert_eq!((trades.messages, trades.duplicates), (2, 2));
        let eth = &h.channels["B-ETH_USDT@trades-futures"];
        assert_eq!((eth.messages, eth.duplicates), (1, 1));
        let prices = &h.channels["B-BTC_USDT@prices-futures"];
        assert_eq!((prices.messages, prices.duplicates), (1, 1));
        let candles = &h.channels["B-BTC_USDT_1m-futures"];
        assert_eq!((candles.messages, candles.duplicates), (1, 1));
        let ob = &h.channels["B-BTC_USDT@orderbook@20-futures"];
        assert!(ob.messages >= 25 && ob.gaps == 0, "{ob:?}");
        assert!(h.channels[CURRENT_PRICES_CHANNEL].messages >= 1);
        assert_eq!(h.unattributed_price_ticks, 0);
        assert_eq!(h.parse_errors, 0);

        let mut kinds: BTreeMap<&str, u32> = BTreeMap::new();
        while let Ok(e) = rx.try_recv() {
            let k = match &e {
                WsEvent::Trade(_) => "trade",
                WsEvent::Price(p) => {
                    assert_eq!(p.instrument.as_deref(), Some("B-BTC_USDT"));
                    "price"
                }
                WsEvent::Book(_) => "book",
                WsEvent::CurrentPrices(_) => "cp",
                WsEvent::Candle(_) => "candle",
                _ => "other",
            };
            *kinds.entry(k).or_insert(0) += 1;
        }
        assert_eq!(kinds["trade"], 3);
        assert_eq!(kinds["price"], 1);
        assert_eq!(kinds["candle"], 1);
        assert!(kinds["book"] >= 25);
        assert!(kinds["cp"] >= 1);
    }

    #[test]
    fn price_ticks_unattributed_with_two_prices_channels() {
        let mut s = state(vec![
            Channel::Prices("B-BTC_USDT".into()),
            Channel::Prices("B-ETH_USDT".into()),
        ]);
        for line in include_str!("fixtures/stream_basic.txt").lines() {
            feed(&mut s, line);
        }
        assert_eq!(s.h.health().unattributed_price_ticks, 1);
    }

    #[test]
    fn stale_channel_invalidates_book_and_recovers() {
        let mut s = state(vec![Channel::OrderBook("B-BTC_USDT".into(), Depth::D20)]);
        s.cfg.stale.order_book = Duration::from_millis(0);
        let mut rx = s.h.subscribe();
        let lines: Vec<&str> = include_str!("fixtures/depth_btc.txt").lines().collect();
        for l in &lines[..10] {
            feed(&mut s, l);
        }
        assert!(s.h.book("B-BTC_USDT").is_some());
        std::thread::sleep(Duration::from_millis(5));
        s.health(|h| h.connected = true);
        s.check_stale();
        assert!(
            s.h.book("B-BTC_USDT").is_none(),
            "stale book must not be served"
        );
        assert!(!s.h.health().healthy());
        for l in &lines[10..] {
            feed(&mut s, l);
        }
        let mut saw = (false, false);
        while let Ok(e) = rx.try_recv() {
            match e {
                WsEvent::Stale { .. } => saw.0 = true,
                WsEvent::Recovered { .. } => saw.1 = true,
                _ => {}
            }
        }
        assert_eq!(saw, (true, true));
        assert!(
            s.h.book("B-BTC_USDT").is_some(),
            "rebuilt from next snapshot"
        );
        assert!(s.h.health().healthy());
    }
}
