//! Per-endpoint + global rate limiting with 429 backoff for authenticated calls.
//!
//! Documented limit (FAQ "What are the rate limits applicable on CoinDCX
//! APIs"): 16 requests/s and 960/min. CoinDCX does not publish per-endpoint
//! limits for futures, so the per-endpoint budgets below are our own,
//! deliberately conservative choices. The global budget is half the documented
//! limit and is separate from the public-data limiter in `client.rs`.
//!
//! On HTTP 429 every call is refused for a cooldown that doubles per
//! consecutive 429 (2 s .. 120 s); a successful response resets it.
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub const DOC_LIMIT_PER_SEC: usize = 16;
pub const DOC_LIMIT_PER_MIN: usize = 960;
pub const GLOBAL_PER_SEC: usize = 8;
pub const GLOBAL_PER_MIN: usize = 480;
const BASE_COOLDOWN: Duration = Duration::from_secs(2);
const MAX_COOLDOWN: Duration = Duration::from_secs(120);

/// Self-imposed per-endpoint budget per 60 s (path -> calls).
pub fn endpoint_budget_per_min(path: &str) -> usize {
    match path {
        p if p.ends_with("/orders/create") => 60,
        p if p.ends_with("/update_leverage") => 30,
        p if p.ends_with("/create_tpsl") => 60,
        p if p.ends_with("/positions/exit") => 60,
        _ => 120,
    }
}

#[derive(Debug, Default)]
struct Window {
    hits: VecDeque<Instant>,
}

impl Window {
    fn count_since(&mut self, now: Instant, span: Duration) -> usize {
        while let Some(&t) = self.hits.front() {
            if now.duration_since(t) >= span {
                self.hits.pop_front();
            } else {
                break;
            }
        }
        self.hits.len()
    }
    /// Time until the oldest hit inside `span` expires.
    fn wait(&self, now: Instant, span: Duration) -> Duration {
        self.hits
            .front()
            .map(|t| span.saturating_sub(now.duration_since(*t)))
            .unwrap_or_default()
    }
}

#[derive(Debug, Default)]
pub struct RateLimiter {
    global_sec: Window,
    global_min: Window,
    per_endpoint: HashMap<String, Window>,
    cooldown_until: Option<Instant>,
    consecutive_429: u32,
}

impl RateLimiter {
    /// Reserve one call to `path` at `now`, or return how long to wait.
    pub fn try_acquire(&mut self, path: &str, now: Instant) -> Result<(), Duration> {
        if let Some(t) = self.cooldown_until {
            if t > now {
                return Err(t - now);
            }
        }
        let sec = Duration::from_secs(1);
        let min = Duration::from_secs(60);
        if self.global_sec.count_since(now, sec) >= GLOBAL_PER_SEC {
            return Err(self.global_sec.wait(now, sec));
        }
        if self.global_min.count_since(now, min) >= GLOBAL_PER_MIN {
            return Err(self.global_min.wait(now, min));
        }
        let ep = self.per_endpoint.entry(path.to_string()).or_default();
        if ep.count_since(now, min) >= endpoint_budget_per_min(path) {
            return Err(ep.wait(now, min));
        }
        ep.hits.push_back(now);
        self.global_sec.hits.push_back(now);
        self.global_min.hits.push_back(now);
        Ok(())
    }

    pub fn on_429(&mut self, now: Instant) -> Duration {
        self.consecutive_429 = self.consecutive_429.saturating_add(1);
        let factor = 1u32 << (self.consecutive_429 - 1).min(6);
        let cd = (BASE_COOLDOWN * factor).min(MAX_COOLDOWN);
        self.cooldown_until = Some(now + cd);
        cd
    }

    pub fn on_success(&mut self) {
        self.consecutive_429 = 0;
        self.cooldown_until = None;
    }

    pub fn cooling_down(&self, now: Instant) -> Option<Duration> {
        self.cooldown_until.filter(|t| *t > now).map(|t| t - now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn global_per_second_budget() {
        let mut l = RateLimiter::default();
        let t0 = Instant::now();
        for _ in 0..GLOBAL_PER_SEC {
            l.try_acquire("/a", t0).unwrap();
        }
        let w = l.try_acquire("/a", t0).unwrap_err();
        assert!(w <= Duration::from_secs(1) && w > Duration::ZERO);
        l.try_acquire("/a", t0 + Duration::from_secs(1)).unwrap();
        assert!(GLOBAL_PER_SEC < DOC_LIMIT_PER_SEC && GLOBAL_PER_MIN < DOC_LIMIT_PER_MIN);
    }
    #[test]
    fn per_endpoint_budget() {
        let mut l = RateLimiter::default();
        let path = "/exchange/v1/derivatives/futures/positions/update_leverage";
        let mut t = Instant::now();
        for _ in 0..endpoint_budget_per_min(path) {
            l.try_acquire(path, t).unwrap();
            t += Duration::from_millis(200); // stay under the per-second budget
        }
        assert!(l.try_acquire(path, t).is_err());
        assert!(l.try_acquire("/other", t).is_ok());
    }
    #[test]
    fn backoff_on_429_doubles_and_resets() {
        let mut l = RateLimiter::default();
        let t0 = Instant::now();
        assert_eq!(l.on_429(t0), Duration::from_secs(2));
        assert_eq!(l.on_429(t0), Duration::from_secs(4));
        assert!(l.try_acquire("/a", t0 + Duration::from_secs(1)).is_err());
        for _ in 0..20 {
            l.on_429(t0);
        }
        assert_eq!(l.cooling_down(t0), Some(MAX_COOLDOWN));
        l.on_success();
        assert!(l.try_acquire("/a", t0).is_ok());
    }
}
