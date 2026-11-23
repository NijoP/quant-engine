//! Walk-forward evaluator: replays recorded public data through the desk's
//! qe-math forecast and scores it with strict no-lookahead.
//!
//! # Model under test (`desk.fp.v1`, identical to `qe-desk::evaluate`)
//! * sigma_1s = RV(1-step log returns of the last <= 600 RT samples) / sqrt(mean dt)
//! * mu = LAMBDA * r_60 / 60 + GAMMA * OBI_10 * sigma_1s
//! * TP = K_T * sigma_1s * sqrt(120), SL = K_S * sigma_1s * sqrt(120)
//! * P(TP first), P(SL first) from `qe_math::prob::first_passage(mu, sigma, TP, SL, 120, 0.5)`
//!   (short side uses -mu); P(up at h) = Phi(mu h / (sigma sqrt h)).
//! * cost C = 2 * taker fee + spread + 2 * 1 bp slippage; EV = `bracket_ev(fp, TP, SL, C)`.
//!
//! `mom.fp.v1` is the same with OBI = 0 (no book needed), so it runs on all pairs.
//! `mom.fp.v1.1m` uses completed 1m candles: sigma_1s from the last 60 1m returns,
//! r_60 = last 1m return; barriers are checked on candle high/low.
//!
//! # No-lookahead
//! Features at decision time t use only samples with ts <= t (books: ts <= t and
//! t - ts <= 5 s). Labels use only samples in (t, t + 120 s]. Calibration maps are
//! fitted by [`crate::calibration::walk_forward`] on earlier folds with a 120 s purge.
//!
//! # Labels (conservative)
//! * Barrier touch on the sampled path; take-profit realises exactly TP (resting
//!   limit), stop realises the observed price at the touching sample (gap risk).
//! * 1m candles: if TP and SL are both inside one candle's range the order is
//!   unknown and the stop is assumed first.
//! * Neither: exit at the last sample <= t + 120 s.
//! * `up_h`: sign of the move at the first sample >= t + h (within 1.5 s);
//!   unchanged prices (ties) are excluded and counted.
use crate::calibration::{CalibrationEntry, Point};
use anyhow::Result;
use qe_math::dist::norm_cdf;
use qe_math::prob::{bracket_ev, first_passage, FirstPassage};
use qe_math::{micro, stats};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::BufRead;
use std::path::Path;

pub const HORIZON_S: f64 = 120.0;
pub const UP_HORIZONS_S: [i64; 9] = [1, 2, 5, 10, 15, 30, 60, 90, 120];
pub const DEFAULT_TAKER_FEE: f64 = 0.00059;
pub const SLIPPAGE_PER_SIDE: f64 = 0.0001;
pub const COST_MULTS: [f64; 6] = [0.0, 0.25, 0.5, 1.0, 1.5, 2.0];

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Params {
    pub lambda: f64,
    pub gamma: f64,
    pub k_s: f64,
    pub k_t: f64,
    pub dt: f64,
}

/// The desk's current (uncalibrated) priors, copied from `qe-desk/src/main.rs`.
pub const DESK_PARAMS: Params = Params {
    lambda: 0.2,
    gamma: 0.05,
    k_s: 1.0,
    k_t: 1.5,
    dt: 0.5,
};

#[derive(Debug, Clone, Copy)]
pub struct Forecast {
    pub mu: f64,
    pub sigma: f64,
    pub tp: f64,
    pub sl: f64,
    pub long: FirstPassage,
    pub short: FirstPassage,
}

pub fn forecast(p: &Params, sigma: f64, r60: f64, obi: f64) -> Option<Forecast> {
    if !(sigma > 0.0 && sigma.is_finite() && r60.is_finite()) {
        return None;
    }
    let mu = p.lambda * r60 / 60.0 + p.gamma * obi * sigma;
    let s120 = sigma * HORIZON_S.sqrt();
    let (tp, sl) = (p.k_t * s120, p.k_s * s120);
    Some(Forecast {
        mu,
        sigma,
        tp,
        sl,
        long: first_passage(mu, sigma, tp, sl, HORIZON_S, p.dt),
        short: first_passage(-mu, sigma, tp, sl, HORIZON_S, p.dt),
    })
}

pub fn p_up(f: &Forecast, h_s: f64) -> f64 {
    norm_cdf(f.mu * h_s / (f.sigma * h_s.sqrt()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Outcome {
    Target,
    Stop,
    Neither,
}

/// Realised outcome and gross log return for (long, short) along a sampled path.
pub fn label_path(entry: f64, tp: f64, sl: f64, path: &[f64]) -> Option<[(Outcome, f64); 2]> {
    let last = *path.last()?;
    let xs: Vec<f64> = path.iter().map(|p| (p / entry).ln()).collect();
    let long = xs
        .iter()
        .find_map(|&x| {
            if x >= tp {
                Some((Outcome::Target, tp))
            } else if x <= -sl {
                Some((Outcome::Stop, x))
            } else {
                None
            }
        })
        .unwrap_or((Outcome::Neither, (last / entry).ln()));
    let short = xs
        .iter()
        .find_map(|&x| {
            if x <= -tp {
                Some((Outcome::Target, tp))
            } else if x >= sl {
                Some((Outcome::Stop, -x))
            } else {
                None
            }
        })
        .unwrap_or((Outcome::Neither, -(last / entry).ln()));
    Some([long, short])
}

/// Candle-path labels. Each candle is (open, high, low, close). Same-candle
/// TP and SL ⇒ stop first (conservative). Gap opens beyond the stop realise the open.
pub fn label_candles(
    entry: f64,
    tp: f64,
    sl: f64,
    c: &[(f64, f64, f64, f64)],
) -> Option<[(Outcome, f64); 2]> {
    let last = c.last()?.3;
    let l = |x: f64| (x / entry).ln();
    let mut long = None;
    let mut short = None;
    for &(o, h, lo, _) in c {
        if long.is_none() {
            if l(lo) <= -sl {
                long = Some((Outcome::Stop, l(o).min(-sl)));
            } else if l(h) >= tp {
                long = Some((Outcome::Target, tp));
            }
        }
        if short.is_none() {
            if l(h) >= sl {
                short = Some((Outcome::Stop, (-l(o)).min(-sl)));
            } else if l(lo) <= -tp {
                short = Some((Outcome::Target, tp));
            }
        }
    }
    Some([
        long.unwrap_or((Outcome::Neither, l(last))),
        short.unwrap_or((Outcome::Neither, -l(last))),
    ])
}

/// One evaluated decision.
#[derive(Debug, Clone)]
pub struct Sample {
    pub t_ms: i64,
    pub pair: String,
    pub f: Forecast,
    /// full round-trip cost fraction, when spread and fee are known
    pub cost: Option<f64>,
    pub out: [(Outcome, f64); 2],
    /// (horizon s, p_up, Some(up) | None for tie)
    pub up: Vec<(i64, f64, Option<bool>)>,
}

// ---------------------------------------------------------------- loading

pub struct Recording {
    pub rt: HashMap<String, Vec<(i64, f64)>>,
    /// pair -> (ts, obi10, spread fraction), ts-sorted
    pub books: HashMap<String, Vec<(i64, f64, f64)>>,
    pub fees: HashMap<String, f64>,
    pub rt_polls: usize,
    pub t_first: i64,
    pub t_last: i64,
}

pub fn load_recording(dir: &Path) -> Result<Recording> {
    let mut rt: HashMap<String, Vec<(i64, f64)>> = HashMap::new();
    let (mut polls, mut t_first, mut t_last) = (0, i64::MAX, i64::MIN);
    let lines = |f: &str| -> Result<Vec<Value>> {
        let p = dir.join(f);
        if !p.exists() {
            return Ok(vec![]);
        }
        let r = std::io::BufReader::new(std::fs::File::open(p)?);
        Ok(r.lines()
            .map_while(|l| l.ok())
            .filter_map(|l| serde_json::from_str(&l).ok()) // a truncated last line is skipped
            .collect())
    };
    for v in lines("rt.jsonl")? {
        let Some(ts) = v["ts"].as_i64() else { continue };
        polls += 1;
        t_first = t_first.min(ts);
        t_last = t_last.max(ts);
        if let Some(m) = v["p"].as_object() {
            for (pair, x) in m {
                if let Some(px) = x[0].as_f64().filter(|p| *p > 0.0) {
                    let s = rt.entry(pair.clone()).or_default();
                    if s.last().is_none_or(|l| l.0 < ts) {
                        s.push((ts, px));
                    }
                }
            }
        }
    }
    let mut books: HashMap<String, Vec<(i64, f64, f64)>> = HashMap::new();
    for v in lines("books.jsonl")? {
        let (Some(pair), Some(ts)) = (v["pair"].as_str(), v["ts"].as_i64()) else {
            continue;
        };
        let side = |k: &str| -> Vec<(f64, f64)> {
            v[k].as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|l| Some((l[0].as_f64()?, l[1].as_f64()?)))
                        .collect()
                })
                .unwrap_or_default()
        };
        let (bids, asks) = (side("bids"), side("asks"));
        let (Some(b), Some(a)) = (bids.first(), asks.first()) else {
            continue;
        };
        if b.0 >= a.0 {
            continue; // crossed book fails integrity
        }
        let Some(obi) = micro::obi(&bids, &asks, 10) else {
            continue;
        };
        books
            .entry(pair.into())
            .or_default()
            .push((ts, obi, (a.0 - b.0) / micro::mid(b.0, a.0)));
    }
    for b in books.values_mut() {
        b.sort_by_key(|x| x.0);
        b.dedup_by_key(|x| x.0);
    }
    let mut fees = HashMap::new();
    for v in lines("instruments.jsonl")? {
        let i = &v["instrument"];
        if let (Some(p), Some(f)) = (i["pair"].as_str(), i["taker_fee"].as_f64()) {
            fees.insert(p.to_string(), f / 100.0);
        }
    }
    for s in rt.values_mut() {
        s.sort_by_key(|x| x.0);
    }
    Ok(Recording {
        rt,
        books,
        fees,
        rt_polls: polls,
        t_first,
        t_last,
    })
}

fn sigma_per_s(h: &[(i64, f64)]) -> Option<f64> {
    if h.len() < 30 {
        return None;
    }
    let p: Vec<f64> = h.iter().map(|x| x.1).collect();
    let r = stats::log_returns(&p);
    let dt = (h.last()?.0 - h.first()?.0) as f64 / 1000.0 / (h.len() - 1) as f64;
    let rv = stats::realized_vol(&r)?;
    (rv > 0.0 && dt > 0.0).then(|| rv / dt.sqrt())
}

/// Replay RT samples. `use_book`: desk model (requires a fresh book, OBI from it).
pub fn rt_samples(rec: &Recording, p: &Params, use_book: bool, stride_ms: i64) -> Vec<Sample> {
    let mut out = vec![];
    let mut pairs: Vec<&String> = rec.rt.keys().collect();
    pairs.sort();
    for pair in pairs {
        let s = &rec.rt[pair];
        let books = rec.books.get(pair);
        if use_book && books.is_none() {
            continue;
        }
        let fee = rec.fees.get(pair).copied().unwrap_or(DEFAULT_TAKER_FEE);
        let mut next_t = i64::MIN;
        for i in 0..s.len() {
            let (t, entry) = s[i];
            if t < next_t {
                continue;
            }
            let lo = i.saturating_sub(599);
            let Some(sig) = sigma_per_s(&s[lo..=i]) else {
                continue;
            };
            // r_60: last sample vs latest sample with ts <= t - 60 s (desk `ret_over`)
            let j = s[..=i].partition_point(|x| x.0 <= t - 60_000);
            let r60 = if j > 0 {
                (entry / s[j - 1].1).ln()
            } else {
                0.0
            };
            // freshest book with ts <= t, at most 5 s old
            let book = books.and_then(|b| {
                let k = b.partition_point(|x| x.0 <= t);
                (k > 0 && t - b[k - 1].0 <= 5_000).then(|| b[k - 1])
            });
            if use_book && book.is_none() {
                continue;
            }
            let obi = if use_book {
                book.map_or(0.0, |b| b.1)
            } else {
                0.0
            };
            // label window (t, t+120 s]; require coverage to >= t + 115 s
            let end = s.partition_point(|x| x.0 <= t + 120_000);
            if end <= i + 1 || s[end - 1].0 < t + 115_000 {
                continue;
            }
            let Some(f) = forecast(p, sig, r60, obi) else {
                continue;
            };
            let path: Vec<f64> = s[i + 1..end].iter().map(|x| x.1).collect();
            let Some(o) = label_path(entry, f.tp, f.sl, &path) else {
                continue;
            };
            let up = UP_HORIZONS_S
                .iter()
                .filter_map(|&h| {
                    let k = s.partition_point(|x| x.0 < t + h * 1000);
                    let x = s.get(k).filter(|x| x.0 <= t + h * 1000 + 1_500)?;
                    let y = if x.1 > entry {
                        Some(true)
                    } else if x.1 < entry {
                        Some(false)
                    } else {
                        None
                    };
                    Some((h, p_up(&f, h as f64), y))
                })
                .collect();
            out.push(Sample {
                t_ms: t,
                pair: pair.clone(),
                f,
                cost: book.map(|b| 2.0 * fee + b.2 + 2.0 * SLIPPAGE_PER_SIDE),
                out: o,
                up,
            });
            next_t = t + stride_ms;
        }
    }
    out
}

/// Replay completed, contiguous 1m candles of one pair.
pub fn candle_samples(
    pair: &str,
    candles: &[qe_math::timeframe::Candle],
    p: &Params,
    cost: Option<f64>,
) -> Vec<Sample> {
    let c: Vec<_> = candles
        .iter()
        .filter(|c| c.complete && c.width_ms == 60_000)
        .collect();
    let mut out = vec![];
    for i in 60..c.len().saturating_sub(2) {
        let w = &c[i - 60..=i + 2];
        if w.windows(2).any(|x| x[1].open_ms != x[0].open_ms + 60_000) {
            continue; // gap in history: skip rather than invent
        }
        let closes: Vec<f64> = c[i - 60..=i].iter().map(|x| x.close).collect();
        let r = stats::log_returns(&closes);
        let Some(rv) = stats::realized_vol(&r) else {
            continue;
        };
        let sig = rv / 60f64.sqrt();
        let entry = c[i].close;
        let r60 = (c[i].close / c[i - 1].close).ln();
        let Some(f) = forecast(p, sig, r60, 0.0) else {
            continue;
        };
        let path: Vec<(f64, f64, f64, f64)> = c[i + 1..=i + 2]
            .iter()
            .map(|x| (x.open, x.high, x.low, x.close))
            .collect();
        let Some(o) = label_candles(entry, f.tp, f.sl, &path) else {
            continue;
        };
        let up = [(60i64, c[i + 1].close), (120, c[i + 2].close)]
            .iter()
            .map(|&(h, px)| {
                let y = if px > entry {
                    Some(true)
                } else if px < entry {
                    Some(false)
                } else {
                    None
                };
                (h, p_up(&f, h as f64), y)
            })
            .collect();
        out.push(Sample {
            t_ms: c[i].close_ms(),
            pair: pair.into(),
            f,
            cost,
            out: o,
            up,
        });
    }
    out
}

// ---------------------------------------------------------------- scoring

pub fn entries(
    model: &str,
    dataset: &str,
    samples: &[Sample],
    folds: usize,
) -> Vec<CalibrationEntry> {
    let mut v = vec![];
    for (side, idx) in [("long", 0usize), ("short", 1)] {
        let pts: Vec<Point> = samples
            .iter()
            .map(|s| {
                let fp = if idx == 0 { s.f.long } else { s.f.short };
                Point {
                    t_ms: s.t_ms,
                    p: fp.p_target,
                    y: s.out[idx].0 == Outcome::Target,
                }
            })
            .collect();
        v.push(CalibrationEntry::from_points(
            model,
            &format!("tp_first_{side}"),
            HORIZON_S,
            dataset,
            &pts,
            folds,
        ));
        let pts: Vec<Point> = samples
            .iter()
            .map(|s| {
                let fp = if idx == 0 { s.f.long } else { s.f.short };
                Point {
                    t_ms: s.t_ms,
                    p: fp.p_stop,
                    y: s.out[idx].0 == Outcome::Stop,
                }
            })
            .collect();
        v.push(CalibrationEntry::from_points(
            model,
            &format!("sl_first_{side}"),
            HORIZON_S,
            dataset,
            &pts,
            folds,
        ));
    }
    let mut hs: Vec<i64> = samples
        .iter()
        .flat_map(|s| s.up.iter().map(|u| u.0))
        .collect();
    hs.sort();
    hs.dedup();
    for h in hs {
        let pts: Vec<Point> = samples
            .iter()
            .flat_map(|s| s.up.iter().filter(|u| u.0 == h).map(move |u| (s.t_ms, u)))
            .filter_map(|(t, u)| u.2.map(|y| Point { t_ms: t, p: u.1, y }))
            .collect();
        v.push(CalibrationEntry::from_points(
            model,
            &format!("up_{h}s"),
            h as f64,
            dataset,
            &pts,
            folds,
        ));
    }
    v
}

fn max_drawdown(x: &[f64]) -> f64 {
    let (mut cum, mut peak, mut dd) = (0.0f64, 0.0f64, 0.0f64);
    for v in x {
        cum += v;
        peak = peak.max(cum);
        dd = dd.max(peak - cum);
    }
    dd
}

/// Trading metrics: EV>0 selection at each cost multiplier, plus unconditional
/// realised gross returns. Units: basis points of notional per trade.
pub fn trading(samples: &[Sample]) -> Value {
    let mut s: Vec<&Sample> = samples.iter().filter(|s| s.cost.is_some()).collect();
    s.sort_by_key(|x| x.t_ms);
    let n = s.len();
    if n == 0 {
        return json!({"n_with_cost": 0, "note": "no samples with a measured spread; net expectancy not computed"});
    }
    let mean = |v: &[f64]| {
        if v.is_empty() {
            f64::NAN
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    let costs: Vec<f64> = s.iter().map(|x| x.cost.unwrap() * 1e4).collect();
    let gl: Vec<f64> = s.iter().map(|x| x.out[0].1 * 1e4).collect();
    let gs: Vec<f64> = s.iter().map(|x| x.out[1].1 * 1e4).collect();
    let mut sens = vec![];
    for m in COST_MULTS {
        let mut net = vec![];
        let mut pred = vec![];
        let mut wins = 0usize;
        for x in &s {
            let c = x.cost.unwrap() * m;
            let el = bracket_ev(&x.f.long, x.f.tp, x.f.sl, c);
            let es = bracket_ev(&x.f.short, x.f.tp, x.f.sl, c);
            let (ev, idx) = if el >= es { (el, 0) } else { (es, 1) };
            if ev > 0.0 {
                let r = x.out[idx].1 - c;
                if r > 0.0 {
                    wins += 1;
                }
                net.push(r * 1e4);
                pred.push(ev * 1e4);
            }
        }
        sens.push(json!({"cost_multiplier": m, "n_trades": net.len(),
            "mean_predicted_ev_bps": mean(&pred), "mean_realised_net_bps": mean(&net),
            "win_rate": if net.is_empty() { f64::NAN } else { wins as f64 / net.len() as f64 },
            "sum_net_bps": net.iter().sum::<f64>(), "max_drawdown_bps": max_drawdown(&net)}));
    }
    let tp_rate =
        |i: usize| s.iter().filter(|x| x.out[i].0 == Outcome::Target).count() as f64 / n as f64;
    let sl_rate =
        |i: usize| s.iter().filter(|x| x.out[i].0 == Outcome::Stop).count() as f64 / n as f64;
    json!({
        "n_with_cost": n,
        "mean_cost_bps": mean(&costs),
        "unconditional": {
            "long": {"mean_gross_bps": mean(&gl), "mean_net_bps": mean(&gl) - mean(&costs),
                      "tp_first_rate": tp_rate(0), "sl_first_rate": sl_rate(0),
                      "mean_pred_tp_first": s.iter().map(|x| x.f.long.p_target).sum::<f64>() / n as f64},
            "short": {"mean_gross_bps": mean(&gs), "mean_net_bps": mean(&gs) - mean(&costs),
                      "tp_first_rate": tp_rate(1), "sl_first_rate": sl_rate(1),
                      "mean_pred_tp_first": s.iter().map(|x| x.f.short.p_target).sum::<f64>() / n as f64},
            "mean_tp_bps": s.iter().map(|x| x.f.tp).sum::<f64>() / n as f64 * 1e4,
            "mean_sl_bps": s.iter().map(|x| x.f.sl).sum::<f64>() / n as f64 * 1e4,
        },
        "ev_positive_selection_by_cost_multiplier": sens,
    })
}

/// Summary of a sample set for the report.
pub fn describe(samples: &[Sample], stride_s: f64) -> Value {
    let mut pairs: BTreeMap<&str, usize> = BTreeMap::new();
    for s in samples {
        *pairs.entry(&s.pair).or_default() += 1;
    }
    let ties: usize = samples
        .iter()
        .flat_map(|s| &s.up)
        .filter(|u| u.2.is_none())
        .count();
    let ups: usize = samples.iter().map(|s| s.up.len()).sum();
    let (t0, t1) = (
        samples.iter().map(|s| s.t_ms).min(),
        samples.iter().map(|s| s.t_ms).max(),
    );
    json!({"n_samples": samples.len(), "n_pairs": pairs.len(), "t_first_ms": t0, "t_last_ms": t1,
        "decision_stride_s": stride_s,
        "approx_independent_120s_windows": (samples.len() as f64 * stride_s / HORIZON_S).round(),
        "up_label_tie_fraction": if ups > 0 { ties as f64 / ups as f64 } else { f64::NAN }})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_labels_are_conservative() {
        // long hits TP at +tp; short sees the same path as an adverse move
        let p = [100.0, 100.3, 100.6];
        let o = label_path(100.0, 0.005, 0.005, &p).unwrap();
        assert_eq!(o[0], (Outcome::Target, 0.005));
        assert_eq!(o[1].0, Outcome::Stop);
        assert!(o[1].1 <= -0.005); // stop realises the observed (gapped) price
        let o = label_path(100.0, 0.01, 0.01, &[100.1, 99.95]).unwrap();
        assert_eq!(o[0].0, Outcome::Neither);
        assert!((o[0].1 - (0.9995f64).ln()).abs() < 1e-12);
        assert!((o[1].1 + o[0].1).abs() < 1e-15);
    }

    #[test]
    fn candle_ambiguity_assumes_stop_first() {
        // one candle spans both barriers: order unknown -> stop for both sides
        let o = label_candles(100.0, 0.01, 0.01, &[(100.0, 102.0, 98.0, 100.0)]).unwrap();
        assert_eq!(o[0].0, Outcome::Stop);
        assert_eq!(o[1].0, Outcome::Stop);
        let o = label_candles(100.0, 0.01, 0.01, &[(100.0, 101.5, 99.5, 101.0)]).unwrap();
        assert_eq!(o[0], (Outcome::Target, 0.01));
        assert_eq!(o[1].0, Outcome::Stop);
    }

    #[test]
    fn forecast_matches_desk_formula() {
        let f = forecast(&DESK_PARAMS, 0.0005, 0.003, 0.4).unwrap();
        let mu = 0.2 * 0.003 / 60.0 + 0.05 * 0.4 * 0.0005;
        assert!((f.mu - mu).abs() < 1e-18);
        assert!((f.tp - 1.5 * 0.0005 * 120f64.sqrt()).abs() < 1e-15);
        assert!(f.long.p_target > f.short.p_target);
        assert!((p_up(&f, 60.0) - norm_cdf(mu * 60.0 / (0.0005 * 60f64.sqrt()))).abs() < 1e-15);
    }

    #[test]
    fn loads_store_and_rejects_bad_records() {
        let d = std::env::temp_dir().join(format!("qe-research-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("rt.jsonl"),
            concat!(
                r#"{"recv_ms":2,"ts":1000,"vs":1,"p":{"B-X_USDT":[100.0,100.0,5.0]}}"#,
                "\n",
                r#"{"recv_ms":3,"ts":1000,"vs":1,"p":{"B-X_USDT":[101.0,100.0,5.0]}}"#,
                "\n",
                r#"{"recv_ms":4,"ts":2000,"vs":2,"p":{"B-X_USDT":[102.0,null,5.0]}}"#,
                "\n",
                r#"{"recv_ms":5,"ts":3000,"p":{"B-X_US"#
            ),
        )
        .unwrap();
        std::fs::write(
            d.join("books.jsonl"),
            concat!(
                r#"{"pair":"B-X_USDT","ts":1500,"bids":[[99.0,3.0]],"asks":[[101.0,1.0]]}"#,
                "\n",
                r#"{"pair":"B-X_USDT","ts":1600,"bids":[[102.0,3.0]],"asks":[[101.0,1.0]]}"#,
                "\n"
            ),
        )
        .unwrap();
        std::fs::write(
            d.join("instruments.jsonl"),
            r#"{"instrument":{"pair":"B-X_USDT","taker_fee":0.05}}"#,
        )
        .unwrap();
        let r = load_recording(&d).unwrap();
        std::fs::remove_dir_all(&d).unwrap();
        // duplicate exchange ts keeps the first; truncated last line skipped
        assert_eq!(r.rt["B-X_USDT"], vec![(1000, 100.0), (2000, 102.0)]);
        assert_eq!(r.rt_polls, 3);
        // crossed book (bid >= ask) dropped
        assert_eq!(r.books["B-X_USDT"].len(), 1);
        assert!((r.books["B-X_USDT"][0].1 - 0.5).abs() < 1e-12);
        assert!((r.books["B-X_USDT"][0].2 - 0.02).abs() < 1e-12);
        assert!((r.fees["B-X_USDT"] - 0.0005).abs() < 1e-15);
    }

    #[test]
    fn rt_replay_uses_no_future_data() {
        // flat history then a jump after t: the forecast at t must not see it
        let mut rt = HashMap::new();
        let s: Vec<(i64, f64)> = (0..400)
            .map(|i| {
                (
                    i * 1000,
                    if i < 200 {
                        100.0 + 0.01 * (i % 3) as f64
                    } else {
                        110.0
                    },
                )
            })
            .collect();
        rt.insert("B-X_USDT".to_string(), s);
        let rec = Recording {
            rt,
            books: HashMap::new(),
            fees: HashMap::new(),
            rt_polls: 400,
            t_first: 0,
            t_last: 399_000,
        };
        let v = rt_samples(&rec, &DESK_PARAMS, false, 1_000);
        let at = v.iter().find(|x| x.t_ms == 150_000).unwrap();
        // drift at t=150 s reflects only the flat regime
        assert!(at.f.mu.abs() < 1e-6, "{}", at.f.mu);
        // but its label (t, t+120 s] does contain the jump at 200 s
        assert_eq!(at.out[0].0, Outcome::Target);
        // no sample is emitted without full 115 s label coverage
        assert!(v.iter().all(|x| x.t_ms + 115_000 <= 399_000));
    }
}
