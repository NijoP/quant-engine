//! Champion/challenger evaluation on recorded public data (`qe-research challenge`).
//!
//! Builds order-flow features at each decision time t from data with ts <= t only,
//! labels the 120 s bracket on the path in (t, t + 120 s], and scores every model with
//! the same purged chronological walk-forward as [`crate::calibration`]: K time-span
//! folds, fold 0 trains only, fold k is predicted by models fitted on folds < k whose
//! label windows end before fold k starts.
//!
//! Models (see docs/FORECASTING_MODEL.md for formulas):
//! * `desk.fp.v1`        champion: momentum + OBI10 drift, first-passage lattice (raw).
//! * `desk.fp.v1+iso`    champion probabilities, isotonic recalibration (PAV) per fold.
//! * `desk.fp.v1+platt`  champion probabilities, Platt scaling per fold.
//! * `logit.flow.v2`     ridge logistic on order-flow features, one model per target.
//! * `cond.flow.v2`      conditional-historical frequencies in OBI5 x TFI30 x vol-regime cells.
//!
//! Parameter search: k_s, k_t and the EV threshold are chosen by nested walk-forward
//! (the config for fold k is the one with the best realised net over folds 1..k-1),
//! so the reported nested result never uses the fold it is scored on.
use crate::eval::{self, label_path, Outcome, DESK_PARAMS, HORIZON_S, SLIPPAGE_PER_SIDE};
use crate::registry::{FeatureDef, ModelCard, ModelRegistry, TargetScore, SCHEMA};
use anyhow::Result;
use qe_math::learn::{logit, Isotonic, Logistic};
use qe_math::{micro, prob};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::path::Path;

pub const FEATURES: [(&str, &str); 11] = [
    ("mom60_z", "ln(P_t / P_{t-60s}) / (sigma_1s * sqrt(60))"),
    ("mom10_z", "ln(P_t / P_{t-10s}) / (sigma_1s * sqrt(10))"),
    ("obi1", "(qb1 - qa1) / (qb1 + qa1) at the best level of the freshest valid book (<= 5 s old)"),
    ("obi5", "depth imbalance over 5 levels"),
    ("obi20", "depth imbalance over 20 levels"),
    ("micro_dev_z", "(microprice - mid) / mid / sigma_1s"),
    ("ofi_z", "Cont best-level order-flow imbalance between the two latest valid books (gap <= 5 s), / mean best-level depth, clamped to [-10, 10]"),
    ("tfi30", "aggressor trade-flow imbalance (buy - sell qty) / (buy + sell qty) over (t-30 s, t]; 0 without trades"),
    ("intensity", "ln(1 + trades in (t-30 s, t]) - ln(1 + trades in (t-300 s, t] / 10)"),
    ("vol_regime", "ln(sigma over last 60 RT samples / sigma over last 600 RT samples)"),
    ("spread_bps", "(ask1 - bid1) / mid * 1e4 of the freshest valid book"),
];
const NF: usize = FEATURES.len();
pub const K_S: [f64; 3] = [0.5, 1.0, 1.5];
pub const K_T: [f64; 4] = [0.5, 1.0, 1.5, 2.0];
pub const THRESH_BPS: [f64; 4] = [0.0, 1.0, 3.0, 5.0];
pub const UP_H: [i64; 4] = [10, 30, 60, 120];
pub const SCENARIOS: [&str; 3] = ["taker_taker", "maker_taker", "maker_maker"];
const BASE_CFG: (f64, f64) = (DESK_PARAMS.k_s, DESK_PARAMS.k_t);
const L2: f64 = 1.0;
const COND_PRIOR: f64 = 30.0;

#[derive(Debug, Clone)]
struct Book {
    ts: i64,
    bids: Vec<(f64, f64)>,
    asks: Vec<(f64, f64)>,
}

/// One decision point.
#[derive(Debug, Clone)]
pub struct Row {
    pub t: i64,
    pub pair: String,
    pub entry: f64,
    pub sigma: f64,
    pub x: [f64; NF],
    pub obi10: f64,
    pub r60: f64,
    /// prices in (t, t + 120 s]
    pub path: Vec<f64>,
    /// (h, up?) for h in UP_H; None = tie
    pub up: Vec<(i64, Option<bool>)>,
    /// round-trip cost per scenario (fraction)
    pub cost: [f64; 3],
    pub fold: usize,
}

fn lines(dir: &Path, f: &str) -> Result<Vec<Value>> {
    let p = dir.join(f);
    if !p.exists() {
        return Ok(vec![]);
    }
    let r = std::io::BufReader::new(std::fs::File::open(p)?);
    Ok(r.lines()
        .map_while(|l| l.ok())
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect())
}

pub struct Data {
    pub rows: Vec<Row>,
    pub meta: Value,
}

/// Load the recording and build decision rows on every pair with books.
pub fn build_rows(dir: &Path, stride_ms: i64, folds: usize) -> Result<Data> {
    let rec = eval::load_recording(dir)?;
    let mut books: HashMap<String, Vec<Book>> = HashMap::new();
    for v in lines(dir, "books.jsonl")? {
        let (Some(pair), Some(ts)) = (v["pair"].as_str(), v["ts"].as_i64()) else {
            continue;
        };
        let side = |k: &str| -> Vec<(f64, f64)> {
            v[k].as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|l| Some((l[0].as_f64()?, l[1].as_f64()?)))
                        .filter(|l| l.0 > 0.0 && l.1 > 0.0)
                        .collect()
                })
                .unwrap_or_default()
        };
        let (bids, asks) = (side("bids"), side("asks"));
        match (bids.first(), asks.first()) {
            (Some(b), Some(a)) if b.0 < a.0 => {}
            _ => continue, // empty or crossed snapshot: invalid, not used
        }
        books
            .entry(pair.into())
            .or_default()
            .push(Book { ts, bids, asks });
    }
    for b in books.values_mut() {
        b.sort_by_key(|x| x.ts);
        b.dedup_by_key(|x| x.ts);
    }
    // tape: union of polled trades, deduplicated on (ts, price, qty, side)
    let mut tape: HashMap<String, Vec<(i64, f64, f64, f64)>> = HashMap::new();
    let (mut tick_agree, mut tick_n) = (0usize, 0usize);
    for v in lines(dir, "trades.jsonl")? {
        let Some(pair) = v["pair"].as_str() else {
            continue;
        };
        let e = tape.entry(pair.into()).or_default();
        for t in v["trades"].as_array().into_iter().flatten() {
            let (Some(ts), Some(px), Some(q)) = (
                t["timestamp"].as_f64(),
                t["price"].as_f64(),
                t["quantity"].as_f64(),
            ) else {
                continue;
            };
            // assumption: is_maker = buyer was maker => seller aggressed
            let sign = if t["is_maker"].as_bool().unwrap_or(false) {
                -1.0
            } else {
                1.0
            };
            e.push((ts as i64, px, q, sign));
        }
    }
    for t in tape.values_mut() {
        t.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        t.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2 && a.3 == b.3);
        for w in t.windows(2) {
            if w[1].1 != w[0].1 {
                tick_n += 1;
                if (w[1].1 > w[0].1) == (w[1].3 > 0.0) {
                    tick_agree += 1;
                }
            }
        }
    }
    let mut maker: HashMap<String, f64> = HashMap::new();
    for v in lines(dir, "instruments.jsonl")? {
        let i = &v["instrument"];
        if let (Some(p), Some(f)) = (i["pair"].as_str(), i["maker_fee"].as_f64()) {
            maker.insert(p.into(), f / 100.0);
        }
    }

    let mut rows = vec![];
    let mut pairs: Vec<&String> = books.keys().collect();
    pairs.sort();
    for pair in pairs {
        let Some(s) = rec.rt.get(pair) else { continue };
        let bk = &books[pair];
        let tp = tape.get(pair).map(|v| v.as_slice()).unwrap_or(&[]);
        let taker = rec
            .fees
            .get(pair)
            .copied()
            .unwrap_or(eval::DEFAULT_TAKER_FEE);
        let mk = maker.get(pair).copied().unwrap_or(taker);
        let mut next_t = i64::MIN;
        for i in 0..s.len() {
            let (t, entry) = s[i];
            if t < next_t {
                continue;
            }
            let Some(sig) = eval::sigma_per_s(&s[i.saturating_sub(599)..=i]) else {
                continue;
            };
            let sig_short = eval::sigma_per_s(&s[i.saturating_sub(59)..=i]).unwrap_or(sig);
            let k = bk.partition_point(|b| b.ts <= t);
            if k == 0 || t - bk[k - 1].ts > 5_000 {
                continue;
            }
            let b = &bk[k - 1];
            let end = s.partition_point(|x| x.0 <= t + 120_000);
            if end <= i + 1 || s[end - 1].0 < t + 115_000 {
                continue;
            }
            let ret = |lag: i64| {
                let j = s[..=i].partition_point(|x| x.0 <= t - lag);
                if j > 0 {
                    (entry / s[j - 1].1).ln()
                } else {
                    0.0
                }
            };
            let (r60, r10) = (ret(60_000), ret(10_000));
            let (bb, ba) = (b.bids[0], b.asks[0]);
            let mid = micro::mid(bb.0, ba.0);
            let mp = micro::microprice(bb.0, bb.1, ba.0, ba.1);
            let ofi = if k >= 2 && b.ts - bk[k - 2].ts <= 5_000 {
                let p = &bk[k - 2];
                let (pb, pa) = (p.bids[0], p.asks[0]);
                let e = if bb.0 >= pb.0 { bb.1 } else { 0.0 }
                    - if bb.0 <= pb.0 { pb.1 } else { 0.0 }
                    - if ba.0 <= pa.0 { ba.1 } else { 0.0 }
                    + if ba.0 >= pa.0 { pa.1 } else { 0.0 };
                (e / ((bb.1 + ba.1) / 2.0)).clamp(-10.0, 10.0)
            } else {
                0.0
            };
            let lo30 = tp.partition_point(|x| x.0 <= t - 30_000);
            let lo300 = tp.partition_point(|x| x.0 <= t - 300_000);
            let hi = tp.partition_point(|x| x.0 <= t);
            let (mut buy, mut sell) = (0.0, 0.0);
            for x in &tp[lo30..hi] {
                if x.3 > 0.0 {
                    buy += x.2;
                } else {
                    sell += x.2;
                }
            }
            let tfi = if buy + sell > 0.0 {
                (buy - sell) / (buy + sell)
            } else {
                0.0
            };
            let n30 = (hi - lo30) as f64;
            let n300 = (hi - lo300) as f64;
            let spread = (ba.0 - bb.0) / mid;
            let x = [
                r60 / (sig * 60f64.sqrt()),
                r10 / (sig * 10f64.sqrt()),
                micro::obi(&b.bids, &b.asks, 1).unwrap_or(0.0),
                micro::obi(&b.bids, &b.asks, 5).unwrap_or(0.0),
                micro::obi(&b.bids, &b.asks, 20).unwrap_or(0.0),
                (mp - mid) / mid / sig,
                ofi,
                tfi,
                (1.0 + n30).ln() - (1.0 + n300 / 10.0).ln(),
                (sig_short / sig).ln(),
                spread * 1e4,
            ];
            if x.iter().any(|v| !v.is_finite()) {
                continue;
            }
            let up = UP_H
                .iter()
                .filter_map(|&h| {
                    let j = s.partition_point(|x| x.0 < t + h * 1000);
                    let p = s.get(j).filter(|x| x.0 <= t + h * 1000 + 1_500)?.1;
                    Some((h, (p != entry).then_some(p > entry)))
                })
                .collect();
            rows.push(Row {
                t,
                pair: pair.clone(),
                entry,
                sigma: sig,
                x,
                obi10: micro::obi(&b.bids, &b.asks, 10).unwrap_or(0.0),
                r60,
                path: s[i + 1..end].iter().map(|x| x.1).collect(),
                up,
                cost: [
                    2.0 * taker + spread + 2.0 * SLIPPAGE_PER_SIDE,
                    mk + taker + spread / 2.0 + SLIPPAGE_PER_SIDE,
                    2.0 * mk,
                ],
                fold: 0,
            });
            next_t = t + stride_ms;
        }
    }
    rows.sort_by_key(|r| r.t);
    let (t0, t1) = (
        rows.first().map_or(0, |r| r.t),
        rows.last().map_or(0, |r| r.t) + 1,
    );
    let span = (t1 - t0) as f64 / folds as f64;
    for r in rows.iter_mut() {
        r.fold = (((r.t - t0) as f64 / span) as usize).min(folds - 1);
    }
    let fold_bounds: Vec<i64> = (0..=folds).map(|k| t0 + (span * k as f64) as i64).collect();
    let meta = json!({
        "recording_minutes": (rec.t_last - rec.t_first) as f64 / 60_000.0,
        "rt_polls": rec.rt_polls, "t_first_ms": rec.t_first, "t_last_ms": rec.t_last,
        "book_pairs": books.len(), "book_snapshots": books.values().map(|b| b.len()).sum::<usize>(),
        "tape_trades": tape.values().map(|t| t.len()).sum::<usize>(),
        "aggressor_sign_tick_rule_agreement": if tick_n > 0 { tick_agree as f64 / tick_n as f64 } else { f64::NAN },
        "n_rows": rows.len(), "decision_stride_s": stride_ms as f64 / 1000.0, "folds": folds,
        "fold_bounds_ms": fold_bounds,
        "approx_independent_120s_windows": (rows.len() as f64 * stride_ms as f64 / 1000.0 / HORIZON_S).round(),
        "taker_fee": rec.fees, "maker_fee": maker,
    });
    Ok(Data { rows, meta })
}

// ----------------------------------------------------------------- labels

/// Bracket labels for (k_s, k_t): [long, short] (outcome, gross log return).
fn bracket(r: &Row, ks: f64, kt: f64) -> [(Outcome, f64); 2] {
    let s120 = r.sigma * HORIZON_S.sqrt();
    label_path(r.entry, kt * s120, ks * s120, &r.path).expect("path non-empty")
}

/// Target index: 0 tp_long, 1 sl_long, 2 tp_short, 3 sl_short.
const BRACKET_TARGETS: [&str; 4] = [
    "tp_first_long",
    "sl_first_long",
    "tp_first_short",
    "sl_first_short",
];

fn bracket_y(o: &[(Outcome, f64); 2], k: usize) -> bool {
    let want = if k.is_multiple_of(2) {
        Outcome::Target
    } else {
        Outcome::Stop
    };
    o[k / 2].0 == want
}

// ----------------------------------------------------------------- models

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    DeskRaw,
    DeskIso,
    DeskPlatt,
    Logit,
    Cond,
}

impl Kind {
    pub fn id(self) -> &'static str {
        match self {
            Kind::DeskRaw => "desk.fp.v1",
            Kind::DeskIso => "desk.fp.v1+iso",
            Kind::DeskPlatt => "desk.fp.v1+platt",
            Kind::Logit => "logit.flow.v2",
            Kind::Cond => "cond.flow.v2",
        }
    }
    fn version(self) -> &'static str {
        match self {
            Kind::DeskRaw => "v1",
            Kind::DeskIso => "v1+iso.1",
            Kind::DeskPlatt => "v1+platt.1",
            Kind::Logit | Kind::Cond => "v2",
        }
    }
}

const KINDS: [Kind; 5] = [
    Kind::DeskRaw,
    Kind::DeskIso,
    Kind::DeskPlatt,
    Kind::Logit,
    Kind::Cond,
];

/// Raw champion probability for a target on a row at (ks, kt).
fn desk_p(r: &Row, ks: f64, kt: f64, target: &str) -> f64 {
    let p = eval::Params {
        k_s: ks,
        k_t: kt,
        ..DESK_PARAMS
    };
    let Some(f) = eval::forecast(&p, r.sigma, r.r60, r.obi10) else {
        return f64::NAN;
    };
    match target {
        "tp_first_long" => f.long.p_target,
        "sl_first_long" => f.long.p_stop,
        "tp_first_short" => f.short.p_target,
        "sl_first_short" => f.short.p_stop,
        t => {
            let h: f64 = t
                .trim_start_matches("up_")
                .trim_end_matches('s')
                .parse()
                .unwrap_or(60.0);
            eval::p_up(&f, h)
        }
    }
}

fn cell(x: &[f64; NF]) -> usize {
    let b = |v: f64| {
        if v < -0.2 {
            0
        } else if v > 0.2 {
            2
        } else {
            1
        }
    };
    b(x[3]) * 6 + b(x[7]) * 2 + usize::from(x[9] >= 0.0)
}

/// Fitted predictor for one target.
enum Fit {
    Raw,
    Iso(Isotonic),
    Platt(Logistic),
    Logit(Logistic),
    Cond(Vec<f64>),
    Clim(f64),
}

fn fit(kind: Kind, xs: &[[f64; NF]], praw: &[f64], y: &[bool]) -> Fit {
    let base = y.iter().filter(|v| **v).count() as f64 / y.len().max(1) as f64;
    let clim = Fit::Clim(base);
    match kind {
        Kind::DeskRaw => Fit::Raw,
        Kind::DeskIso => Isotonic::fit(praw, y, 10.0).map_or(clim, Fit::Iso),
        Kind::DeskPlatt => {
            let x: Vec<Vec<f64>> = praw.iter().map(|p| vec![logit(*p)]).collect();
            Logistic::fit(&x, y, 0.0).map_or(clim, Fit::Platt)
        }
        Kind::Logit => {
            let x: Vec<Vec<f64>> = xs.iter().map(|r| r.to_vec()).collect();
            Logistic::fit(&x, y, L2).map_or(clim, Fit::Logit)
        }
        Kind::Cond => {
            let (mut s, mut n) = (vec![0.0; 18], vec![0.0; 18]);
            for (r, &yy) in xs.iter().zip(y) {
                let c = cell(r);
                n[c] += 1.0;
                s[c] += f64::from(u8::from(yy));
            }
            Fit::Cond(
                (0..18)
                    .map(|c| (s[c] + COND_PRIOR * base) / (n[c] + COND_PRIOR))
                    .collect(),
            )
        }
    }
}

fn predict(f: &Fit, x: &[f64; NF], praw: f64) -> f64 {
    match f {
        Fit::Raw => praw,
        Fit::Iso(m) => m.apply(praw),
        Fit::Platt(m) => m.predict(&[logit(praw)]),
        Fit::Logit(m) => m.predict(x),
        Fit::Cond(v) => v[cell(x)],
        Fit::Clim(b) => *b,
    }
}

/// OOS predictions for one target: (row index, p, climatology p, y). Rows without
/// a label (`y` None) are skipped; a fold with < 50 purged training rows abstains.
fn oos(
    rows: &[Row],
    folds: usize,
    kind: Kind,
    praw: &[f64],
    y: &[Option<bool>],
    horizon_ms: i64,
    bounds: &[i64],
) -> Vec<(usize, f64, f64, bool)> {
    let mut out = vec![];
    for (k, &start) in bounds.iter().enumerate().take(folds).skip(1) {
        let tr: Vec<usize> = (0..rows.len())
            .filter(|&i| {
                rows[i].fold < k
                    && rows[i].t + horizon_ms < start
                    && y[i].is_some()
                    && praw[i].is_finite()
            })
            .collect();
        if tr.len() < 50 {
            continue;
        }
        let xs: Vec<[f64; NF]> = tr.iter().map(|&i| rows[i].x).collect();
        let pr: Vec<f64> = tr.iter().map(|&i| praw[i]).collect();
        let yy: Vec<bool> = tr.iter().map(|&i| y[i].unwrap()).collect();
        let base = yy.iter().filter(|v| **v).count() as f64 / yy.len() as f64;
        let f = fit(kind, &xs, &pr, &yy);
        for i in (0..rows.len()).filter(|&i| rows[i].fold == k && praw[i].is_finite()) {
            if let Some(yi) = y[i] {
                out.push((i, predict(&f, &rows[i].x, praw[i]), base, yi));
            }
        }
    }
    out
}

type Oos = [(usize, f64, f64, bool)];

fn score(rows: &[Row], o: &Oos) -> TargetScore {
    if o.is_empty() {
        return TargetScore::default();
    }
    let p: Vec<f64> = o.iter().map(|x| x.1).collect();
    let c: Vec<f64> = o.iter().map(|x| x.2).collect();
    let y: Vec<bool> = o.iter().map(|x| x.3).collect();
    let brier = prob::brier(&p, &y).unwrap_or(f64::NAN);
    let bc = prob::brier(&c, &y).unwrap_or(f64::NAN);
    type FoldVecs = (Vec<f64>, Vec<f64>, Vec<bool>);
    let mut by_fold: BTreeMap<usize, FoldVecs> = BTreeMap::new();
    for x in o {
        let e = by_fold.entry(rows[x.0].fold).or_default();
        e.0.push(x.1);
        e.1.push(x.2);
        e.2.push(x.3);
    }
    let pos = by_fold
        .values()
        .filter(|(p, c, y)| prob::brier(p, y) < prob::brier(c, y))
        .count();
    TargetScore {
        n_oos: o.len(),
        oos_folds: by_fold.len(),
        base_rate: y.iter().filter(|v| **v).count() as f64 / y.len() as f64,
        mean_p: p.iter().sum::<f64>() / p.len() as f64,
        brier,
        brier_clim: bc,
        skill: 1.0 - brier / bc,
        ece: prob::ece(&p, &y, 10).unwrap_or(f64::NAN),
        log_loss: prob::log_loss(&p, &y).unwrap_or(f64::NAN),
        folds_positive_skill: pos,
    }
}

// ----------------------------------------------------------------- trading

/// OOS bracket probabilities [tp_l, sl_l, tp_s, sl_s] per row for (kind, cfg).
struct CfgPred {
    ks: f64,
    kt: f64,
    p: HashMap<usize, [f64; 4]>,
    labels: Vec<[(Outcome, f64); 2]>,
}

fn cfg_pred(rows: &[Row], folds: usize, kind: Kind, ks: f64, kt: f64, bounds: &[i64]) -> CfgPred {
    let labels: Vec<_> = rows.iter().map(|r| bracket(r, ks, kt)).collect();
    let mut p: HashMap<usize, [f64; 4]> = HashMap::new();
    for (k, tg) in BRACKET_TARGETS.iter().enumerate() {
        let praw: Vec<f64> = if matches!(kind, Kind::Logit | Kind::Cond) {
            vec![0.5; rows.len()]
        } else {
            rows.iter().map(|r| desk_p(r, ks, kt, tg)).collect()
        };
        let y: Vec<Option<bool>> = labels.iter().map(|o| Some(bracket_y(o, k))).collect();
        for (i, pi, _, _) in oos(rows, folds, kind, &praw, &y, 120_000, bounds) {
            p.entry(i).or_insert([f64::NAN; 4])[k] = pi;
        }
    }
    CfgPred { ks, kt, p, labels }
}

/// Mean realised stop loss (positive fraction) and mean neither-return per side,
/// from training rows only (fold < k, label window ends before `start`).
fn train_moments(rows: &[Row], c: &CfgPred, k: usize, start: i64) -> [(f64, f64); 2] {
    let mut out = [(f64::NAN, 0.0); 2];
    for (side, o) in out.iter_mut().enumerate() {
        let (mut sl, mut nsl, mut ne, mut nne) = (0.0, 0.0, 0.0, 0.0);
        for (i, r) in rows.iter().enumerate() {
            if r.fold >= k || r.t + 120_000 >= start {
                continue;
            }
            match c.labels[i][side] {
                (Outcome::Stop, x) => {
                    sl -= x;
                    nsl += 1.0;
                }
                (Outcome::Neither, x) => {
                    ne += x;
                    nne += 1.0;
                }
                _ => {}
            }
        }
        if nsl > 0.0 {
            o.0 = sl / nsl;
        }
        if nne > 0.0 {
            o.1 = ne / nne;
        }
    }
    out
}

/// Trades per OOS fold for a config at threshold `thr_bps` under scenario `sc`:
/// fold -> realised net bps. At most one open bracket per pair (no overlap).
fn trade(
    rows: &[Row],
    c: &CfgPred,
    folds: usize,
    sc: usize,
    thr_bps: f64,
    bounds: &[i64],
) -> BTreeMap<usize, Vec<f64>> {
    let mut out: BTreeMap<usize, Vec<f64>> = BTreeMap::new();
    for (k, &start) in bounds.iter().enumerate().take(folds).skip(1) {
        let m = train_moments(rows, c, k, start);
        if m.iter().any(|x| !x.0.is_finite()) {
            continue;
        }
        let mut busy: HashMap<&str, i64> = HashMap::new();
        let v = out.entry(k).or_default();
        for (i, r) in rows.iter().enumerate().filter(|(_, r)| r.fold == k) {
            let Some(p) = c.p.get(&i) else { continue };
            if busy.get(r.pair.as_str()).is_some_and(|&u| r.t < u) {
                continue;
            }
            let tp = c.kt * r.sigma * HORIZON_S.sqrt();
            let cost = r.cost[sc];
            let ev = |side: usize| {
                let (pt, ps) = (p[side * 2], p[side * 2 + 1]);
                let pn = (1.0 - pt - ps).max(0.0);
                pt * tp - ps * m[side].0 + pn * m[side].1 - cost
            };
            let (el, es) = (ev(0), ev(1));
            let (e, side) = if el >= es { (el, 0) } else { (es, 1) };
            if e.is_finite() && e * 1e4 > thr_bps {
                v.push((c.labels[i][side].1 - cost) * 1e4);
                busy.insert(&r.pair, r.t + 120_000);
            }
        }
    }
    out
}

fn summarize(by_fold: &BTreeMap<usize, Vec<f64>>) -> Value {
    let all: Vec<f64> = by_fold.values().flatten().copied().collect();
    let n = all.len();
    let mean = if n > 0 {
        all.iter().sum::<f64>() / n as f64
    } else {
        f64::NAN
    };
    let sd = if n > 1 {
        (all.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt()
    } else {
        f64::NAN
    };
    let folds: Vec<Value> = by_fold
        .iter()
        .map(|(k, v)| {
            let m = if v.is_empty() {
                f64::NAN
            } else {
                v.iter().sum::<f64>() / v.len() as f64
            };
            json!({"fold": k, "n": v.len(), "mean_net_bps": m})
        })
        .collect();
    let pos = by_fold
        .values()
        .filter(|v| !v.is_empty() && v.iter().sum::<f64>() > 0.0)
        .count();
    let nonempty = by_fold.values().filter(|v| !v.is_empty()).count();
    let win = if n > 0 {
        all.iter().filter(|x| **x > 0.0).count() as f64 / n as f64
    } else {
        f64::NAN
    };
    json!({"n_trades": n, "mean_net_bps": mean, "sd_bps": sd,
        "t_stat": mean / (sd / (n as f64).sqrt()), "win_rate": win,
        "sum_net_bps": all.iter().sum::<f64>(), "folds_positive": pos,
        "folds_with_trades": nonempty, "by_fold": folds})
}

/// Promotion gate on a trading summary (taker_taker, the only executable scenario).
fn trading_failures(s: &Value) -> Vec<String> {
    let mut f = vec![];
    let n = s["n_trades"].as_u64().unwrap_or(0);
    let mean = s["mean_net_bps"].as_f64().unwrap_or(f64::NAN);
    let t = s["t_stat"].as_f64().unwrap_or(f64::NAN);
    let pos = s["folds_positive"].as_u64().unwrap_or(0);
    let nf = s["folds_with_trades"].as_u64().unwrap_or(0);
    if n < 100 {
        f.push(format!("taker_taker: n_trades {n} < 100"));
    }
    if mean.is_nan() || mean <= 0.0 {
        f.push(format!("taker_taker: mean net {mean:.2} bps <= 0"));
    }
    if t.is_nan() || t < 2.0 {
        f.push(format!("taker_taker: t-stat {t:.2} < 2"));
    }
    if nf < 3 || (pos as f64) < (2.0 / 3.0) * nf as f64 {
        f.push(format!(
            "taker_taker: positive in {pos}/{nf} folds (need >= 2/3 of >= 3)"
        ));
    }
    f
}

fn calib_failures(cal: &BTreeMap<String, TargetScore>) -> Vec<String> {
    let mut f = vec![];
    for t in BRACKET_TARGETS {
        let Some(s) = cal.get(t) else {
            f.push(format!("{t}: not scored"));
            continue;
        };
        if s.n_oos < 1000 || s.oos_folds < 3 {
            f.push(format!(
                "{t}: n_oos {} over {} folds (need >= 1000 over >= 3)",
                s.n_oos, s.oos_folds
            ));
        }
        if s.ece.is_nan() || s.ece > 0.03 {
            f.push(format!("{t}: ECE {:.3} > 0.03", s.ece));
        }
        if s.skill.is_nan() || s.skill < 0.01 {
            f.push(format!("{t}: Brier skill {:.4} < 0.01", s.skill));
        }
        if (s.folds_positive_skill as f64) < (2.0 / 3.0) * s.oos_folds as f64 {
            f.push(format!(
                "{t}: skill > 0 in {}/{} folds",
                s.folds_positive_skill, s.oos_folds
            ));
        }
    }
    f
}

pub const PROMOTION_CRITERIA: [&str; 6] = [
    "calibration, each of tp_first_{long,short} and sl_first_{long,short} at the champion bracket (k_s=1.0, k_t=1.5): n_oos >= 1000 over >= 3 OOS folds",
    "calibration: ECE (10 bins) <= 0.03 and Brier skill vs training-fold climatology >= 0.01, skill > 0 in >= 2/3 of OOS folds",
    "trading under the taker_taker cost (2 x taker fee + spread + 2 x 1 bp), one bracket per pair at a time: n_trades >= 100",
    "trading: mean realised net > 0 bps with t-stat >= 2, and positive net in >= 2/3 of >= 3 OOS folds",
    "challengers with a parameter search are judged on the nested walk-forward result (config for fold k chosen on folds < k), never on the best grid cell",
    "maker_taker and maker_maker results are reported but cannot promote: passive fills and adverse selection are not modelled",
];

fn card_features(kind: Kind) -> Vec<FeatureDef> {
    let fd = |n: &str, d: &str| FeatureDef {
        name: n.into(),
        definition: d.into(),
    };
    match kind {
        Kind::Logit => FEATURES.iter().map(|(n, d)| fd(n, d)).collect(),
        Kind::Cond => FEATURES
            .iter()
            .filter(|(n, _)| ["obi5", "tfi30", "vol_regime"].contains(n))
            .map(|(n, d)| fd(n, d))
            .collect(),
        _ => vec![
            fd(
                "sigma_1s",
                "RV of the last <= 600 RT log returns / sqrt(mean dt)",
            ),
            fd("r60", "ln(P_t / P_{t-60s})"),
            fd(
                "obi10",
                "depth imbalance over 10 levels of the freshest valid book",
            ),
        ],
    }
}

fn kind_text(kind: Kind) -> &'static str {
    match kind {
        Kind::DeskRaw => "first-passage (Brownian drift lattice), uncalibrated priors",
        Kind::DeskIso => "first-passage + isotonic (PAV) recalibration, prior weight 10",
        Kind::DeskPlatt => "first-passage + Platt scaling (logistic on logit p)",
        Kind::Logit => {
            "ridge logistic regression (IRLS, l2 = 1 on standardized features), one per target"
        }
        Kind::Cond => "conditional-historical frequency in 18 cells (OBI5 x TFI30 x vol regime), Beta shrinkage 30 toward base rate",
    }
}

/// Trading block for one model: per scenario the fixed champion-bracket result and,
/// for searched models, the nested walk-forward result. Returns (block, taker summary
/// the gate judges, grid rows).
fn trading_block(
    rows: &[Row],
    folds: usize,
    kind: Kind,
    bounds: &[i64],
) -> (Value, Value, Vec<Value>) {
    let searched = matches!(kind, Kind::Logit | Kind::Cond);
    let cfgs: Vec<(f64, f64)> = if searched {
        K_S.iter()
            .flat_map(|&s| K_T.iter().map(move |&t| (s, t)))
            .collect()
    } else {
        vec![BASE_CFG]
    };
    let preds: Vec<CfgPred> = cfgs
        .iter()
        .map(|&(s, t)| cfg_pred(rows, folds, kind, s, t, bounds))
        .collect();
    let mut block = serde_json::Map::new();
    let mut grid = vec![];
    let mut judged_taker = Value::Null;
    for (sc, name) in SCENARIOS.iter().enumerate() {
        let mut cells = vec![];
        for c in &preds {
            for thr in THRESH_BPS {
                let tr = trade(rows, c, folds, sc, thr, bounds);
                grid.push(
                    json!({"model": kind.id(), "scenario": name, "k_s": c.ks, "k_t": c.kt,
                    "threshold_bps": thr, "result": summarize(&tr)}),
                );
                cells.push(((c.ks, c.kt, thr), tr));
            }
        }
        let fixed = cells
            .iter()
            .find(|((s, t, th), _)| (*s, *t) == BASE_CFG && *th == 0.0)
            .map(|x| summarize(&x.1))
            .unwrap_or(Value::Null);
        let mut entry = json!({"champion_bracket_threshold_0": fixed});
        let judged = if searched {
            let mut nested: BTreeMap<usize, Vec<f64>> = BTreeMap::new();
            let mut chosen = vec![];
            for k in 2..folds {
                let best = cells
                    .iter()
                    .map(|(cfg, tr)| {
                        let past: Vec<f64> = (1..k)
                            .flat_map(|j| tr.get(&j).cloned().unwrap_or_default())
                            .collect();
                        (cfg, tr, past.len(), past.iter().sum::<f64>())
                    })
                    .filter(|x| x.2 >= 10)
                    .max_by(|a, b| a.3.total_cmp(&b.3));
                match best {
                    Some((cfg, tr, _, past)) if past > 0.0 => {
                        nested.insert(k, tr.get(&k).cloned().unwrap_or_default());
                        chosen.push(json!({"fold": k, "k_s": cfg.0, "k_t": cfg.1,
                            "threshold_bps": cfg.2, "past_sum_net_bps": past}));
                    }
                    other => {
                        nested.insert(k, vec![]);
                        let why = if other.is_some() {
                            "best past cell has net <= 0: abstain"
                        } else {
                            "no cell with >= 10 past trades: abstain"
                        };
                        chosen.push(json!({"fold": k, "abstain": true, "reason": why}));
                    }
                }
            }
            let s = summarize(&nested);
            entry["nested_walk_forward"] = json!({"result": s, "chosen": chosen});
            s
        } else {
            fixed
        };
        if sc == 0 {
            judged_taker = judged;
        }
        block.insert(name.to_string(), entry);
    }
    (Value::Object(block), judged_taker, grid)
}

/// Run the full evaluation and build the registry.
pub fn run(dir: &Path, stride_s: f64, folds: usize) -> Result<ModelRegistry> {
    let d = build_rows(dir, (stride_s * 1000.0) as i64, folds)?;
    let rows = &d.rows;
    anyhow::ensure!(rows.len() >= 200, "too few decision rows ({})", rows.len());
    let bounds: Vec<i64> = d.meta["fold_bounds_ms"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_i64())
        .collect();
    let base_labels: Vec<_> = rows
        .iter()
        .map(|r| bracket(r, BASE_CFG.0, BASE_CFG.1))
        .collect();
    let mut cards = vec![];
    let mut grid = vec![];
    for kind in KINDS {
        let mut cal = BTreeMap::new();
        for (k, tg) in BRACKET_TARGETS.iter().enumerate() {
            let praw: Vec<f64> = rows
                .iter()
                .map(|r| desk_p(r, BASE_CFG.0, BASE_CFG.1, tg))
                .collect();
            let y: Vec<Option<bool>> = base_labels.iter().map(|o| Some(bracket_y(o, k))).collect();
            let o = oos(rows, folds, kind, &praw, &y, 120_000, &bounds);
            cal.insert(tg.to_string(), score(rows, &o));
        }
        for h in UP_H {
            let tg = format!("up_{h}s");
            let praw: Vec<f64> = rows
                .iter()
                .map(|r| desk_p(r, BASE_CFG.0, BASE_CFG.1, &tg))
                .collect();
            let y: Vec<Option<bool>> = rows
                .iter()
                .map(|r| r.up.iter().find(|u| u.0 == h).and_then(|u| u.1))
                .collect();
            let o = oos(rows, folds, kind, &praw, &y, h * 1000, &bounds);
            cal.insert(tg, score(rows, &o));
        }
        let (trading, judged, g) = trading_block(rows, folds, kind, &bounds);
        grid.extend(g);
        let mut failures = calib_failures(&cal);
        failures.extend(trading_failures(&judged));
        let searched = matches!(kind, Kind::Logit | Kind::Cond);
        cards.push(ModelCard {
            id: kind.id().into(),
            version: kind.version().into(),
            role: if kind == Kind::DeskRaw { "champion" } else { "challenger" }.into(),
            kind: kind_text(kind).into(),
            params: json!({"desk_params": DESK_PARAMS, "l2": L2, "cond_prior": COND_PRIOR,
                "search_k_s": if searched { json!(K_S) } else { json!([BASE_CFG.0]) },
                "search_k_t": if searched { json!(K_T) } else { json!([BASE_CFG.1]) },
                "thresholds_bps": if searched { json!(THRESH_BPS) } else { json!([0.0]) }}),
            training_window: json!({"mode": "expanding chronological walk-forward",
                "purge": "label horizon (120 s for brackets, h for up_h)",
                "first_ms": d.meta["t_first_ms"], "fold_bounds_ms": d.meta["fold_bounds_ms"],
                "train_for_fold_k": "rows in folds < k whose label window ends before fold k starts"}),
            validation_window: json!({"oos_folds": (1..folds).collect::<Vec<_>>(),
                "nested_trading_folds": (2..folds).collect::<Vec<_>>()}),
            features: card_features(kind),
            calibration: cal,
            trading,
            assumptions: vec![
                "RT last price is the execution reference; take-profit fills exactly at TP, stops realise the observed price at the touching sample".into(),
                "aggressor side: is_maker=true means the buyer was maker (seller aggressed); agreement with the tick rule is reported in data".into(),
                "the tape is the union of polled public trades; trades between polls of busy pairs can be missing".into(),
                "decision rows overlap in their 120 s outcomes; trading t-stats use non-overlapping per-pair trades treated as independent".into(),
            ],
            limitations: vec![
                format!("one short recording ({:.1} min, {} book pairs): regime coverage is minimal",
                    d.meta["recording_minutes"].as_f64().unwrap_or(0.0), d.meta["book_pairs"]),
                "books are REST depth-20 snapshots sampled every few seconds per pair, not the full update stream".into(),
                "maker scenarios assume passive fills at the touch with no queue, non-fill or adverse-selection cost: upper bounds only".into(),
                "funding is not included (horizon 120 s)".into(),
            ],
            promotable: failures.is_empty(),
            promotion_failures: failures,
        });
    }
    let promotable: Vec<&str> = cards
        .iter()
        .filter(|c| c.promotable && c.role == "challenger")
        .map(|c| c.id.as_str())
        .collect();
    let champion_reason = if promotable.is_empty() {
        "no challenger passed the predefined out-of-sample gates; the champion stays (it has no strategy authority unless qe-research calibration grants it)".to_string()
    } else {
        format!("challengers {promotable:?} passed the OOS gates; the champion changes only after a paper-forward test")
    };
    Ok(ModelRegistry {
        schema: SCHEMA.into(),
        generated_ms: qe_coindcx::auth::request_timestamp_ms(),
        champion: Kind::DeskRaw.id().into(),
        champion_reason,
        rollback: Kind::DeskRaw.id().into(),
        promotion_criteria: PROMOTION_CRITERIA.iter().map(|s| s.to_string()).collect(),
        data: d.meta,
        models: cards,
        promotion_history: vec![],
        grid: Value::Array(grid),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(t: i64, fold: usize, x0: f64) -> Row {
        let mut x = [0.0; NF];
        x[0] = x0;
        Row {
            t,
            pair: "B-X_USDT".into(),
            entry: 100.0,
            sigma: 1e-4,
            x,
            obi10: 0.0,
            r60: 0.0,
            path: vec![100.0],
            up: vec![],
            cost: [0.0; 3],
            fold,
        }
    }

    #[test]
    fn oos_never_trains_on_the_fold_it_predicts() {
        // fold 1 reverses the fold-0 rule; a leak would make fold-1 predictions right
        let mut rows = vec![];
        let mut y = vec![];
        for i in 0..400 {
            let fold = usize::from(i >= 200);
            let x0 = (i % 2) as f64;
            rows.push(row(i as i64 * 1000, fold, x0));
            y.push(Some(if fold == 0 { x0 > 0.5 } else { x0 < 0.5 }));
        }
        let o = oos(
            &rows,
            2,
            Kind::Logit,
            &[0.5; 400],
            &y,
            120_000,
            &[0, 200_000, 400_000],
        );
        assert!(!o.is_empty());
        assert!(o.iter().all(|x| rows[x.0].fold == 1));
        let wrong = o.iter().filter(|x| (x.1 > 0.5) != x.3).count();
        assert!(wrong as f64 > 0.9 * o.len() as f64);
    }

    #[test]
    fn purge_drops_rows_whose_label_crosses_the_fold_start() {
        let rows: Vec<Row> = (0..300)
            .map(|i| row(i * 1000, usize::from(i >= 200), 0.0))
            .collect();
        let y: Vec<Option<bool>> = (0..300).map(|i| Some(i % 3 == 0)).collect();
        let b = [0, 200_000, 300_000];
        // horizon 140 s: rows with t + 140 s < 200 s may train, 60 rows
        assert!(!oos(&rows, 2, Kind::Cond, &[0.5; 300], &y, 140_000, &b).is_empty());
        // horizon 160 s: 40 rows < 50 -> the fold abstains
        assert!(oos(&rows, 2, Kind::Cond, &[0.5; 300], &y, 160_000, &b).is_empty());
    }

    #[test]
    fn trading_gate_rejects_negative_or_thin_results() {
        let mut m = BTreeMap::new();
        m.insert(1, [5.0, 6.0, 4.0, 5.5].repeat(10));
        m.insert(2, [-1.0, -0.5].repeat(20));
        m.insert(3, [2.0, 3.0].repeat(20));
        assert!(trading_failures(&summarize(&m)).is_empty());
        m.insert(2, vec![-20.0; 40]);
        let f = trading_failures(&summarize(&m));
        assert!(f.iter().any(|s| s.contains("mean net")));
        let mut thin = BTreeMap::new();
        thin.insert(1, vec![10.0; 5]);
        assert!(trading_failures(&summarize(&thin))
            .iter()
            .any(|s| s.contains("n_trades")));
    }
}
