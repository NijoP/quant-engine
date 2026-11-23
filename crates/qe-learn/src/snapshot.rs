//! §6.1 Immutable pre-trade decision snapshot and the append-only decision log.
//!
//! A snapshot is written *before* any order exists, for approved and rejected
//! proposals alike. Once sealed it is never modified: the log exposes only
//! shared references, outcomes are stored separately (§6.2), and each sealed
//! record carries a SHA-256 over its canonical JSON so a replayed journal can
//! prove nothing was rewritten.
use crate::types::{finite, prob, Direction, LearnError, Mode, Result, HORIZONS_S, LIFETIME_MS};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

/// Forecast for one horizon, as the predictive engine produced it at decision time.
/// Returns are simple fractional returns of the mid from `ref_price` (market
/// terms, not direction-adjusted).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HorizonForecast {
    pub horizon_s: u32,
    pub expected_return: f64,
    /// standard deviation of the predicted return distribution
    pub return_sd: f64,
    pub p_up: f64,
    /// confidence interval of the return (e.g. 90 %)
    pub ci_low: f64,
    pub ci_high: f64,
}

/// Input data quality at decision time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataQuality {
    /// age of the order-book snapshot used, ms
    pub book_age_ms: i64,
    /// age of the last trade print used, ms
    pub trade_age_ms: i64,
    /// false when the snapshot was marked invalid / mis-sequenced
    pub book_valid: bool,
    /// observed interval between consecutive book snapshots, ms (futures
    /// books are snapshots, not deltas: liquidity between them is unseen)
    pub book_snapshot_interval_ms: i64,
}

/// The independent risk governor's verdict, copied verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskDecision {
    pub approved: bool,
    pub codes: Vec<String>,
    /// maximum loss the governor allowed for this trade (quote currency)
    pub risk_budget: Decimal,
}

/// Everything known at the moment of decision (intent §6.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionSnapshot {
    pub trade_id: String,
    pub instrument: String,
    /// `spot` | `inr_futures` | `usdt_futures`
    pub market: String,
    pub mode: Mode,
    pub ts_ms: i64,
    pub direction: Direction,
    pub features: BTreeMap<String, f64>,
    pub regime: String,
    /// multi-timeframe context (e.g. `1h.trend`, `5m.vol`)
    pub timeframe_context: BTreeMap<String, f64>,
    pub book_features: BTreeMap<String, f64>,
    pub data_quality: DataQuality,
    /// per-horizon forecasts, ascending horizon, horizons from [`HORIZONS_S`]
    pub forecasts: Vec<HorizonForecast>,
    /// the horizon the trade is held for (<= 120 s)
    pub trade_horizon_s: u32,
    pub win_probability: f64,
    /// P(TP before SL within the lifetime)
    pub p_tp_first: f64,
    /// P(SL before TP within the lifetime); P(neither) = 1 - both
    pub p_sl_first: f64,
    /// expected max favourable / adverse excursion, fraction of ref price (>= 0)
    pub expected_mfe: f64,
    pub expected_mae: f64,
    /// spread between ensemble members' predicted return, in predicted-sd units
    pub model_disagreement_sd: Option<f64>,
    /// round-trip half-spread crossing (= full spread), bps of notional
    pub est_spread_bps: f64,
    /// round-trip fees, bps of notional
    pub est_fee_bps: f64,
    /// round-trip slippage beyond the touch, bps of notional
    pub est_slippage_bps: f64,
    /// mid price at decision
    pub ref_price: Decimal,
    pub stop_price: Decimal,
    pub target_price: Decimal,
    pub qty: Decimal,
    pub notional: Decimal,
    pub margin: Decimal,
    pub leverage: Decimal,
    pub sizing_note: String,
    pub strategy_version: String,
    pub model_version: String,
    pub risk: RiskDecision,
    pub reasoning_codes: Vec<String>,
}

impl DecisionSnapshot {
    pub fn p_neither(&self) -> f64 {
        (1.0 - self.p_tp_first - self.p_sl_first).max(0.0)
    }
    /// estimated round-trip execution cost, bps of notional
    pub fn est_cost_bps(&self) -> f64 {
        self.est_spread_bps + self.est_fee_bps + self.est_slippage_bps
    }
    pub fn forecast_at(&self, h: u32) -> Option<&HorizonForecast> {
        self.forecasts.iter().find(|f| f.horizon_s == h)
    }
    /// forecast at the trade horizon, else the longest one recorded
    pub fn trade_forecast(&self) -> Option<&HorizonForecast> {
        self.forecast_at(self.trade_horizon_s)
            .or(self.forecasts.last())
    }
    pub fn stop_distance_frac(&self) -> f64 {
        rel_dist(self.ref_price, self.stop_price)
    }
    pub fn target_distance_frac(&self) -> f64 {
        rel_dist(self.ref_price, self.target_price)
    }

    pub fn validate(&self) -> Result<()> {
        for (n, v) in [
            ("trade_id", &self.trade_id),
            ("instrument", &self.instrument),
            ("market", &self.market),
            ("model_version", &self.model_version),
            ("strategy_version", &self.strategy_version),
        ] {
            if v.trim().is_empty() {
                return Err(LearnError::Invalid(format!("{n} is empty")));
            }
        }
        if self.ts_ms <= 0 {
            return Err(LearnError::Invalid("ts_ms must be > 0".into()));
        }
        prob("win_probability", self.win_probability)?;
        prob("p_tp_first", self.p_tp_first)?;
        prob("p_sl_first", self.p_sl_first)?;
        if self.p_tp_first + self.p_sl_first > 1.0 + 1e-9 {
            return Err(LearnError::Invalid("p_tp_first + p_sl_first > 1".into()));
        }
        if self.trade_horizon_s == 0 || i64::from(self.trade_horizon_s) * 1000 > LIFETIME_MS {
            return Err(LearnError::Invalid(
                "trade_horizon_s must be in 1..=120".into(),
            ));
        }
        for (n, x) in [
            ("expected_mfe", self.expected_mfe),
            ("expected_mae", self.expected_mae),
            ("est_spread_bps", self.est_spread_bps),
            ("est_fee_bps", self.est_fee_bps),
            ("est_slippage_bps", self.est_slippage_bps),
        ] {
            finite(n, x)?;
            if x < 0.0 {
                return Err(LearnError::Invalid(format!("{n} < 0")));
            }
        }
        if let Some(d) = self.model_disagreement_sd {
            finite("model_disagreement_sd", d)?;
        }
        for (k, v) in self
            .features
            .iter()
            .chain(&self.timeframe_context)
            .chain(&self.book_features)
        {
            finite(k, *v)?;
        }
        if self.forecasts.is_empty() {
            return Err(LearnError::Invalid("no forecasts".into()));
        }
        let mut last = 0;
        for f in &self.forecasts {
            if !HORIZONS_S.contains(&f.horizon_s) || f.horizon_s <= last {
                return Err(LearnError::Invalid(format!(
                    "forecast horizon {}s not ascending / not in {HORIZONS_S:?}",
                    f.horizon_s
                )));
            }
            last = f.horizon_s;
            finite("expected_return", f.expected_return)?;
            finite("return_sd", f.return_sd)?;
            finite("ci", f.ci_low)?;
            finite("ci", f.ci_high)?;
            prob("p_up", f.p_up)?;
            if f.return_sd < 0.0 || f.ci_low > f.ci_high {
                return Err(LearnError::Invalid(format!(
                    "bad distribution at {}s",
                    f.horizon_s
                )));
            }
        }
        if self.risk.approved {
            let s = self.direction.sign_dec();
            if self.ref_price <= Decimal::ZERO || self.qty <= Decimal::ZERO {
                return Err(LearnError::Invalid(
                    "approved trade needs ref_price, qty > 0".into(),
                ));
            }
            if self.leverage < Decimal::ONE {
                return Err(LearnError::Invalid("leverage < 1".into()));
            }
            if self.risk.risk_budget <= Decimal::ZERO {
                return Err(LearnError::Invalid(
                    "approved trade needs risk_budget > 0".into(),
                ));
            }
            // mandatory SL and TP, on the correct sides of the reference price
            if (self.ref_price - self.stop_price) * s <= Decimal::ZERO {
                return Err(LearnError::Invalid("stop on wrong side / missing".into()));
            }
            if (self.target_price - self.ref_price) * s <= Decimal::ZERO {
                return Err(LearnError::Invalid("target on wrong side / missing".into()));
            }
        }
        Ok(())
    }
}

fn rel_dist(a: Decimal, b: Decimal) -> f64 {
    if a.is_zero() {
        return 0.0;
    }
    crate::types::to_f64(((b - a) / a).abs())
}

/// SHA-256 of the canonical JSON (struct field order is fixed, maps are BTreeMaps).
pub fn seal_hash<T: Serialize>(rec: &T) -> String {
    let bytes = serde_json::to_vec(rec).expect("records always serialise");
    hex::encode(Sha256::digest(bytes))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SealedDecision {
    pub seq: u64,
    pub sha256: String,
    pub record: DecisionSnapshot,
}

impl SealedDecision {
    pub fn verify(&self) -> bool {
        seal_hash(&self.record) == self.sha256
    }
}

/// Append-only, time-ordered decision log. No method hands out `&mut`.
#[derive(Debug, Default, Clone)]
pub struct DecisionLog {
    rows: Vec<SealedDecision>,
    by_id: HashMap<String, usize>,
}

impl DecisionLog {
    pub fn append(&mut self, snap: DecisionSnapshot) -> Result<&SealedDecision> {
        snap.validate()?;
        if self.by_id.contains_key(&snap.trade_id) {
            return Err(LearnError::Duplicate(snap.trade_id));
        }
        if let Some(last) = self.rows.last() {
            if snap.ts_ms < last.record.ts_ms {
                return Err(LearnError::NotChronological {
                    last_ms: last.record.ts_ms,
                    got_ms: snap.ts_ms,
                });
            }
        }
        let sealed = SealedDecision {
            seq: self.rows.len() as u64,
            sha256: seal_hash(&snap),
            record: snap,
        };
        self.by_id
            .insert(sealed.record.trade_id.clone(), self.rows.len());
        self.rows.push(sealed);
        Ok(self.rows.last().expect("just pushed"))
    }
    pub fn get(&self, trade_id: &str) -> Option<&SealedDecision> {
        self.by_id.get(trade_id).map(|&i| &self.rows[i])
    }
    pub fn iter(&self) -> impl Iterator<Item = &SealedDecision> {
        self.rows.iter()
    }
    pub fn len(&self) -> usize {
        self.rows.len()
    }
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
    /// recompute every hash; returns trade ids whose record no longer matches
    pub fn verify_all(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|r| !r.verify())
            .map(|r| r.record.trade_id.clone())
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::types::dec;

    /// test helper: approved paper long at 100, SL 99.5 (-50 bps), TP 100.6
    /// (+60 bps); 120 s forecast +4 bps, sd 40 bps.
    pub fn snap(id: &str, ts: i64) -> DecisionSnapshot {
        let forecasts = HORIZONS_S
            .iter()
            .map(|&h| {
                let sd = 0.004 * (h as f64 / 120.0).sqrt();
                HorizonForecast {
                    horizon_s: h,
                    expected_return: 0.0004 * h as f64 / 120.0,
                    return_sd: sd,
                    p_up: 0.58,
                    ci_low: -1.645 * sd,
                    ci_high: 1.645 * sd,
                }
            })
            .collect();
        DecisionSnapshot {
            trade_id: id.into(),
            instrument: "B-BTC_USDT".into(),
            market: "usdt_futures".into(),
            mode: Mode::Paper,
            ts_ms: ts,
            direction: Direction::Long,
            features: BTreeMap::from([("obi_5".into(), 0.3), ("mom_10s".into(), 0.0002)]),
            regime: "trend_up".into(),
            timeframe_context: BTreeMap::from([("1h.trend".into(), 1.0)]),
            book_features: BTreeMap::from([("spread_bps".into(), 1.0)]),
            data_quality: DataQuality {
                book_age_ms: 200,
                trade_age_ms: 300,
                book_valid: true,
                book_snapshot_interval_ms: 500,
            },
            forecasts,
            trade_horizon_s: 120,
            win_probability: 0.6,
            p_tp_first: 0.55,
            p_sl_first: 0.35,
            expected_mfe: 0.004,
            expected_mae: 0.003,
            model_disagreement_sd: Some(0.2),
            est_spread_bps: 1.0,
            est_fee_bps: 10.0,
            est_slippage_bps: 1.0,
            ref_price: dec("100"),
            stop_price: dec("99.5"),
            target_price: dec("100.6"),
            qty: dec("1"),
            notional: dec("100"),
            margin: dec("10"),
            leverage: dec("10"),
            sizing_note: "1% risk".into(),
            strategy_version: "fp-bracket.v1".into(),
            model_version: "desk.fp.v1".into(),
            risk: RiskDecision {
                approved: true,
                codes: vec!["ok".into()],
                risk_budget: dec("1"),
            },
            reasoning_codes: vec!["ev_pos".into()],
        }
    }

    #[test]
    fn append_only_rejects_duplicates_and_out_of_order() {
        let mut log = DecisionLog::default();
        log.append(snap("a", 1_000)).unwrap();
        assert!(matches!(
            log.append(snap("a", 2_000)),
            Err(LearnError::Duplicate(_))
        ));
        assert!(matches!(
            log.append(snap("b", 500)),
            Err(LearnError::NotChronological { .. })
        ));
        assert_eq!(log.len(), 1);
        assert!(log.verify_all().is_empty());
    }

    #[test]
    fn hash_detects_any_change() {
        let mut log = DecisionLog::default();
        let s = log.append(snap("a", 1_000)).unwrap().clone();
        let mut forged = s.clone();
        forged.record.p_tp_first = 0.9;
        assert!(s.verify());
        assert!(!forged.verify());
    }

    #[test]
    fn approved_trade_requires_stop_and_target_on_correct_side() {
        let mut s = snap("a", 1);
        s.stop_price = dec("100.1");
        assert!(s.validate().is_err());
        let mut s = snap("a", 1);
        s.target_price = dec("99");
        assert!(s.validate().is_err());
        let mut s = snap("a", 1);
        s.direction = Direction::Short; // SL below, TP above is wrong for a short
        assert!(s.validate().is_err());
        // a rejected proposal is still recorded, without a bracket
        let mut s = snap("a", 1);
        s.risk.approved = false;
        s.stop_price = Decimal::ZERO;
        s.target_price = Decimal::ZERO;
        s.validate().unwrap();
    }

    #[test]
    fn rejects_bad_probabilities_and_horizons() {
        let mut s = snap("a", 1);
        s.p_tp_first = 0.8;
        s.p_sl_first = 0.3;
        assert!(s.validate().is_err());
        let mut s = snap("a", 1);
        s.forecasts[0].horizon_s = 2;
        assert!(s.validate().is_err());
        let mut s = snap("a", 1);
        s.features.insert("x".into(), f64::NAN);
        assert!(s.validate().is_err());
    }
}
