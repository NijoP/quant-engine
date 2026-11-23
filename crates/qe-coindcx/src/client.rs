//! Public (unauthenticated) CoinDCX REST client with a conservative, adaptive rate
//! limiter, HTTP 429 detection and a circuit breaker whose open/close transitions are
//! recorded as incidents.
//!
//! Documented limits: FAQ "16/sec, 960/min". Pacing:
//! * every client paces itself to at most 8 req/s (one every 125 ms), and all clients
//!   in the process together to at most 12 req/s, below the documented 16/s;
//! * after an HTTP 429 the client's interval doubles (up to 2 s) and then shrinks back
//!   by 5 ms per success (additive recovery), so a throttled endpoint is not hit again
//!   at full rate the moment the breaker closes.
//!
//! Breaker: an HTTP 429, or 5 consecutive *transport* failures (timeouts, connection
//! errors, HTTP 5xx), opens it; every call then fails fast with [`CircuitOpen`] and
//! callers keep or mark their data stale. The cooldown is 30 s, doubling (up to 5 min)
//! when it re-opens within 5 minutes of closing, and never shorter than a server
//! `Retry-After`. After the cooldown the next call is a probe: success closes the
//! breaker, a failure re-opens it. Responses the server answered but the caller could
//! not use (HTTP 4xx other than 429, or a body that does not decode) do not count
//! toward the breaker: they describe the request or the record, not the exchange's
//! health, and must not block unrelated calls.
use crate::types::*;
use anyhow::{anyhow, Result};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

pub const API: &str = "https://api.coindcx.com";
pub const PUBLIC: &str = "https://public.coindcx.com";
/// Per-client minimum spacing between requests (8 req/s).
pub const MIN_INTERVAL: Duration = Duration::from_millis(125);
/// Process-wide minimum spacing over all public clients (12 req/s < documented 16/s).
pub const GLOBAL_MIN_INTERVAL: Duration = Duration::from_millis(83);
/// Largest per-client spacing after repeated 429s.
pub const MAX_INTERVAL: Duration = Duration::from_millis(2_000);
const RECOVER_STEP: Duration = Duration::from_millis(5);
pub const BREAKER_COOLDOWN: Duration = Duration::from_secs(30);
pub const BREAKER_MAX_COOLDOWN: Duration = Duration::from_secs(300);
/// A re-open within this long after a close doubles the next cooldown.
const ESCALATE_WINDOW: Duration = Duration::from_secs(300);
const FAILURES_TO_OPEN: u32 = 5;
const MAX_EVENTS: usize = 200;

/// Error returned without any request when the breaker is open. Callers can test
/// for it with [`is_circuit_open`] to treat it as "try later", not as a property of
/// the requested record.
#[derive(Debug, Clone)]
pub struct CircuitOpen {
    pub client: &'static str,
    pub retry_in: Duration,
}

impl std::fmt::Display for CircuitOpen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "circuit open (exchange throttled/failing) on {} client; retry in {:.1}s",
            self.client,
            self.retry_in.as_secs_f64()
        )
    }
}
impl std::error::Error for CircuitOpen {}

/// An HTTP 429 (also opens the breaker).
#[derive(Debug, Clone)]
pub struct Throttled {
    pub url: String,
    pub cooldown: Duration,
}
impl std::fmt::Display for Throttled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "HTTP 429 from {}; breaker open {:.0}s",
            self.url,
            self.cooldown.as_secs_f64()
        )
    }
}
impl std::error::Error for Throttled {}

/// True when `e` is a fast-fail from an open breaker or an HTTP 429: a transient
/// condition of the exchange connection, not an answer about the requested record.
pub fn is_circuit_open(e: &anyhow::Error) -> bool {
    e.downcast_ref::<CircuitOpen>().is_some() || e.downcast_ref::<Throttled>().is_some()
}

/// One circuit-breaker transition, kept for the caller's incident log.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BreakerEvent {
    pub ts_ms: i64,
    pub client: &'static str,
    /// "open" or "close".
    pub transition: &'static str,
    /// Why it opened (`open`) or "probe succeeded" (`close`).
    pub reason: String,
    /// Cooldown chosen at open (seconds); 0 for close.
    pub cooldown_s: f64,
    /// Time the breaker spent open, including the cooldown (`close` only).
    pub open_for_s: f64,
    /// Calls refused while it was open (`close` only).
    pub rejected_while_open: u64,
    /// Per-client request spacing after this transition (ms).
    pub interval_ms: u64,
}

/// What happened to one request, as the breaker sees it.
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Ok,
    /// HTTP 429, with the server's Retry-After if it sent one.
    Throttled(Option<Duration>),
    /// Timeout, connection error or HTTP 5xx: counts toward opening the breaker.
    Transport(String),
    /// The server answered, but not usefully (4xx other than 429, undecodable body).
    /// Not the exchange's health: never counts toward the breaker.
    Unusable,
}

#[derive(Debug)]
struct Limiter {
    name: &'static str,
    last: Option<Instant>,
    interval: Duration,
    open_until: Option<Instant>,
    opened_at: Option<Instant>,
    last_cooldown: Duration,
    last_close: Option<Instant>,
    rejected: u64,
    consecutive_failures: u32,
    events: VecDeque<BreakerEvent>,
    opens: u64,
}

fn wall_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl Limiter {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            last: None,
            interval: MIN_INTERVAL,
            open_until: None,
            opened_at: None,
            last_cooldown: Duration::ZERO,
            last_close: None,
            rejected: 0,
            consecutive_failures: 0,
            events: VecDeque::new(),
            opens: 0,
        }
    }

    /// Ok(wait before sending) or the fast-fail error while open.
    fn admit(&mut self, now: Instant) -> std::result::Result<Duration, CircuitOpen> {
        if let Some(t) = self.open_until {
            if t > now {
                self.rejected += 1;
                return Err(CircuitOpen {
                    client: self.name,
                    retry_in: t - now,
                });
            }
            // cooldown over: half-open, this call is the probe (its outcome
            // closes or re-opens the breaker)
        }
        let wait = self.last.map_or(Duration::ZERO, |l| {
            (l + self.interval).saturating_duration_since(now)
        });
        self.last = Some(now + wait);
        Ok(wait)
    }

    fn push(&mut self, ev: BreakerEvent) {
        eprintln!(
            "[breaker] {} {} client: {} (cooldown {:.0}s, open {:.1}s, refused {}, interval {} ms)",
            ev.transition,
            ev.client,
            ev.reason,
            ev.cooldown_s,
            ev.open_for_s,
            ev.rejected_while_open,
            ev.interval_ms
        );
        self.events.push_back(ev);
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
    }

    fn open(&mut self, now: Instant, reason: String, retry_after: Option<Duration>) -> Duration {
        let reopen = self.opened_at.is_some();
        let escalate = reopen
            || self
                .last_close
                .is_some_and(|c| now.saturating_duration_since(c) < ESCALATE_WINDOW);
        let base = if escalate {
            (self.last_cooldown * 2).clamp(BREAKER_COOLDOWN, BREAKER_MAX_COOLDOWN)
        } else {
            BREAKER_COOLDOWN
        };
        let cd = retry_after.map_or(base, |r| r.min(BREAKER_MAX_COOLDOWN).max(base));
        self.open_until = Some(now + cd);
        if !reopen {
            self.opened_at = Some(now);
            self.rejected = 0;
        }
        self.last_cooldown = cd;
        self.consecutive_failures = 0;
        self.opens += 1;
        self.push(BreakerEvent {
            ts_ms: wall_ms(),
            client: self.name,
            transition: "open",
            reason: if reopen {
                format!("probe failed: {reason}")
            } else {
                reason
            },
            cooldown_s: cd.as_secs_f64(),
            open_for_s: 0.0,
            rejected_while_open: 0,
            interval_ms: self.interval.as_millis() as u64,
        });
        cd
    }

    /// Record a finished request; returns the cooldown if it opened the breaker.
    fn record(&mut self, now: Instant, out: &Outcome) -> Option<Duration> {
        match out {
            Outcome::Ok | Outcome::Unusable => {
                if *out == Outcome::Ok {
                    self.consecutive_failures = 0;
                    self.interval = self.interval.saturating_sub(RECOVER_STEP).max(MIN_INTERVAL);
                }
                if let Some(at) = self.opened_at.take() {
                    self.open_until = None;
                    self.last_close = Some(now);
                    let refused = std::mem::take(&mut self.rejected);
                    self.push(BreakerEvent {
                        ts_ms: wall_ms(),
                        client: self.name,
                        transition: "close",
                        reason: "probe succeeded".into(),
                        cooldown_s: 0.0,
                        open_for_s: now.saturating_duration_since(at).as_secs_f64(),
                        rejected_while_open: refused,
                        interval_ms: self.interval.as_millis() as u64,
                    });
                }
                None
            }
            Outcome::Throttled(ra) => {
                self.interval = (self.interval * 2).min(MAX_INTERVAL);
                Some(self.open(now, "HTTP 429 (rate limited)".into(), *ra))
            }
            Outcome::Transport(why) => {
                self.consecutive_failures += 1;
                if self.opened_at.is_some() {
                    // the half-open probe failed
                    return Some(self.open(now, why.clone(), None));
                }
                if self.consecutive_failures >= FAILURES_TO_OPEN {
                    let r =
                        format!("{FAILURES_TO_OPEN} consecutive transport failures (last: {why})");
                    return Some(self.open(now, r, None));
                }
                None
            }
        }
    }
}

/// Process-wide pacing shared by every `PublicClient`.
static GLOBAL_LAST: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

fn global_wait(now: Instant) -> Duration {
    let mut g = GLOBAL_LAST.lock().unwrap_or_else(|p| p.into_inner());
    let wait = g.map_or(Duration::ZERO, |l| {
        (l + GLOBAL_MIN_INTERVAL).saturating_duration_since(now)
    });
    *g = Some(now + wait);
    wait
}

/// Snapshot of one client's limiter, for status pages.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LimiterStatus {
    pub client: &'static str,
    pub open: bool,
    pub retry_in_s: f64,
    pub interval_ms: u64,
    pub opens_total: u64,
    pub consecutive_failures: u32,
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
        Self::named("public")
    }

    /// A client with its own pacing and breaker; `name` labels its incidents.
    pub fn named(name: &'static str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent("quant-engine-desk/0.1")
            .build()
            .expect("http client");
        Self {
            http,
            lim: Arc::new(Mutex::new(Limiter::new(name))),
        }
    }

    pub async fn breaker_open(&self) -> bool {
        matches!(self.lim.lock().await.open_until, Some(t) if t > Instant::now())
    }

    /// Time until the breaker admits a probe (zero when closed).
    pub async fn breaker_retry_in(&self) -> Duration {
        self.lim
            .lock()
            .await
            .open_until
            .map_or(Duration::ZERO, |t| {
                t.saturating_duration_since(Instant::now())
            })
    }

    /// Breaker transitions since the last call (oldest first), for incident logs.
    pub async fn take_breaker_events(&self) -> Vec<BreakerEvent> {
        self.lim.lock().await.events.drain(..).collect()
    }

    pub async fn limiter_status(&self) -> LimiterStatus {
        let l = self.lim.lock().await;
        let retry = l.open_until.map_or(Duration::ZERO, |t| {
            t.saturating_duration_since(Instant::now())
        });
        LimiterStatus {
            client: l.name,
            open: retry > Duration::ZERO,
            retry_in_s: retry.as_secs_f64(),
            interval_ms: l.interval.as_millis() as u64,
            opens_total: l.opens,
            consecutive_failures: l.consecutive_failures,
        }
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let wait = self.lim.lock().await.admit(Instant::now())?;
        tokio::time::sleep(wait).await;
        tokio::time::sleep(global_wait(Instant::now())).await;
        let res = self.http.get(url).send().await;
        let (out, outcome): (Result<T>, Outcome) = match res {
            Ok(r) if r.status().as_u16() == 429 => {
                let ra = r
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .map(Duration::from_secs);
                (Err(anyhow!("HTTP 429")), Outcome::Throttled(ra))
            }
            Ok(r) if r.status().is_server_error() => {
                let m = format!("HTTP {} from {url}", r.status());
                (Err(anyhow!(m.clone())), Outcome::Transport(m))
            }
            Ok(r) if !r.status().is_success() => (
                Err(anyhow!("HTTP {} from {url}", r.status())),
                Outcome::Unusable,
            ),
            Ok(r) => match r.bytes().await {
                Ok(b) => match serde_json::from_slice::<T>(&b) {
                    Ok(v) => (Ok(v), Outcome::Ok),
                    Err(e) => (Err(anyhow!("decode {url}: {e}")), Outcome::Unusable),
                },
                Err(e) => {
                    let m = format!("read body {url}: {e}");
                    (Err(anyhow!(m.clone())), Outcome::Transport(m))
                }
            },
            Err(e) => {
                let m = format!("request {url}: {e}");
                (Err(anyhow!(m.clone())), Outcome::Transport(m))
            }
        };
        let opened = self.lim.lock().await.record(Instant::now(), &outcome);
        match (opened, &outcome) {
            (Some(cd), Outcome::Throttled(_)) => Err(Throttled {
                url: url.to_string(),
                cooldown: cd,
            }
            .into()),
            _ => out,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn t(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn paces_to_min_interval() {
        let b = Instant::now();
        let mut l = Limiter::new("t");
        assert_eq!(l.admit(b).unwrap(), Duration::ZERO);
        // a second call 50 ms later waits the remaining 75 ms
        assert_eq!(l.admit(t(b, 50)).unwrap(), Duration::from_millis(75));
        // and a third right after queues behind the second
        assert_eq!(l.admit(t(b, 50)).unwrap(), Duration::from_millis(200));
    }

    #[test]
    fn decode_and_4xx_never_open_the_breaker() {
        let b = Instant::now();
        let mut l = Limiter::new("t");
        for i in 0..50 {
            assert!(l.record(t(b, i), &Outcome::Unusable).is_none());
        }
        assert!(l.open_until.is_none());
        assert!(l.events.is_empty());
    }

    #[test]
    fn five_transport_failures_open_and_a_probe_closes_with_events() {
        let b = Instant::now();
        let mut l = Limiter::new("meta");
        let to = Outcome::Transport("timeout".into());
        for i in 0..4 {
            assert!(l.record(t(b, i), &to).is_none());
        }
        // a success resets the count
        l.record(t(b, 5), &Outcome::Ok);
        for i in 0..4 {
            assert!(l.record(t(b, 10 + i), &to).is_none());
        }
        assert_eq!(l.record(t(b, 20), &to), Some(BREAKER_COOLDOWN));
        // fast-fail while open, counted
        let e = l.admit(t(b, 1_000)).unwrap_err();
        assert_eq!(e.client, "meta");
        assert!(l.admit(t(b, 2_000)).is_err());
        // after the cooldown the next call is admitted as a probe; success closes
        assert!(l.admit(t(b, 30_100)).is_ok());
        l.record(t(b, 30_200), &Outcome::Ok);
        assert!(l.open_until.is_none());
        let ev: Vec<_> = l.events.iter().map(|e| e.transition).collect();
        assert_eq!(ev, ["open", "close"]);
        let close = &l.events[1];
        assert_eq!(close.rejected_while_open, 2);
        assert!((close.open_for_s - 30.18).abs() < 0.01);
    }

    #[test]
    fn failed_probe_reopens_with_a_longer_cooldown() {
        let b = Instant::now();
        let mut l = Limiter::new("t");
        let to = Outcome::Transport("HTTP 503".into());
        for i in 0..5 {
            l.record(t(b, i), &to);
        }
        assert!(l.admit(t(b, 30_100)).is_ok());
        assert_eq!(l.record(t(b, 30_200), &to), Some(BREAKER_COOLDOWN * 2));
        assert!(l.events[1].reason.starts_with("probe failed"));
        assert!(l.admit(t(b, 31_000)).is_err());
    }

    #[test]
    fn throttle_slows_pacing_escalates_and_honours_retry_after() {
        let b = Instant::now();
        let mut l = Limiter::new("t");
        assert_eq!(
            l.record(b, &Outcome::Throttled(None)),
            Some(BREAKER_COOLDOWN)
        );
        assert_eq!(l.interval, MIN_INTERVAL * 2);
        // probe after cooldown succeeds -> closed
        l.record(t(b, 31_000), &Outcome::Ok);
        assert!(l.open_until.is_none());
        // throttled again within 5 min of the close: cooldown doubles
        assert_eq!(
            l.record(t(b, 60_000), &Outcome::Throttled(None)),
            Some(BREAKER_COOLDOWN * 2)
        );
        // Retry-After longer than the base wins, capped at the max
        let mut m = Limiter::new("t");
        assert_eq!(
            m.record(b, &Outcome::Throttled(Some(Duration::from_secs(45)))),
            Some(Duration::from_secs(45))
        );
        let mut m = Limiter::new("t");
        assert_eq!(
            m.record(b, &Outcome::Throttled(Some(Duration::from_secs(9_999)))),
            Some(BREAKER_MAX_COOLDOWN)
        );
        // a closed breaker that stays quiet for 5 min starts over at the base
        l.record(t(b, 121_000), &Outcome::Ok);
        assert_eq!(
            l.record(t(b, 500_000), &Outcome::Throttled(None)),
            Some(BREAKER_COOLDOWN)
        );
        assert!(l.interval <= MAX_INTERVAL);
        // successes walk the interval back down to the base
        l.record(t(b, 531_000), &Outcome::Ok);
        for i in 0..1_000 {
            l.record(t(b, 600_000 + i), &Outcome::Ok);
        }
        assert_eq!(l.interval, MIN_INTERVAL);
    }

    #[test]
    fn circuit_errors_are_recognisable() {
        let e: anyhow::Error = CircuitOpen {
            client: "x",
            retry_in: Duration::from_secs(3),
        }
        .into();
        assert!(is_circuit_open(&e));
        assert!(e.to_string().contains("circuit open"));
        let e: anyhow::Error = Throttled {
            url: "u".into(),
            cooldown: BREAKER_COOLDOWN,
        }
        .into();
        assert!(is_circuit_open(&e));
        assert!(!is_circuit_open(&anyhow!("decode x: bad")));
    }
}
