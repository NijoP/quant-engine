//! Order-book reconstruction from `depth-snapshot` + `depth-update` with strict
//! version (`vs`) sequencing.
//!
//! Rules (derived from the live stream, see [`super::events::DepthMsg`]):
//! * A snapshot replaces the book and makes it `Live` (unless crossed or older than
//!   the current version).
//! * An update is applied only when the book is `Live` and `vs == last_vs + 1`.
//!   `vs <= last_vs` is a duplicate and is dropped. A gap (`vs > last_vs + 1`)
//!   invalidates the book; it stays `AwaitingSnapshot` until the next snapshot.
//!   Missing updates are never invented.
//! * After applying an update each side is truncated to the subscribed depth:
//!   levels outside the exchange's window are not maintained by the feed.
//! * A crossed book (best bid >= best ask) is treated as an integrity failure.
//! * Disconnects and stale-feed alerts call [`OrderBook::invalidate`].

use super::events::{DepthKind, DepthMsg};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Px(f64);
impl Eq for Px {}
impl PartialOrd for Px {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Px {
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.total_cmp(&o.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum BookState {
    /// No trustworthy book: waiting for the next `depth-snapshot`.
    AwaitingSnapshot,
    /// Book is consistent with an unbroken version sequence since the last snapshot.
    Live,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ApplyOutcome {
    SnapshotApplied,
    UpdateApplied,
    /// `vs` not newer than the current version: dropped.
    Duplicate,
    /// Version gap: book invalidated until next snapshot.
    Gap {
        expected: u64,
        got: u64,
    },
    /// Update received while awaiting a snapshot: ignored.
    IgnoredAwaitingSnapshot,
    /// Book crossed after applying: invalidated.
    Crossed,
}

/// Immutable top-of-book view handed to consumers. Bids descending, asks ascending.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BookView {
    pub instrument: String,
    pub version: u64,
    pub exchange_ts_ms: i64,
    pub recv_ts_ms: i64,
    pub bids: Vec<(f64, f64)>,
    pub asks: Vec<(f64, f64)>,
}

impl BookView {
    pub fn best_bid(&self) -> Option<(f64, f64)> {
        self.bids.first().copied()
    }
    pub fn best_ask(&self) -> Option<(f64, f64)> {
        self.asks.first().copied()
    }
    pub fn mid(&self) -> Option<f64> {
        Some((self.best_bid()?.0 + self.best_ask()?.0) / 2.0)
    }
    pub fn spread_bps(&self) -> Option<f64> {
        let m = self.mid()?;
        Some((self.best_ask()?.0 - self.best_bid()?.0) / m * 10_000.0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BookStats {
    pub snapshots: u64,
    pub updates: u64,
    pub duplicates: u64,
    pub gaps: u64,
    pub crossed: u64,
    pub ignored_awaiting_snapshot: u64,
    pub invalidations: u64,
}

#[derive(Debug, Clone)]
pub struct OrderBook {
    pub instrument: String,
    pub depth: usize,
    bids: BTreeMap<Px, f64>,
    asks: BTreeMap<Px, f64>,
    version: Option<u64>,
    exchange_ts_ms: i64,
    recv_ts_ms: i64,
    state: BookState,
    pub stats: BookStats,
}

impl OrderBook {
    pub fn new(instrument: impl Into<String>, depth: usize) -> Self {
        Self {
            instrument: instrument.into(),
            depth,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            version: None,
            exchange_ts_ms: 0,
            recv_ts_ms: 0,
            state: BookState::AwaitingSnapshot,
            stats: BookStats::default(),
        }
    }

    pub fn state(&self) -> BookState {
        self.state
    }

    pub fn version(&self) -> Option<u64> {
        self.version
    }

    /// Drop the book; the next snapshot rebuilds it. The last version is kept so
    /// that late duplicates are still recognised.
    pub fn invalidate(&mut self) {
        if self.state == BookState::Live {
            self.stats.invalidations += 1;
        }
        self.state = BookState::AwaitingSnapshot;
        self.bids.clear();
        self.asks.clear();
    }

    pub fn apply(&mut self, m: &DepthMsg) -> ApplyOutcome {
        match m.kind {
            DepthKind::Snapshot => {
                if self.state == BookState::Live && self.version.is_some_and(|v| m.version <= v) {
                    self.stats.duplicates += 1;
                    return ApplyOutcome::Duplicate;
                }
                self.bids = m
                    .bids
                    .iter()
                    .filter(|l| l.1 > 0.0)
                    .map(|&(p, q)| (Px(p), q))
                    .collect();
                self.asks = m
                    .asks
                    .iter()
                    .filter(|l| l.1 > 0.0)
                    .map(|&(p, q)| (Px(p), q))
                    .collect();
                self.version = Some(m.version);
                self.stamp(m);
                self.stats.snapshots += 1;
                self.state = BookState::Live;
                self.truncate();
                if self.is_crossed() {
                    self.stats.crossed += 1;
                    self.invalidate();
                    return ApplyOutcome::Crossed;
                }
                ApplyOutcome::SnapshotApplied
            }
            DepthKind::Update => {
                if self.state != BookState::Live {
                    if self.version.is_some_and(|v| m.version <= v) {
                        self.stats.duplicates += 1;
                        return ApplyOutcome::Duplicate;
                    }
                    self.stats.ignored_awaiting_snapshot += 1;
                    return ApplyOutcome::IgnoredAwaitingSnapshot;
                }
                let last = self.version.expect("live book has a version");
                if m.version <= last {
                    self.stats.duplicates += 1;
                    return ApplyOutcome::Duplicate;
                }
                if m.version != last + 1 {
                    self.stats.gaps += 1;
                    self.version = Some(m.version);
                    self.invalidate();
                    return ApplyOutcome::Gap {
                        expected: last + 1,
                        got: m.version,
                    };
                }
                for &(p, q) in &m.bids {
                    if q == 0.0 {
                        self.bids.remove(&Px(p));
                    } else {
                        self.bids.insert(Px(p), q);
                    }
                }
                for &(p, q) in &m.asks {
                    if q == 0.0 {
                        self.asks.remove(&Px(p));
                    } else {
                        self.asks.insert(Px(p), q);
                    }
                }
                self.version = Some(m.version);
                self.stamp(m);
                self.stats.updates += 1;
                self.truncate();
                if self.is_crossed() {
                    self.stats.crossed += 1;
                    self.invalidate();
                    return ApplyOutcome::Crossed;
                }
                ApplyOutcome::UpdateApplied
            }
        }
    }

    fn stamp(&mut self, m: &DepthMsg) {
        self.exchange_ts_ms = m.exchange_ts_ms;
        self.recv_ts_ms = m.recv_ts_ms;
    }

    fn truncate(&mut self) {
        while self.bids.len() > self.depth {
            self.bids.pop_first(); // lowest bid
        }
        while self.asks.len() > self.depth {
            self.asks.pop_last(); // highest ask
        }
    }

    fn is_crossed(&self) -> bool {
        match (self.bids.last_key_value(), self.asks.first_key_value()) {
            (Some((b, _)), Some((a, _))) => b.0 >= a.0,
            _ => false,
        }
    }

    /// Current book, only when `Live`.
    pub fn view(&self) -> Option<BookView> {
        if self.state != BookState::Live {
            return None;
        }
        Some(BookView {
            instrument: self.instrument.clone(),
            version: self.version?,
            exchange_ts_ms: self.exchange_ts_ms,
            recv_ts_ms: self.recv_ts_ms,
            bids: self.bids.iter().rev().map(|(p, q)| (p.0, *q)).collect(),
            asks: self.asks.iter().map(|(p, q)| (p.0, *q)).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::events::{tests::parse_line, MarketMsg};

    fn msg(kind: DepthKind, vs: u64, bids: &[(f64, f64)], asks: &[(f64, f64)]) -> DepthMsg {
        DepthMsg {
            kind,
            symbol: "BTCUSDT".into(),
            version: vs,
            exchange_ts_ms: vs as i64,
            event_ts_ms: None,
            publish_ts_ms: None,
            bids: bids.to_vec(),
            asks: asks.to_vec(),
            recv_ts_ms: 0,
        }
    }

    #[test]
    fn snapshot_then_delta_with_delete() {
        let mut b = OrderBook::new("B-BTC_USDT", 3);
        assert_eq!(
            b.apply(&msg(DepthKind::Update, 9, &[(1.0, 1.0)], &[])),
            ApplyOutcome::IgnoredAwaitingSnapshot
        );
        assert!(b.view().is_none());
        let s = msg(
            DepthKind::Snapshot,
            10,
            &[(99.0, 1.0), (98.0, 2.0), (97.0, 3.0)],
            &[(101.0, 1.0), (102.0, 2.0), (103.0, 3.0)],
        );
        assert_eq!(b.apply(&s), ApplyOutcome::SnapshotApplied);
        let u = msg(
            DepthKind::Update,
            11,
            &[(99.0, 0.0), (99.5, 4.0)],
            &[(101.0, 0.5)],
        );
        assert_eq!(b.apply(&u), ApplyOutcome::UpdateApplied);
        let v = b.view().unwrap();
        assert_eq!(v.bids, vec![(99.5, 4.0), (98.0, 2.0), (97.0, 3.0)]);
        assert_eq!(v.best_ask(), Some((101.0, 0.5)));
        assert_eq!(v.mid(), Some(100.25));
        assert!((v.spread_bps().unwrap() - 1.5 / 100.25 * 1e4).abs() < 1e-9);
        assert_eq!(v.version, 11);
    }

    #[test]
    fn duplicate_and_gap_handling() {
        let mut b = OrderBook::new("X", 5);
        b.apply(&msg(DepthKind::Snapshot, 10, &[(1.0, 1.0)], &[(2.0, 1.0)]));
        assert_eq!(
            b.apply(&msg(DepthKind::Update, 10, &[(1.0, 9.0)], &[])),
            ApplyOutcome::Duplicate
        );
        assert_eq!(
            b.apply(&msg(DepthKind::Snapshot, 9, &[(1.0, 9.0)], &[(2.0, 1.0)])),
            ApplyOutcome::Duplicate
        );
        assert_eq!(b.view().unwrap().bids, vec![(1.0, 1.0)]);
        assert_eq!(
            b.apply(&msg(DepthKind::Update, 12, &[(1.0, 9.0)], &[])),
            ApplyOutcome::Gap {
                expected: 11,
                got: 12
            }
        );
        assert_eq!(b.state(), BookState::AwaitingSnapshot);
        assert!(b.view().is_none());
        assert_eq!(
            b.apply(&msg(DepthKind::Update, 13, &[(1.0, 9.0)], &[])),
            ApplyOutcome::IgnoredAwaitingSnapshot
        );
        assert_eq!(
            b.apply(&msg(DepthKind::Snapshot, 14, &[(1.0, 2.0)], &[(2.0, 1.0)])),
            ApplyOutcome::SnapshotApplied
        );
        assert_eq!(b.stats.gaps, 1);
        assert_eq!(b.stats.duplicates, 2);
    }

    #[test]
    fn crossed_book_is_rejected_and_truncation_keeps_inside_levels() {
        let mut b = OrderBook::new("X", 2);
        assert_eq!(
            b.apply(&msg(DepthKind::Snapshot, 1, &[(3.0, 1.0)], &[(2.0, 1.0)])),
            ApplyOutcome::Crossed
        );
        assert!(b.view().is_none());
        b.apply(&msg(
            DepthKind::Snapshot,
            2,
            &[(1.0, 1.0), (0.9, 1.0)],
            &[(2.0, 1.0), (2.1, 1.0)],
        ));
        b.apply(&msg(DepthKind::Update, 3, &[(0.95, 5.0)], &[(1.9, 5.0)]));
        let v = b.view().unwrap();
        assert_eq!(v.bids, vec![(1.0, 1.0), (0.95, 5.0)]);
        assert_eq!(v.asks, vec![(1.9, 5.0), (2.0, 1.0)]);
    }

    /// Replay consecutive live BTC depth messages: the version sequence is
    /// unbroken, every message applies, and the book never crosses.
    #[test]
    fn replays_captured_live_sequence_cleanly() {
        let fx = include_str!("fixtures/depth_btc.txt");
        let mut b = OrderBook::new("B-BTC_USDT", 20);
        let mut applied = 0;
        let mut before_first_snapshot = true;
        for line in fx.lines() {
            let Some(MarketMsg::Depth(d)) = parse_line(line) else {
                panic!()
            };
            let out = b.apply(&d);
            if d.kind == DepthKind::Snapshot {
                before_first_snapshot = false;
            }
            if before_first_snapshot {
                assert_eq!(out, ApplyOutcome::IgnoredAwaitingSnapshot);
                continue;
            }
            assert!(
                matches!(
                    out,
                    ApplyOutcome::SnapshotApplied | ApplyOutcome::UpdateApplied
                ),
                "vs {} -> {out:?}",
                d.version
            );
            applied += 1;
            let v = b.view().unwrap();
            assert!(v.best_bid().unwrap().0 < v.best_ask().unwrap().0);
            assert!(v.bids.len() <= 20 && v.asks.len() <= 20);
        }
        assert!(applied >= 25);
        assert_eq!(b.stats.gaps, 0);
        assert_eq!(b.stats.crossed, 0);
    }
}
