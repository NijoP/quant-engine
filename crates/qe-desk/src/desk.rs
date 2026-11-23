//! Desk state and the deterministic decision pipeline:
//! DATA -> MATHEMATICS -> PROBABILISTIC FORECAST -> STRATEGY -> RISK GOVERNOR
//! -> (paper) EXECUTION -> FEEDBACK.
//!
//! Everything here runs under one `RwLock<Desk>`; network I/O happens in the
//! loops in `main.rs`, never while a decision is being computed.
use crate::store::Store;
use qe_coindcx::types::{Book, FuturesInstrument, SpotMarketDetail};
use qe_engine::capital::{CapitalController, CapitalPlan, Stage, Step};
use qe_engine::paper::SimFill;
use qe_engine::Decimal;
use qe_math::timeframe::{self, Candle as MCandle, ContextTf};
use qe_math::{features, micro, prob, stats};
use qe_risk::{
    AccountSnapshot, Action, CapitalMode, Decision, InstrumentCapability, LeverageTier,
    PaperExchange, Position, RiskGovernor, RiskPolicy, Side, State, TradeProposal,
};

/// f64 -> Decimal for the money ledger (non-finite values book as zero).
pub fn dec(x: f64) -> Decimal {
    Decimal::from_f64_retain(x).unwrap_or_default().round_dp(8)
}

/// Decimal -> f64 for the (f64) risk and display paths.
pub fn dec_f64(x: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    x.to_f64().unwrap_or(0.0)
}
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

pub const HORIZON_S: f64 = 120.0;
/// SL = K_STOP * sigma_120, TP = K_TARGET * sigma_120 (priors, not yet calibrated).
pub const K_STOP: f64 = 1.0;
pub const K_TARGET: f64 = 1.5;
/// Drift prior: mu = LAMBDA_MOM * r_60 / 60 + GAMMA_OBI * OBI_10 * sigma_1s.
pub const LAMBDA_MOM: f64 = 0.2;
pub const GAMMA_OBI: f64 = 0.05;
pub const MODEL_ID: &str = "drift-mom-obi-v1+first-passage-lattice";
/// Forecast horizons (seconds) reported per instrument.
pub const HORIZONS: [f64; 9] = [1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 90.0, 120.0];
/// Candidates that get order books, forecasts and multi-timeframe context.
pub const TOP_N: usize = 6;
/// The paper account is INR, so it trades INR-margined futures.
pub const TRADE_MARGIN: &str = "INR";
/// Calibration acceptance: enough resolved forward samples, low ECE, and a
/// Brier score better than the climatological base rate.
pub const CALIB_MIN_N: usize = 200;
pub const CALIB_MAX_ECE: f64 = 0.05;

/// Opt-in paper experiment profile (configs/experiment-max-leverage.toml).
pub const EXPERIMENT_PROFILE: &str = "experiment-max-leverage";
pub const EXPERIMENT_BANNER: &str = "PAPER EXPERIMENT - UNGATED, MAX LEVERAGE";

/// Owner-armed gamble profile (configs/gamble.toml, docs/GAMBLE_MODE.md).
pub const GAMBLE_PROFILE: &str = "gamble";
pub const GAMBLE_BANNER: &str = "GAMBLE MODE - owner armed, no proven edge";
/// Marker written when the floor stop fires; startup refuses while it exists.
pub const GAMBLE_STOP_FILE: &str = "gamble-stopped.json";
/// Candidates older than this drop off the gamble ranking board.
pub const GAMBLE_BOARD_MS: i64 = 15_000;

/// Startup guard for the experiment profile: it may only run in paper mode.
/// Every execution-mode source must say paper (an absent env var means paper);
/// anything else refuses startup.
pub fn experiment_guard(
    profile_mode: Option<&str>,
    limits_mode: Option<&str>,
    env_mode: Option<&str>,
) -> Result<(), String> {
    match profile_mode {
        Some("paper") => {}
        other => {
            return Err(format!(
            "experiment profile requires execution_mode = \"paper\" in its config (got {other:?})"
        ))
        }
    }
    if let Some(m) = limits_mode.filter(|m| *m != "paper") {
        return Err(format!(
            "experiment profile refused: risk-limits execution_mode = {m:?}, not paper"
        ));
    }
    if let Some(m) = env_mode.filter(|m| !m.trim().eq_ignore_ascii_case("paper")) {
        return Err(format!(
            "experiment profile refused: QE_EXECUTION_MODE = {m:?}, not paper"
        ));
    }
    Ok(())
}

pub fn now_ms() -> i64 {
    qe_coindcx::auth::request_timestamp_ms()
}

/// Settings loaded from configs/*.toml (falls back to the documented defaults).
#[derive(Debug, Clone, Serialize)]
pub struct Settings {
    pub execution_mode: String,
    /// "standard" (default) or [`EXPERIMENT_PROFILE`] (opt-in, paper only).
    pub profile: String,
    pub paper_start_equity_inr: f64,
    pub stage0_end_ms: i64,
    pub stage1_target_inr: f64,
    pub stage1_checkpoint_ms: i64,
    pub withdrawal_inr: f64,
    pub stage2_target_inr: f64,
    pub stage2_deadline_ms: i64,
    pub paper_slippage_bps: f64,
    pub growth: RiskPolicy,
    pub protective: RiskPolicy,
    /// Owner gamble parameters (Some only for the gamble profile).
    pub gamble: Option<qe_risk::gamble::GambleParams>,
    /// Gamble entries as passive limit (maker) orders; see [`MakerEntry`].
    pub gamble_maker_entry: bool,
    /// Cancel an unfilled maker entry after this long; never chased with a taker.
    pub maker_timeout_ms: i64,
    /// Payoff per bet (`[payoff]` in the gamble config, qe_risk::gamble::payoff):
    /// cost-aware exclusion and volatility-scaled TP/SL. None = legacy sigma barriers.
    pub payoff: Option<qe_risk::gamble::payoff::PayoffCfg>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            execution_mode: "paper".into(),
            profile: "standard".into(),
            paper_start_equity_inr: 100.0,
            stage0_end_ms: qe_engine::capital::STAGE0_END_MS as i64,
            stage1_target_inr: 50_000.0,
            stage1_checkpoint_ms: qe_engine::capital::STAGE2_DEADLINE_MS as i64,
            withdrawal_inr: 0.0,
            stage2_target_inr: 50_000.0,
            stage2_deadline_ms: qe_engine::capital::STAGE2_DEADLINE_MS as i64,
            paper_slippage_bps: 2.0,
            growth: RiskPolicy::growth(),
            protective: RiskPolicy::protective(),
            gamble: None,
            gamble_maker_entry: false,
            maker_timeout_ms: 4_000,
            payoff: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub status: String,
    pub last_run_ms: Option<i64>,
    pub last_ok_ms: Option<i64>,
    pub duration_ms: Option<i64>,
    pub runs: u64,
    pub errors: u64,
    pub last_output: Option<String>,
    pub last_error: Option<String>,
    pub permissions: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Controls {
    pub scanner_running: bool,
    pub paper_running: bool,
    pub strategy_enabled: bool,
    pub entries_paused: bool,
    pub killed: bool,
}

/// An open paper position: the qe-risk lifecycle plus execution context.
#[derive(Debug, Clone)]
pub struct PaperPos {
    pub pos: Position,
    pub margin: String,
    pub fx: f64,
    pub taker_fee: f64,
    pub maker_fee: f64,
    pub mark: f64,
    pub entry_ref_mid: f64,
    pub proposal: Value,
    /// Resting paper maker entry, until filled or cancelled.
    pub maker: Option<MakerEntry>,
}

/// A paper passive-limit entry resting at the touch (buy at the bid, sell at
/// the ask). It fills only when public trades print strictly THROUGH its price
/// after it was placed (a print at the price is not enough: our queue position
/// is unknown, so we assume we are last in the queue). Fills are at the limit
/// price and pay the instrument's maker fee. Unfilled at the deadline: the
/// remainder is cancelled (no taker chase).
#[derive(Debug, Clone)]
pub struct MakerEntry {
    pub price: f64,
    pub placed_ms: i64,
    pub deadline_ms: i64,
}

/// Paper maker-entry counters for `/api/state.gamble.maker`.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct MakerStats {
    pub placed: u32,
    pub filled: u32,
    pub partial: u32,
    pub cancelled: u32,
}

/// One forecast awaiting its 120 s outcome (forward, chronological: no lookahead).
#[derive(Debug, Clone)]
pub struct Pending {
    pub pair: String,
    pub long: bool,
    pub ts: i64,
    pub tp: f64,
    pub sl: f64,
    pub p: f64,
}

#[derive(Debug, Default)]
pub struct Calib {
    pub pending: VecDeque<Pending>,
    pub resolved: VecDeque<(f64, bool)>,
    pub last_sample: HashMap<String, i64>,
    pub dropped: u64,
    /// Resolved forward samples per (pair, long): the empirical bet win rate.
    pub by_key: HashMap<(String, bool), qe_risk::gamble::HitCounts>,
}

impl Calib {
    /// At most one sample per pair per 15 s (both sides), to limit overlap.
    pub fn maybe_record(&mut self, pair: &str, now: i64, sides: [(bool, f64, f64, f64); 2]) {
        if self.last_sample.get(pair).is_some_and(|t| now - t < 15_000) {
            return;
        }
        self.last_sample.insert(pair.to_string(), now);
        for (long, tp, sl, p) in sides {
            self.pending.push_back(Pending {
                pair: pair.to_string(),
                long,
                ts: now,
                tp,
                sl,
                p,
            });
        }
    }

    /// Resolve forecasts older than 120 s against the 1 s RT price path.
    /// Outcome y = target touched before stop within 120 s. Paths with gaps
    /// (no sample within 3 s of the start or of the end) are dropped, not guessed.
    pub fn resolve(&mut self, hist: &HashMap<String, VecDeque<(i64, f64, f64)>>, now: i64) {
        while self.pending.front().is_some_and(|p| p.ts + 120_000 <= now) {
            let p = self.pending.pop_front().unwrap();
            let Some(h) = hist.get(&p.pair) else {
                self.dropped += 1;
                continue;
            };
            let path: Vec<&(i64, f64, f64)> = h
                .iter()
                .filter(|x| x.0 > p.ts && x.0 <= p.ts + 120_000)
                .collect();
            let ok = path.first().is_some_and(|x| x.0 - p.ts <= 3_000)
                && path.last().is_some_and(|x| x.0 >= p.ts + 117_000);
            if !ok {
                self.dropped += 1;
                continue;
            }
            let mut y = false;
            for x in path {
                let (hit_t, hit_s) = if p.long {
                    (x.1 >= p.tp, x.1 <= p.sl)
                } else {
                    (x.1 <= p.tp, x.1 >= p.sl)
                };
                if hit_t {
                    y = true;
                    break;
                }
                if hit_s {
                    break;
                }
            }
            let k = self.by_key.entry((p.pair.clone(), p.long)).or_default();
            k.n += 1;
            k.hits += y as u32;
            k.sum_p += p.p;
            self.resolved.push_back((p.p, y));
            while self.resolved.len() > 5_000 {
                self.resolved.pop_front();
            }
        }
    }

    pub fn status(&self) -> Value {
        let p: Vec<f64> = self.resolved.iter().map(|x| x.0).collect();
        let y: Vec<bool> = self.resolved.iter().map(|x| x.1).collect();
        let n = p.len();
        let hit = if n > 0 {
            Some(y.iter().filter(|b| **b).count() as f64 / n as f64)
        } else {
            None
        };
        let brier = prob::brier(&p, &y);
        let clim = hit.map(|h| h * (1.0 - h));
        let ece = prob::ece(&p, &y, 10);
        let (ok, why) = self.verdict(n, brier, clim, ece);
        json!({
            "model": MODEL_ID, "target": "P(TP before SL within 120 s)",
            "n_resolved": n, "n_pending": self.pending.len(), "n_dropped_gaps": self.dropped,
            "brier": brier, "brier_climatology": clim, "log_loss": prob::log_loss(&p, &y),
            "ece_10bin": ece, "mean_p": stats::mean(&p), "hit_rate": hit,
            "calibrated": ok, "verdict": why,
            "criteria": format!("n >= {CALIB_MIN_N}, ECE <= {CALIB_MAX_ECE}, Brier < climatology"),
            "method": "forward chronological: each forecast resolved on the next 120 s of 1 s RT prices; samples overlap, so effective n is smaller than n",
        })
    }

    fn verdict(
        &self,
        n: usize,
        brier: Option<f64>,
        clim: Option<f64>,
        ece: Option<f64>,
    ) -> (bool, String) {
        if n < CALIB_MIN_N {
            return (
                false,
                format!("insufficient forward samples {n} < {CALIB_MIN_N}"),
            );
        }
        match (brier, clim, ece) {
            (Some(b), Some(c), Some(e)) if e <= CALIB_MAX_ECE && b < c => (
                true,
                format!("ECE {e:.4} <= {CALIB_MAX_ECE}, Brier {b:.4} < {c:.4}"),
            ),
            (Some(b), Some(c), Some(e)) => (
                false,
                format!("ECE {e:.4} (max {CALIB_MAX_ECE}) / Brier {b:.4} vs climatology {c:.4}"),
            ),
            _ => (false, "metrics not computable".into()),
        }
    }

    #[allow(dead_code)]
    pub fn calibrated(&self) -> bool {
        self.status()["calibrated"].as_bool().unwrap_or(false)
    }
}

pub struct Desk {
    pub settings: Settings,
    pub started_ms: i64,
    pub last_universe_ms: Option<i64>,
    pub last_rt_ms: Option<i64>,
    pub last_spot_ms: Option<i64>,
    pub rt_polls: u64,
    pub errors: u64,
    pub last_error: Option<String>,
    pub fut_usdt: HashSet<String>,
    pub fut_inr: HashSet<String>,
    pub spot_active: usize,
    pub spot_details: Vec<SpotMarketDetail>,
    pub spot: HashMap<String, (f64, f64, f64, f64)>,
    pub hist: HashMap<String, VecDeque<(i64, f64, f64)>>,
    #[allow(clippy::type_complexity)]
    pub rt_meta: HashMap<String, (Option<f64>, Option<f64>, Option<f64>, Option<f64>)>,
    pub books: HashMap<String, Book>,
    pub ew_spread: HashMap<String, (f64, u32)>,
    pub instruments: HashMap<String, (i64, FuturesInstrument)>,
    /// instrument key -> (failed at, error); retried after 10 min, never guessed
    pub meta_fail: HashMap<String, (i64, String)>,
    pub mtf: HashMap<String, (i64, Value)>,
    pub ctx: HashMap<String, BTreeMap<ContextTf, features::TfContext>>,
    pub regimes: HashMap<String, Vec<String>>,
    pub scanner: Vec<Value>,
    pub forecasts: HashMap<String, Value>,
    pub opportunities: VecDeque<Value>,
    pub decisions: VecDeque<Value>,
    pub events: VecDeque<Value>,
    pub audit: VecDeque<Value>,
    pub incidents: VecDeque<Value>,
    pub calib: Calib,
    pub controls: Controls,
    pub gov: RiskGovernor,
    pub cap: CapitalController,
    pub fill_model: qe_engine::paper::FillModel,
    pub latency: qe_engine::paper::LatencyStats,
    pub risk_events: VecDeque<String>,
    pub reports_done: HashSet<String>,
    pub learn: crate::learn::LearnCtx,
    pub positions: Vec<PaperPos>,
    pub closed: VecDeque<Value>,
    pub fills: VecDeque<Value>,
    pub equity_curve: Vec<(i64, f64)>,
    pub agents: BTreeMap<String, Agent>,
    pub store: Store,
    pub candle_cache: HashMap<String, (i64, Value)>,
    pub sim: Option<Value>,
    pub checkpoint_handled: bool,
    pub seq: u64,
    pub feed: String,
    /// latest WS currentPrices per pair: (exchange ts, last, vol24h, mark, funding, change%)
    pub ws_prices: HashMap<String, WsPrice>,
    pub ws_cp_ms: Option<i64>,
    /// local receive time of the last Live WS book per pair
    pub ws_book_ms: HashMap<String, i64>,
    /// WS trades per pair, last 300 s (event time), aggressor from the feed
    pub trades: HashMap<String, VecDeque<timeframe::Trade>>,
    pub ws_health: Value,
    /// s2-calib walk-forward registry: the source of forecast authority
    pub calib_report: qe_research::calibration::CalibrationReport,
    pub calib_report_src: String,
    pub calib_report_err: Option<String>,
    /// Gamble ranking board: latest estimate per (pair, side).
    pub gamble_board: HashMap<(String, bool), qe_risk::gamble::BetEstimate>,
    /// Realised gamble bets per (pair, long).
    pub gamble_trades: HashMap<(String, bool), qe_risk::gamble::HitCounts>,
    /// Latest target-probability Monte Carlo (refreshed by the monitor loop).
    pub gamble_sim: Option<Value>,
    pub maker_stats: MakerStats,
}

pub type WsPrice = (i64, f64, Option<f64>, Option<f64>, Option<f64>, Option<f64>);

pub const REGISTRY_MODEL: &str = qe_research::report::MODEL_DESK;

pub fn side_target(side: Side) -> &'static str {
    match side {
        Side::Long => "tp_first_long",
        Side::Short => "tp_first_short",
    }
}

impl Desk {
    /// (Re)load the s2-calib calibration registry. A missing or unreadable file
    /// leaves every model uncalibrated (no authority), never the reverse.
    pub fn load_calibration(&mut self) {
        let path = std::env::var("QE_CALIBRATION_FILE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../qe-research/calibration.json")
            });
        self.calib_report_src = path.display().to_string();
        match qe_research::calibration::CalibrationReport::load(&path) {
            Ok(r) => {
                self.calib_report = r;
                self.calib_report_err = None;
            }
            Err(e) => {
                self.calib_report = Default::default();
                self.calib_report_err = Some(e.to_string());
            }
        }
    }

    /// Forecast authority for one side: the walk-forward registry must grant it
    /// AND the live forward monitor must not contradict it (once it has >= 200
    /// resolved samples, a failing forward check revokes authority).
    pub fn authority(&self, side: Side) -> bool {
        let fwd = self.calib.status();
        let fwd_fail = fwd["n_resolved"].as_u64().unwrap_or(0) as usize >= CALIB_MIN_N
            && !fwd["calibrated"].as_bool().unwrap_or(false);
        self.calib_report
            .grants_authority(REGISTRY_MODEL, side_target(side))
            && !fwd_fail
    }

    /// Calibration view served to the dashboard: authority per side from the
    /// registry, plus the forward monitor's live metrics at the top level.
    pub fn calibration_json(&self) -> Value {
        let mut v = self.calib.status();
        v["forward_calibrated"] = v["calibrated"].clone();
        let (al, ash) = (self.authority(Side::Long), self.authority(Side::Short));
        v["calibrated"] = json!(al || ash);
        v["authority"] = json!({"long": al, "short": ash});
        let reg: serde_json::Map<String, Value> = [
            "tp_first_long",
            "tp_first_short",
            "sl_first_long",
            "sl_first_short",
        ]
        .iter()
        .map(|t| {
            let e = self.calib_report.entry(REGISTRY_MODEL, t);
            (
                t.to_string(),
                json!({"status": self.calib_report.status(REGISTRY_MODEL, t),
                    "n_oos": e.map(|x| x.n_oos), "reasons": e.map(|x| x.reasons.clone())}),
            )
        })
        .collect();
        v["registry"] = json!({"source": "qe-research walk-forward registry (s2-calib)", "model": REGISTRY_MODEL,
            "file": self.calib_report_src, "load_error": self.calib_report_err,
            "generated_ms": self.calib_report.generated_ms, "targets": reg});
        let reg_verdict = if al || ash {
            "registry grants authority"
        } else {
            "registry denies authority"
        };
        v["verdict"] = json!(format!(
            "{reg_verdict}; forward monitor: {}",
            v["verdict"].as_str().unwrap_or("")
        ));
        v
    }

    /// True when the WS currentPrices stream delivered within 3 s.
    pub fn ws_prices_fresh(&self) -> bool {
        self.ws_cp_ms.is_some_and(|t| now_ms() - t <= 3_000)
    }
    /// True when a Live WS book for `pair` arrived within `max_ms`.
    pub fn ws_book_fresh(&self, pair: &str, max_ms: i64) -> bool {
        self.ws_book_ms
            .get(pair)
            .is_some_and(|t| now_ms() - t <= max_ms)
    }
}

pub const AGENTS: [(&str, &str, &str); 10] = [
    (
        "universe_scanner",
        "Global CoinDCX universe scanner",
        "public REST read",
    ),
    (
        "opportunity_scanner",
        "High-frequency opportunity scanner",
        "public REST read",
    ),
    (
        "instrument_analyst",
        "Historical & multi-timeframe analyst",
        "public REST read",
    ),
    ("quant_brain", "Predictive quant brain", "compute only"),
    (
        "strategy_critic",
        "Independent mathematical critic (calibration)",
        "compute only",
    ),
    (
        "risk_governor",
        "Risk governor (final authority)",
        "approve/reject; cannot be overridden",
    ),
    (
        "execution_manager",
        "Execution manager (paper)",
        "paper fills only; live gate refused",
    ),
    (
        "capital_manager",
        "Capital & portfolio manager",
        "paper equity only",
    ),
    ("system_monitor", "System monitor", "read health"),
    (
        "metadata",
        "Instrument metadata (capability model)",
        "public REST read",
    ),
];

impl Settings {
    pub fn is_experiment(&self) -> bool {
        self.profile == EXPERIMENT_PROFILE
    }
    pub fn is_gamble(&self) -> bool {
        self.profile == GAMBLE_PROFILE && self.gamble.is_some()
    }
    /// The two-stage capital plan from these settings.
    pub fn plan(&self) -> CapitalPlan {
        CapitalPlan {
            stage0_end_ms: self.stage0_end_ms as u64,
            stage1_start_inr: dec(self.paper_start_equity_inr),
            stage1_target_inr: dec(self.stage1_target_inr),
            stage1_checkpoint_ms: self.stage1_checkpoint_ms as u64,
            withdrawal_inr: dec(self.withdrawal_inr),
            stage2_expected_start_inr: dec(0.0),
            stage2_target_inr: dec(self.stage2_target_inr),
            stage2_deadline_ms: self.stage2_deadline_ms as u64,
        }
    }
}

impl Desk {
    pub fn new(settings: Settings) -> Self {
        let now = now_ms();
        let s = &settings;
        let cap = CapitalController::new(s.plan(), dec(s.paper_start_equity_inr), true, now as u64);
        let policy = if s.is_experiment() && s.execution_mode == "paper" {
            RiskPolicy::experiment_max_leverage()
        } else if let (true, Some(g)) = (s.is_gamble(), s.gamble.clone()) {
            RiskPolicy::gamble(g)
        } else {
            s.growth.clone()
        };
        let gov = RiskGovernor::new(policy, s.paper_start_equity_inr, s.stage1_target_inr);
        let agents = AGENTS
            .iter()
            .map(|(id, name, perm)| {
                (
                    id.to_string(),
                    Agent {
                        id: id.to_string(),
                        name: name.to_string(),
                        status: "stopped".into(),
                        permissions: perm.to_string(),
                        ..Default::default()
                    },
                )
            })
            .collect();
        let start = s.paper_start_equity_inr;
        let mut desk = Self {
            settings,
            started_ms: now,
            last_universe_ms: None,
            last_rt_ms: None,
            last_spot_ms: None,
            rt_polls: 0,
            errors: 0,
            last_error: None,
            fut_usdt: HashSet::new(),
            fut_inr: HashSet::new(),
            spot_active: 0,
            spot_details: vec![],
            spot: HashMap::new(),
            hist: HashMap::new(),
            rt_meta: HashMap::new(),
            books: HashMap::new(),
            ew_spread: HashMap::new(),
            instruments: HashMap::new(),
            meta_fail: HashMap::new(),
            mtf: HashMap::new(),
            ctx: HashMap::new(),
            regimes: HashMap::new(),
            scanner: vec![],
            forecasts: HashMap::new(),
            opportunities: VecDeque::new(),
            decisions: VecDeque::new(),
            events: VecDeque::new(),
            audit: VecDeque::new(),
            incidents: VecDeque::new(),
            calib: Calib::default(),
            controls: Controls {
                scanner_running: true,
                paper_running: true,
                strategy_enabled: true,
                entries_paused: false,
                killed: false,
            },
            gov,
            cap,
            fill_model: qe_engine::paper::FillModel::new(Default::default()),
            latency: Default::default(),
            risk_events: VecDeque::new(),
            reports_done: HashSet::new(),
            learn: crate::learn::LearnCtx::default(),
            positions: vec![],
            closed: VecDeque::new(),
            fills: VecDeque::new(),
            equity_curve: vec![(now, start)],
            agents,
            store: Store::open(),
            candle_cache: HashMap::new(),
            sim: None,
            checkpoint_handled: false,
            seq: 0,
            feed: "starting".into(),
            ws_prices: HashMap::new(),
            ws_cp_ms: None,
            ws_book_ms: HashMap::new(),
            trades: HashMap::new(),
            ws_health: Value::Null,
            calib_report: Default::default(),
            calib_report_src: String::new(),
            calib_report_err: None,
            gamble_board: HashMap::new(),
            gamble_trades: HashMap::new(),
            gamble_sim: None,
            maker_stats: MakerStats::default(),
        };
        desk.learn = crate::learn::LearnCtx::open(&desk.store.dir);
        if let Some(e) = desk.learn.error.clone() {
            desk.incident("learning", e);
        }
        desk
    }

    pub fn audit(&mut self, kind: &str, msg: String, data: Value) {
        eprintln!("[{kind}] {msg}");
        let rec = json!({"ts_ms": now_ms(), "kind": kind, "message": msg, "data": data});
        self.store.append("audit", &rec);
        self.audit.push_front(rec);
        self.audit.truncate(200);
    }

    pub fn incident(&mut self, kind: &str, detail: String) {
        let rec = json!({"ts_ms": now_ms(), "kind": kind, "detail": detail});
        self.store.append("incidents", &rec);
        self.incidents.push_front(rec);
        self.incidents.truncate(200);
        self.audit("incident", format!("{kind}: {detail}"), Value::Null);
    }

    pub fn err(&mut self, e: String) {
        self.errors += 1;
        self.audit("error", e.clone(), Value::Null);
        self.last_error = Some(e);
    }

    pub fn agent_ok(&mut self, id: &str, started: i64, output: String) {
        let now = now_ms();
        if let Some(a) = self.agents.get_mut(id) {
            a.runs += 1;
            a.status = "ok".into();
            a.last_run_ms = Some(now);
            a.last_ok_ms = Some(now);
            a.duration_ms = Some(now - started);
            a.last_output = Some(output);
        }
    }

    pub fn agent_err(&mut self, id: &str, started: i64, e: String) {
        let now = now_ms();
        if let Some(a) = self.agents.get_mut(id) {
            a.runs += 1;
            a.errors += 1;
            a.status = "degraded".into();
            a.last_run_ms = Some(now);
            a.duration_ms = Some(now - started);
            a.last_error = Some(e);
        }
    }

    pub fn agent_stopped(&mut self, id: &str) {
        if let Some(a) = self.agents.get_mut(id) {
            a.status = "stopped".into();
        }
    }

    /// USDT -> INR from the CoinDCX spot USDTINR ticker (mid). This approximates
    /// the exchange's own INR-margin conversion rate, which is not published in
    /// the public instrument endpoint.
    pub fn fx_usdtinr(&self) -> Option<f64> {
        self.spot
            .get("USDTINR")
            .map(|s| (s.1 + s.2) / 2.0)
            .filter(|x| *x > 0.0)
    }

    pub fn rt_fresh(&self) -> bool {
        self.last_rt_ms.is_some_and(|t| now_ms() - t <= 5_000)
    }

    pub fn equity(&self) -> f64 {
        self.cap.equity_f64()
    }

    pub fn unrealized(&self) -> f64 {
        self.positions
            .iter()
            .filter(|p| p.pos.filled_qty > 0.0)
            .map(|p| {
                let s = if p.pos.order.side == Side::Long {
                    1.0
                } else {
                    -1.0
                };
                (p.mark - p.pos.avg_entry) * (p.pos.filled_qty - p.pos.exited_qty) * s * p.fx
            })
            .sum()
    }

    pub fn used_margin(&self) -> f64 {
        self.positions
            .iter()
            .map(|p| p.pos.order.margin_account)
            .sum()
    }
}

/// Map live CoinDCX instrument metadata into the qe-risk capability model.
/// Nothing is hardcoded per instrument. The maintenance-margin rate is taken
/// conservatively as max(safety_percentage, smallest dynamic safety tier) / 100;
/// the exact liquidation formula is not documented publicly (unverified).
pub fn capability(i: &FuturesInstrument) -> InstrumentCapability {
    let tiers = i
        .leverage_tiers()
        .into_iter()
        .map(|(l, n)| LeverageTier {
            leverage: l,
            max_notional: n,
        })
        .collect();
    let smallest = i
        .dynamic_safety_margin_details
        .iter()
        .filter_map(|(k, v)| k.parse::<f64>().ok().map(|n| (n, *v)))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|x| x.1);
    let mmr = match (i.safety_percentage, smallest) {
        (Some(a), Some(b)) => Some(a.max(b) / 100.0),
        (Some(a), None) => Some(a / 100.0),
        (None, Some(b)) => Some(b / 100.0),
        _ => None,
    };
    let mut maxq = if i.max_quantity > 0.0 {
        i.max_quantity
    } else {
        f64::INFINITY
    };
    if let Some(m) = i.max_market_order_quantity.filter(|m| *m > 0.0) {
        maxq = maxq.min(m);
    }
    InstrumentCapability {
        pair: i.pair.clone(),
        margin_currency: i.margin_currency_short_name.clone(),
        active: i.status == "active",
        exit_only: i.exit_only,
        max_leverage_long: i.max_leverage_long,
        max_leverage_short: i.max_leverage_short,
        tiers,
        min_notional: i.min_notional,
        min_quantity: i.min_quantity,
        max_quantity: maxq,
        quantity_increment: i.quantity_increment,
        price_increment: i.price_increment,
        taker_fee: i.taker_fee_frac(),
        maker_fee: i.maker_fee / 100.0,
        maintenance_margin_rate: mmr,
        // Margin-mode support is not exposed by the public instrument endpoint.
        margin_modes: vec![],
        supports_market_order: i.order_types.iter().any(|o| o == "market_order"),
    }
}

pub fn ret_over(h: &VecDeque<(i64, f64, f64)>, w_ms: i64) -> Option<f64> {
    let (t, p, _) = *h.back()?;
    let past = h.iter().rev().find(|x| x.0 <= t - w_ms)?;
    Some((p / past.1).ln())
}

/// Per-second volatility from 1-step log returns of the 1 s RT samples:
/// sigma_1s = RV_step / sqrt(mean dt).
/// Median high-low range / mean price over consecutive 120 s buckets of the
/// price history (newest first; buckets with fewer than 20 prints are skipped).
pub fn range_120s_frac(h: &VecDeque<(i64, f64, f64)>) -> Option<f64> {
    let end = h.back()?.0;
    let mut v = vec![];
    for k in 0..5i64 {
        let (lo_t, hi_t) = (end - (k + 1) * 120_000, end - k * 120_000);
        let p: Vec<f64> = h
            .iter()
            .filter(|x| x.0 > lo_t && x.0 <= hi_t && x.1 > 0.0)
            .map(|x| x.1)
            .collect();
        if p.len() < 20 {
            continue;
        }
        let (lo, hi) = p
            .iter()
            .fold((f64::INFINITY, 0.0f64), |a, x| (a.0.min(*x), a.1.max(*x)));
        let mean = p.iter().sum::<f64>() / p.len() as f64;
        v.push((hi - lo) / mean);
    }
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(v[v.len() / 2])
}

pub fn sigma_per_s(h: &VecDeque<(i64, f64, f64)>) -> Option<f64> {
    if h.len() < 30 {
        return None;
    }
    let p: Vec<f64> = h.iter().map(|x| x.1).collect();
    let r = stats::log_returns(&p);
    let dt = (h.back()?.0 - h.front()?.0) as f64 / 1000.0 / (h.len() - 1) as f64;
    let rv = stats::realized_vol(&r)?;
    if rv <= 0.0 || dt <= 0.0 {
        return None;
    }
    Some(rv / dt.sqrt())
}

pub fn to_mcandles(c: &[qe_coindcx::types::Candle], width_ms: i64, now: i64) -> Vec<MCandle> {
    let mut v: Vec<MCandle> = c
        .iter()
        .map(|x| MCandle {
            open_ms: x.time,
            width_ms,
            open: x.open,
            high: x.high,
            low: x.low,
            close: x.close,
            volume: x.volume,
            buy_volume: 0.0,
            sell_volume: 0.0,
            trades: 0,
            complete: false,
        })
        .collect();
    v.sort_by_key(|c| c.open_ms);
    v.dedup_by_key(|c| c.open_ms);
    timeframe::mark_completeness(&mut v, now);
    v
}

/// Explicit numerical regime rules over multi-timeframe context and the live
/// scanner row. Labels (any subset): trend_up / trend_down / range_bound,
/// high_volatility / low_volatility, volatility_expansion / volatility_contraction,
/// momentum_acceleration, momentum_exhaustion, potential_reversal,
/// abnormal_volume, liquidity_withdrawal, unstable.
pub fn regimes(
    ctx: Option<&BTreeMap<ContextTf, features::TfContext>>,
    row: &Value,
    spread_ratio: Option<f64>,
    data_ok: bool,
) -> Vec<String> {
    let mut r: Vec<String> = vec![];
    let f = |k: &str| row[k].as_f64();
    let sig = f("vol_1s");
    if let Some(c) = ctx {
        if let Some(h1) = c.get(&ContextTf::H1) {
            r.push(
                if h1.trend_z > 2.0 {
                    "trend_up"
                } else if h1.trend_z < -2.0 {
                    "trend_down"
                } else {
                    "range_bound"
                }
                .into(),
            );
        }
        if let Some(d1) = c.get(&ContextTf::D1) {
            if d1.range_ratio > 1.5 {
                r.push("high_volatility".into());
            } else if d1.range_ratio < 0.5 {
                r.push("low_volatility".into());
            }
        }
        if let (Some(m1), Some(s)) = (c.get(&ContextTf::M1), sig) {
            // micro sigma (per s) vs 1m-candle sigma scaled to per s
            let ctx_s = m1.sigma / 60f64.sqrt();
            if ctx_s > 0.0 {
                let q = s / ctx_s;
                if q > 1.5 {
                    r.push("volatility_expansion".into());
                } else if q < 0.67 {
                    r.push("volatility_contraction".into());
                }
            }
            if m1.volume_z > 3.0 {
                r.push("abnormal_volume".into());
            }
        }
        if let (Some(m15), Some(z10)) = (c.get(&ContextTf::M15), f("z_ret_10s")) {
            if z10.abs() > 2.0 && m15.trend_z.abs() > 1.0 && z10.signum() != m15.trend_z.signum() {
                r.push("potential_reversal".into());
            }
        }
    }
    if let (Some(z10), Some(r60)) = (f("z_ret_10s"), f("ret_60s")) {
        if z10.abs() > 2.0 && z10.signum() == r60.signum() {
            r.push("momentum_acceleration".into());
        }
    }
    if let (Some(r300), Some(s), Some(z10)) = (f("ret_300s"), sig, f("z_ret_10s")) {
        if s > 0.0 && (r300 / (s * 300f64.sqrt())).abs() > 2.0 && z10.abs() < 0.5 {
            r.push("momentum_exhaustion".into());
        }
    }
    if f("z_volume").is_some_and(|z| z > 3.0) && !r.iter().any(|x| x == "abnormal_volume") {
        r.push("abnormal_volume".into());
    }
    if spread_ratio.is_some_and(|q| q > 3.0) {
        r.push("liquidity_withdrawal".into());
    }
    if !data_ok {
        r.push("unstable".into());
    }
    r
}

pub fn scan(d: &mut Desk) {
    let mut rows = vec![];
    let now = now_ms();
    for (pair, h) in &d.hist {
        let Some(&(_, last, _)) = h.back() else {
            continue;
        };
        let sig = sigma_per_s(h);
        let r10 = ret_over(h, 10_000);
        let z10 = match (r10, sig) {
            (Some(r), Some(s)) if s > 0.0 => Some(r / (s * 10f64.sqrt())),
            _ => None,
        };
        let v: Vec<f64> = h.iter().map(|x| x.2).collect();
        let dv: Vec<f64> = v.windows(2).map(|w| (w[1] - w[0]).max(0.0)).collect();
        let zv = dv.last().and_then(|x| stats::zscore(*x, &dv));
        let (mark, fr, pc, vol) = d.rt_meta.get(pair).cloned().unwrap_or_default();
        let base = pair.trim_start_matches("B-").replace('_', "");
        let basis = d.spot.get(&base).map(|s| (last - s.0) / s.0 * 1e4);
        let book = d
            .books
            .get(pair)
            .filter(|b| b.is_sane() && now - b.ts < 10_000);
        let (obi, spread) = match book {
            Some(b) => (
                micro::obi(&b.bids, &b.asks, 10),
                Some(micro::spread_bps(
                    b.best_bid().unwrap(),
                    b.best_ask().unwrap(),
                )),
            ),
            None => (None, None),
        };
        let mut reasons = vec![];
        if z10.is_some_and(|z| z.abs() > 3.0) {
            reasons.push("price_jump_z>3");
        }
        if zv.is_some_and(|z| z > 3.0) {
            reasons.push("volume_spike_z>3");
        }
        if spread.is_some_and(|s| s > 30.0) {
            reasons.push("wide_spread>30bps");
        }
        let liq = (1.0 + vol.unwrap_or(0.0) * last).ln();
        let score =
            z10.map_or(0.0, f64::abs) + 0.1 * liq + if reasons.is_empty() { 0.0 } else { 1.0 };
        let vol_1m = d
            .ctx
            .get(pair)
            .and_then(|c| c.get(&ContextTf::M1))
            .map(|c| c.sigma);
        let regime = d.regimes.get(pair).map(|r| r.join(","));
        // Known-ineligible INR contracts (e.g. null exchange max leverage) rank last so
        // the order-book / trade shortlist is spent on instruments that can trade.
        let tradable = d
            .instruments
            .get(&format!("{pair}|{TRADE_MARGIN}"))
            .map(|(_, i)| i.is_tradable());
        rows.push(json!({
            "tradable": tradable,
            "pair": pair, "margin": if d.fut_inr.contains(pair) { "INR" } else { "USDT" },
            "last": last, "mark": mark, "funding_rate": fr, "change_24h_pct": pc, "volume_24h": vol,
            "ret_10s": r10, "ret_30s": ret_over(h, 30_000), "ret_60s": ret_over(h, 60_000), "ret_300s": ret_over(h, 300_000),
            "vol_1s": sig, "vol_1m_candle": vol_1m, "z_ret_10s": z10, "z_volume": zv, "basis_bps": basis,
            "obi": obi, "spread_bps": spread, "anomaly": !reasons.is_empty(), "anomaly_reasons": reasons,
            "regime": regime, "score": score,
        }));
    }
    rows.sort_by(|a, b| {
        let dead = |r: &Value| r["tradable"] == json!(false);
        dead(a).cmp(&dead(b)).then_with(|| {
            b["score"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&a["score"].as_f64().unwrap_or(0.0))
        })
    });
    for (i, r) in rows.iter_mut().enumerate() {
        r["rank"] = json!(i + 1);
    }
    d.scanner = rows;
}

fn round_to(x: f64, inc: f64) -> f64 {
    if inc > 0.0 {
        (x / inc).round() * inc
    } else {
        x
    }
}

/// Forecast, opportunity evaluation, strategy proposal and risk-governor decision
/// for one candidate with a fresh, sane book. Every number used is recorded in
/// the decision's `explanation`.
pub fn evaluate(d: &mut Desk, pair: &str) {
    let now = now_ms();
    let Some(h) = d.hist.get(pair) else { return };
    let Some(sig) = sigma_per_s(h) else { return };
    let m60 = ret_over(h, 60_000).unwrap_or(0.0) / 60.0;
    let Some(b) = d.books.get(pair).filter(|b| b.is_sane()) else {
        return;
    };
    let book_age = now - b.ts;
    let (bid, ask) = (b.best_bid().unwrap(), b.best_ask().unwrap());
    let (bid_lv, ask_lv) = (b.bids.clone(), b.asks.clone());
    let mid = micro::mid(bid, ask);
    let obi = micro::obi(&b.bids, &b.asks, 10).unwrap_or(0.0);
    let spread_bps = micro::spread_bps(bid, ask);
    // EW reference spread for liquidity-withdrawal detection
    let e = d
        .ew_spread
        .entry(pair.to_string())
        .or_insert((spread_bps, 0));
    let spread_ratio = if e.1 >= 10 && e.0 > 0.0 {
        Some(spread_bps / e.0)
    } else {
        None
    };
    e.0 = 0.9 * e.0 + 0.1 * spread_bps;
    e.1 += 1;
    let data_ok = book_age < 5_000 && d.rt_fresh();
    let tflow: Vec<Value> = d
        .trades
        .get(pair)
        .map(|t| {
            let v: Vec<timeframe::Trade> = t.iter().copied().collect();
            [10_000i64, 30_000, 60_000]
                .iter()
                .map(|w| {
                    let ws = timeframe::window_stats(&v, now, *w);
                    json!({"window_s": w / 1000, "trades": ws.trades, "volume": ws.volume,
                        "tfi": features::trade_flow_imbalance(&ws)})
                })
                .collect()
        })
        .unwrap_or_default();
    let row = d
        .scanner
        .iter()
        .find(|r| r["pair"] == pair)
        .cloned()
        .unwrap_or(Value::Null);
    let reg = regimes(d.ctx.get(pair), &row, spread_ratio, data_ok);
    d.regimes.insert(pair.to_string(), reg.clone());

    let mu = LAMBDA_MOM * m60 + GAMMA_OBI * obi * sig;
    let s120 = sig * HORIZON_S.sqrt();
    let (tp, sl) = (K_TARGET * s120, K_STOP * s120);
    let auth_long = d.authority(Side::Long);
    let auth_short = d.authority(Side::Short);
    let calibrated = auth_long || auth_short;
    let horizons: Vec<Value> = HORIZONS
        .iter()
        .map(|&hs| {
            let sh = sig * hs.sqrt();
            let er = mu * hs;
            let pu = prob::norm_cdf(er / sh);
            json!({"h_s": hs, "exp_ret": er, "p_up": pu, "p_down": 1.0 - pu, "sigma_h": sh,
                   "q05": er - 1.6449 * sh, "q95": er + 1.6449 * sh})
        })
        .collect();

    // live per-instrument metadata for the traded (INR-margined) contract
    let meta = d
        .instruments
        .get(&format!("{pair}|{TRADE_MARGIN}"))
        .map(|x| x.1.clone());
    let cap = meta.as_ref().map(capability);
    let fee = cap.as_ref().map(|c| c.taker_fee);
    let slip = d.settings.paper_slippage_bps / 1e4;
    let fee_used = fee.unwrap_or(f64::NAN);
    let maker_entry = d.settings.gamble_maker_entry && d.gov.policy.is_gamble();
    let cost = if maker_entry {
        // Maker entry rests at the near touch and the taker exit leaves from
        // the same side: maker fee + taker fee + exit slippage, no spread.
        cap.as_ref().map_or(f64::NAN, |c| c.maker_fee) + fee_used + slip
    } else {
        // C = 2 * taker fee + spread + 2 * slippage (round trip, charged once)
        2.0 * fee_used + (ask - bid) / mid + 2.0 * slip
    };

    let mut sides = vec![];
    for (side, m) in [(Side::Long, mu), (Side::Short, -mu)] {
        let fp = prob::first_passage(m, sig, tp, sl, HORIZON_S, 0.5);
        let gross = prob::bracket_ev(&fp, tp, sl, 0.0);
        sides.push((side, fp, gross, gross - cost));
    }
    let fp_long = sides[0].1;
    d.calib.maybe_record(
        pair,
        now,
        [
            (true, mid * tp.exp(), mid * (-sl).exp(), sides[0].1.p_target),
            (
                false,
                mid * (-tp).exp(),
                mid * sl.exp(),
                sides[1].1.p_target,
            ),
        ],
    );
    d.forecasts.insert(
        pair.to_string(),
        json!({"pair": pair, "ts_ms": now, "mu_per_s": mu, "sigma_per_s": sig, "horizons": horizons,
            "first_passage": {"p_target": fp_long.p_target, "p_stop": fp_long.p_stop, "p_neither": fp_long.p_neither,
                "e_t_target": fp_long.e_t_target, "e_t_stop": fp_long.e_t_stop, "side": "long",
                "tp_ret": tp, "sl_ret": sl},
            "short_first_passage": {"p_target": sides[1].1.p_target, "p_stop": sides[1].1.p_stop, "p_neither": sides[1].1.p_neither},
            "model": MODEL_ID, "calibrated": calibrated, "regime": reg,
            "label": "estimate, not a fact"}),
    );

    let gamble = d.gov.policy.is_gamble();
    let mut gamble_skip: Option<String> = None;
    let mut gamble_est: Option<qe_risk::gamble::BetEstimate> = None;
    let mut payoff_plan: Option<qe_risk::gamble::payoff::PayoffPlan> = None;
    let mut best = if sides[0].3 >= sides[1].3 { 0 } else { 1 };
    if gamble {
        // Rank by the best available estimate of P(win) and payoff after costs.
        let g = global_counts(&d.calib);
        let ests: Vec<qe_risk::gamble::BetEstimate> = sides
            .iter()
            .map(|(s, f, _, _)| {
                let long = *s == Side::Long;
                let key = (pair.to_string(), long);
                let pw = qe_risk::gamble::win_prob(
                    f.p_target,
                    g,
                    d.calib.by_key.get(&key).copied().unwrap_or_default(),
                    d.gamble_trades.get(&key).copied().unwrap_or_default(),
                );
                qe_risk::gamble::BetEstimate::new(
                    pair,
                    long,
                    now,
                    f.p_target,
                    pw,
                    f.p_neither,
                    f.e_ret_neither,
                    tp,
                    sl,
                    cost,
                )
            })
            .collect();
        best = if ests[0].score >= ests[1].score { 0 } else { 1 };
        let mut me = ests[best].clone();
        if let Some(pc) = d.settings.payoff {
            // Payoff per bet: volatility-scaled TP/SL, maker/taker cost with
            // spread and depth impact, exclusion when cost eats the TP.
            let fx_v = d.fx_usdtinr().unwrap_or(f64::NAN);
            let range = d.hist.get(pair).and_then(range_120s_frac).unwrap_or(0.0);
            let owner_lev = d.gov.policy.gamble.as_ref().map_or(1, |g| g.leverage);
            let room = d.equity() - d.gov.policy.gamble.as_ref().map_or(0.0, |g| g.floor());
            let notional = (room.max(0.0) * owner_lev as f64).max(1.0);
            let quotes: Vec<_> = [true, false]
                .iter()
                .map(|&long| {
                    let lv = |x: f64| {
                        if x.is_finite() && x >= 1.0 {
                            x as u32
                        } else {
                            0
                        }
                    };
                    let (lvls, mlev) = if long {
                        (&bid_lv, cap.as_ref().map_or(0, |c| lv(c.max_leverage_long)))
                    } else {
                        (
                            &ask_lv,
                            cap.as_ref().map_or(0, |c| lv(c.max_leverage_short)),
                        )
                    };
                    let depth = lvls.iter().take(5).map(|(p, q)| p * q).sum::<f64>() * fx_v;
                    qe_risk::gamble::payoff::InstrumentQuote {
                        pair: pair.to_string(),
                        long,
                        maker_fee: cap.as_ref().map_or(f64::NAN, |c| c.maker_fee),
                        taker_fee: fee_used,
                        spread_frac: (ask - bid) / mid,
                        depth_inr: if depth.is_finite() { depth } else { 0.0 },
                        max_leverage: mlev,
                        maint_margin_frac: cap
                            .as_ref()
                            .and_then(|c| c.maintenance_margin_rate)
                            .unwrap_or(0.0),
                        range_120s_frac: range,
                        sigma_120s_frac: s120,
                        measured: d
                            .gamble_trades
                            .get(&(pair.to_string(), long))
                            .map(|h| (h.n, h.hits)),
                    }
                })
                .collect();
            let plans: Vec<_> = quotes
                .iter()
                .map(|q| qe_risk::gamble::payoff::plan(q, &pc, owner_lev, notional))
                .collect();
            let pick = match (&plans[0], &plans[1]) {
                (Ok(a), Ok(b)) if (a.ev_per_margin - b.ev_per_margin).abs() > 1e-15 => {
                    if a.ev_per_margin > b.ev_per_margin {
                        0
                    } else {
                        1
                    }
                }
                (Ok(_), Ok(_)) => best,
                (Ok(_), Err(_)) => 0,
                (Err(_), Ok(_)) => 1,
                (Err(e), Err(_)) => {
                    gamble_skip = Some(format!("gamble: skipped, {e}"));
                    best
                }
            };
            best = pick;
            if let Ok(pl) = &plans[pick] {
                me = qe_risk::gamble::BetEstimate::new(
                    pair,
                    pl.long,
                    now,
                    pl.win_p,
                    pl.win_p,
                    0.0,
                    0.0,
                    pl.tp_frac,
                    pl.sl_frac,
                    pl.cost_frac,
                );
                payoff_plan = Some(pl.clone());
            }
        }
        d.gamble_board
            .retain(|_, b| now - b.ts_ms <= GAMBLE_BOARD_MS && b.pair != pair);
        let board: Vec<_> = d.gamble_board.values().cloned().collect();
        if gamble_skip.is_none() {
            gamble_skip = qe_risk::gamble::skip_reason(&me, &board);
        }
        // Only instruments the account can actually trade compete for the bet;
        // an ineligible one (e.g. null exchange leverage) must not block others.
        let tradable = cap.as_ref().is_some_and(|c| {
            c.active
                && !c.exit_only
                && c.max_leverage_long >= 1.0
                && c.max_leverage_short >= 1.0
                && c.supports_market_order
        }) && (!maker_entry
            || meta
                .as_ref()
                .is_some_and(|m| m.order_types.iter().any(|o| o == "limit_order")));
        if tradable && (d.settings.payoff.is_none() || payoff_plan.is_some()) {
            d.gamble_board
                .insert((pair.to_string(), me.long), me.clone());
        }
        gamble_est = Some(me);
    }
    let (side, fp, _gross, mut net) = sides[best];
    // With [payoff], the bet uses the plan's barriers and cost.
    let (tp, sl, cost) = payoff_plan
        .as_ref()
        .map_or((tp, sl, cost), |p| (p.tp_frac, p.sl_frac, p.cost_frac));
    if let Some(e) = &gamble_est {
        net = e.ev_frac;
    }
    let side_auth = !gamble
        && if side == Side::Long {
            auth_long
        } else {
            auth_short
        };
    // With authority, the registry's recalibrated P(TP first) replaces the raw
    // lattice probability in the EV (P(neither) kept; P(SL) = 1 - P(TP) - P(neither)).
    let p_recal = d
        .calib_report
        .recalibrate(REGISTRY_MODEL, side_target(side), fp.p_target);
    if side_auth && !gamble {
        if let Some(pt) = p_recal {
            let ps = (1.0 - pt - fp.p_neither).max(0.0);
            net = pt * tp - ps * sl + fp.p_neither * fp.e_ret_neither - cost;
        }
    }
    let entry = match (side == Side::Long, maker_entry) {
        (true, false) | (false, true) => ask,
        (true, true) | (false, false) => bid,
    };
    let dir = if side == Side::Long { 1.0 } else { -1.0 };
    let inc = cap.as_ref().map_or(0.0, |c| c.price_increment);
    let tp_price = round_to(entry * (dir * tp).exp(), inc);
    let sl_price = round_to(entry * (-dir * sl).exp(), inc);
    let fx = d.fx_usdtinr();
    d.seq += 1;
    let trace = format!("qe-{now}-{}", d.seq);

    let explanation = json!({
        "model_id": MODEL_ID,
        "formulas": {
            "mu": "LAMBDA_MOM * r_60/60 + GAMMA_OBI * OBI_10 * sigma_1s",
            "barriers": "TP = K_TARGET * sigma_1s * sqrt(120), SL = K_STOP * sigma_1s * sqrt(120)",
            "first_passage": "binomial lattice dt=0.5 s, horizon 120 s",
            "ev": "EV = P(TP) W - P(SL) L + P(neither) E[R|neither] - C",
            "cost": if maker_entry { "C = maker_fee + taker_fee + paper_slippage (maker entry at the near touch)" }
                else { "C = 2 * taker_fee(live instrument) + (ask-bid)/mid + 2 * paper_slippage" },
        },
        "inputs": {"bid": bid, "ask": ask, "mid": mid, "obi_10": obi, "spread_bps": spread_bps, "r_60": m60 * 60.0,
            "sigma_1s": sig, "book_age_ms": book_age, "lambda_mom": LAMBDA_MOM, "gamma_obi": GAMMA_OBI,
            "k_stop": K_STOP, "k_target": K_TARGET, "taker_fee": fee, "paper_slippage": slip, "fx_usdtinr": fx},
        "intermediates": {"mu_per_s": mu, "sigma_120": s120, "tp_ret": tp, "sl_ret": sl, "cost": cost},
        "sides": sides.iter().map(|(s, f, g, n)| json!({"side": s, "p_target": f.p_target, "p_stop": f.p_stop,
            "p_neither": f.p_neither, "e_t_target": f.e_t_target, "e_t_stop": f.e_t_stop,
            "e_ret_neither": f.e_ret_neither, "ev_gross": g, "ev_net": n})).collect::<Vec<_>>(),
        "regime": reg, "data_ok": data_ok, "calibrated": calibrated,
        "authority": {"long": auth_long, "short": auth_short, "source": "qe-research registry + forward monitor",
            "registry_model": REGISTRY_MODEL, "p_target_recalibrated": p_recal, "ev_net_used": net},
        "trade_flow_ws": tflow,
        "book_source": if d.ws_book_fresh(pair, 5_000) { "ws (sequence-checked Live book)" } else { "rest depth 20" },
    });

    // ---------------- strategy + risk governor
    let mut decision = "rejected";
    let mut reasons: Vec<String> = vec![];
    let mut approved: Option<qe_risk::ApprovedOrder> = None;
    let gated_regime = reg
        .iter()
        .any(|r| r == "unstable" || r == "liquidity_withdrawal");
    if !d.controls.scanner_running {
        reasons.push("scanner stopped".into());
    } else if !d.controls.strategy_enabled {
        reasons.push("strategy evaluation disabled".into());
    } else if !d.controls.paper_running {
        reasons.push("paper trading stopped".into());
    } else if cap.is_none() && !d.fut_inr.is_empty() && !d.fut_inr.contains(pair) {
        reasons.push(format!("no {TRADE_MARGIN}-margined contract for this pair"));
    } else if let (None, Some((_, why))) =
        (&cap, d.meta_fail.get(&format!("{pair}|{TRADE_MARGIN}")))
    {
        // e.g. the exchange publishes null max leverage: no permitted leverage
        reasons.push(format!(
            "{TRADE_MARGIN} instrument metadata rejected (not tradable): {why}"
        ));
    } else if cap.is_none() {
        reasons.push(format!(
            "{TRADE_MARGIN} instrument metadata not fetched yet"
        ));
    } else if fx.is_none() {
        reasons.push("USDTINR fx unavailable".into());
    } else if let Some(why) = gamble_skip.clone() {
        reasons.push(why);
    } else if !d.cap.entries_allowed() {
        reasons.push(format!(
            "capital stage {:?}: no new entries (checkpoint workflow / complete)",
            d.cap.stage
        ));
    } else {
        let c = cap.clone().unwrap();
        let proposal = TradeProposal {
            trace_id: trace.clone(),
            pair: pair.to_string(),
            side,
            entry_price: entry,
            stop_loss: sl_price,
            take_profit: tp_price,
            expected_net_edge: net,
            p_tp_first: match &gamble_est {
                Some(e) => e.p_win,
                None if side_auth => p_recal.unwrap_or(fp.p_target),
                None => fp.p_target,
            },
            calibrated: side_auth,
            data_healthy: data_ok && !gated_regime,
            cost_per_unit: cost * entry,
            fx_to_account: fx.unwrap(),
        };
        let eq = d.equity().min(d.equity() + d.unrealized());
        let acct = AccountSnapshot {
            equity: eq,
            available_margin: (eq - d.used_margin()).max(0.0),
            paper: true,
        };
        match d.gov.evaluate_proposal(&proposal, &c, &acct, now as u64) {
            Decision::Approved(o) => {
                decision = "taken";
                reasons.push(format!(
                    "approved: qty {} lev {}x notional INR {:.2} risk INR {:.4}",
                    o.qty, o.leverage, o.notional_account, o.risk_at_stop
                ));
                approved = Some(o);
            }
            Decision::Rejected { reasons: r, .. } => {
                // a bet the account cannot size must not outrank affordable ones
                if gamble && r.iter().any(|x| x.starts_with("gamble:")) {
                    d.gamble_board
                        .remove(&(pair.to_string(), side == Side::Long));
                }
                reasons = r
            }
        }
        for a in std::mem::take(&mut d.gov.audit) {
            d.store
                .append("governor", &serde_json::to_value(&a).unwrap_or_default());
        }
    }
    let reason = reasons.join("; ");
    let (qty, notional, lev) = approved
        .as_ref()
        .map_or((0.0, 0.0, 0.0), |o| (o.qty, o.notional_account, o.leverage));
    let p_up = prob::norm_cdf(mu * HORIZON_S / s120);
    for (i, (s, f, g, n)) in sides.iter().enumerate() {
        let sgn = if *s == Side::Long { 1.0 } else { -1.0 };
        let is_best = i == best;
        let opp = json!({"pair": pair, "side": if *s == Side::Long {"long"} else {"short"},
            "p_win": f.p_target, "p_up_horizon": if sgn > 0.0 { p_up } else { 1.0 - p_up }, "p_stop": f.p_stop,
            "p_neither": f.p_neither, "tp_pct": tp * 100.0, "sl_pct": sl * 100.0, "ev_gross_bps": g * 1e4,
            "cost_bps": cost * 1e4, "ev_net_bps": n * 1e4, "horizon_s": HORIZON_S,
            "qty": if is_best { qty } else { 0.0 }, "notional": if is_best { notional } else { 0.0 },
            "leverage": if is_best { lev } else { 0.0 },
            "decision": if is_best { decision } else { "rejected" },
            "reason": if is_best { reason.clone() } else { "lower net EV than the opposite side".to_string() },
            "calibrated": if *s == Side::Long { auth_long } else { auth_short }, "ts_ms": now, "trace_id": trace});
        d.opportunities.push_front(opp);
    }
    d.opportunities.truncate(100);
    let mode = if gamble { "gamble" } else { "standard" };
    let rec = json!({"ts_ms": now, "trace_id": trace, "pair": pair, "side": side, "decision": decision, "mode": mode,
        "gamble_estimate": gamble_est, "payoff_plan": payoff_plan,
        "reason": reason, "entry": entry, "tp_price": tp_price, "sl_price": sl_price, "ev_net_bps": net * 1e4,
        "approved_order": approved, "instrument": cap, "explanation": explanation});
    d.store.append("decisions", &rec);
    d.decisions.push_front(rec.clone());
    d.decisions.truncate(300);
    if let Some(o) = approved {
        let c = cap.unwrap();
        let p_sl = fp.p_stop;
        let snap = crate::learn::snapshot(&crate::learn::DecisionCtx {
            trace: &trace,
            pair,
            ts_ms: now,
            side,
            mu: if side == Side::Long { mu } else { -mu },
            sigma_1s: sig,
            obi,
            spread_bps,
            book_age_ms: book_age,
            data_ok,
            regimes: &reg,
            p_tp: if side_auth {
                p_recal.unwrap_or(fp.p_target)
            } else {
                fp.p_target
            },
            p_sl,
            tp_frac: tp,
            sl_frac: sl,
            fee: c.taker_fee,
            slip,
            mid,
            calibrated: side_auth,
            order: &o,
            model_version: MODEL_ID,
            gamble,
        });
        match crate::learn::record_decision(d, snap) {
            Ok(()) if crate::live_exec::live_installed() => {
                let lev = |x: f64| (x.is_finite() && x >= 1.0).then_some(x as u32);
                let job = crate::live_exec::LiveJob {
                    req: crate::live_exec::LiveOrderRequest {
                        trade_id: trace.clone(),
                        pair: o.pair.clone(),
                        margin: TRADE_MARGIN.into(),
                        long: o.side == Side::Long,
                        qty: o.qty,
                        ref_price: o.entry_price,
                        stop_loss: o.stop_loss,
                        take_profit: o.take_profit,
                        governor_leverage: o.leverage.floor().max(1.0) as u32,
                        exchange_max_leverage: lev(if o.side == Side::Long {
                            c.max_leverage_long
                        } else {
                            c.max_leverage_short
                        }),
                        governor_approved: true,
                        entry_limit: maker_entry.then_some(o.entry_price),
                        fill_timeout_ms: maker_entry
                            .then_some(d.settings.maker_timeout_ms.max(1) as u64),
                    },
                    risk_at_stop: o.risk_at_stop,
                    risk_fraction: d.gov.policy.risk_fraction,
                    created_ms: now,
                    gamble_floor: d.gov.policy.gamble.as_ref().map(|g| g.floor()),
                    margin: o.margin_account,
                };
                if !crate::live_exec::submit(job) {
                    d.incident(
                        "live",
                        format!("{trace}: live task unavailable, entry NOT sent"),
                    );
                }
            }
            Ok(()) => open_paper(d, o, c, fx.unwrap(), mid, rec),
            Err(e) => d.incident(
                "learning",
                format!("{trace}: decision snapshot not journaled, entry NOT sent: {e}"),
            ),
        }
    }
}

fn open_paper(
    d: &mut Desk,
    o: qe_risk::ApprovedOrder,
    c: InstrumentCapability,
    fx: f64,
    mid: f64,
    rec: Value,
) {
    let now = now_ms() as u64;
    let policy = d.gov.policy.clone();
    let maker = (d.settings.gamble_maker_entry && policy.is_gamble()).then(|| MakerEntry {
        price: o.entry_price,
        placed_ms: now as i64,
        deadline_ms: now as i64 + d.settings.maker_timeout_ms,
    });
    let mut pos = Position::new(o, &policy);
    let actions = pos.start(now);
    d.positions.push(PaperPos {
        pos,
        margin: TRADE_MARGIN.into(),
        fx,
        taker_fee: c.taker_fee,
        maker_fee: c.maker_fee,
        mark: mid,
        entry_ref_mid: mid,
        proposal: rec,
        maker,
    });
    let idx = d.positions.len() - 1;
    run_actions(d, idx, actions);
    sweep(d);
}

fn fill_rec(d: &mut Desk, idx: usize, buy: bool, f: qe_risk::Fill, ref_mid: f64, reason: &str) {
    fill_rec_liq(d, idx, buy, f, ref_mid, reason, "taker")
}

fn fill_rec_liq(
    d: &mut Desk,
    idx: usize,
    buy: bool,
    f: qe_risk::Fill,
    ref_mid: f64,
    reason: &str,
    liquidity: &str,
) {
    let p = &d.positions[idx];
    let slip_bps = if buy {
        (f.price - ref_mid) / ref_mid * 1e4
    } else {
        (ref_mid - f.price) / ref_mid * 1e4
    };
    let rec = json!({"id": format!("{}-{}", p.pos.order.trace_id, d.fills.len()), "position_id": p.pos.order.trace_id,
        "pair": p.pos.order.pair, "side": if buy {"buy"} else {"sell"}, "qty": f.qty, "price": f.price,
        "ref_mid": ref_mid, "slippage_bps": slip_bps, "fee": f.fee, "fee_inr": f.fee * p.fx, "liquidity": liquidity,
        "reason": reason, "ts_ms": now_ms(), "paper": true});
    d.store.append("fills", &rec);
    d.fills.push_front(rec);
    d.fills.truncate(200);
}

fn exit_reason_str(r: qe_risk::ExitReason) -> &'static str {
    use qe_risk::ExitReason::*;
    match r {
        StopLoss => "sl",
        TakeProfit => "tp",
        EdgeNegative => "edge_lost",
        DataIntegrity => "data_integrity",
        TimeExpiry => "time_stop",
        Emergency => "flatten",
        ProtectionFailed => "protection_failed",
        Liquidated => "liquidated",
    }
}

/// Execute lifecycle actions against the paper exchange. Every action is either
/// performed (paper) or recorded; nothing reaches CoinDCX.
pub fn run_actions(d: &mut Desk, idx: usize, actions: Vec<Action>) {
    let mut q: VecDeque<Action> = actions.into();
    while let Some(a) = q.pop_front() {
        let now = now_ms();
        let pair = d.positions[idx].pos.order.pair.clone();
        let book = d.books.get(&pair).filter(|b| b.is_sane()).cloned();
        let px = PaperExchange {
            taker_fee: d.positions[idx].taker_fee,
            slippage_bps: d.settings.paper_slippage_bps,
        };
        match a {
            Action::SetLeverage { pair, leverage } => d.audit(
                "execution",
                format!("paper: leverage {leverage}x for {pair} recorded (no exchange call)"),
                Value::Null,
            ),
            Action::SubmitEntry {
                client_id,
                side,
                qty,
            } => {
                let buy = side == Side::Long;
                if let Some(m) = d.positions[idx].maker.clone() {
                    d.maker_stats.placed += 1;
                    d.audit(
                        "execution",
                        format!(
                            "paper maker entry {client_id} resting: {} {qty} @ {} (limit, GTC, cancel after {} ms)",
                            if buy { "buy" } else { "sell" },
                            m.price,
                            m.deadline_ms - m.placed_ms
                        ),
                        Value::Null,
                    );
                    continue;
                }
                let (inc, minq) = d
                    .instruments
                    .get(&format!("{pair}|{TRADE_MARGIN}"))
                    .map(|c| capability(&c.1))
                    .map_or((0.0, 0.0), |c| (c.quantity_increment, c.min_quantity));
                let decided = d.positions[idx].proposal["ts_ms"].as_i64().unwrap_or(now);
                let sim = d.fill_model.entry(qty, inc, minq);
                let (fill_qty, partial, lat) = match sim {
                    SimFill::Rejected { reason, latency_ms } => {
                        d.incident(
                            "entry_rejected",
                            format!(
                                "{client_id}: {reason} after {latency_ms} ms (paper fill model)"
                            ),
                        );
                        let more = d.positions[idx].pos.on_entry_rejected();
                        q.extend(more);
                        continue;
                    }
                    SimFill::Filled {
                        qty,
                        partial,
                        latency_ms,
                    } => (qty, partial, latency_ms),
                };
                match book.and_then(|b| {
                    px.market_fill(buy, fill_qty, b.best_bid()?, b.best_ask()?)
                        .map(|f| (f, micro::mid(b.best_bid().unwrap(), b.best_ask().unwrap())))
                }) {
                    Some((f, m)) => {
                        let latency_ms = (now - decided).max(0) as f64 + lat as f64;
                        d.latency.push(latency_ms);
                        fill_rec(
                            d,
                            idx,
                            buy,
                            f,
                            m,
                            if partial { "entry_partial" } else { "entry" },
                        );
                        if let Some(fr) = d.fills.front_mut() {
                            fr["latency_ms"] = json!(latency_ms);
                            fr["simulated_latency_ms"] = json!(lat);
                            fr["partial"] = json!(partial);
                        }
                        d.audit(
                            "execution",
                            format!("paper entry {client_id} filled {} @ {}", f.qty, f.price),
                            Value::Null,
                        );
                        let mut more = d.positions[idx]
                            .pos
                            .on_fill(f.qty, f.price, f.fee, now as u64);
                        if partial {
                            more.extend(d.positions[idx].pos.on_entry_done_partial());
                        }
                        q.extend(more);
                    }
                    None => {
                        d.incident(
                            "entry_rejected",
                            format!("{client_id}: no sane book for paper fill"),
                        );
                        let more = d.positions[idx].pos.on_entry_rejected();
                        q.extend(more);
                    }
                }
            }
            Action::AttachProtection {
                stop_loss,
                take_profit,
                qty,
            } => {
                d.positions[idx].pos.on_protection_confirmed();
                d.audit(
                    "execution",
                    format!("paper protection: SL {stop_loss} TP {take_profit} qty {qty}, enforced by the server-side monitor (exchange-side TP/SL not used in paper)"),
                    Value::Null,
                );
            }
            Action::CancelEntryRemainder { client_id } => d.audit(
                "execution",
                format!("paper: cancel entry remainder {client_id}"),
                Value::Null,
            ),
            Action::SubmitExit {
                client_id,
                reason,
                qty,
            } => {
                let buy = d.positions[idx].pos.order.side == Side::Short;
                match book.and_then(|b| {
                    px.market_fill(buy, qty, b.best_bid()?, b.best_ask()?)
                        .map(|f| (f, micro::mid(b.best_bid().unwrap(), b.best_ask().unwrap())))
                }) {
                    Some((f, m)) => {
                        fill_rec(d, idx, buy, f, m, exit_reason_str(reason));
                        let fx = d.positions[idx].fx;
                        if let Some(pnl) =
                            d.positions[idx].pos.on_exit_fill(f.qty, f.price, f.fee, fx)
                        {
                            close_trade(d, idx, pnl, f.price, exit_reason_str(reason));
                        }
                    }
                    None => d.incident(
                        "exit_unfilled",
                        format!("{client_id}: no sane book; reconcile on ack timeout"),
                    ),
                }
            }
            Action::Reconcile { client_id } => {
                // paper exchange truth: remaining exposure
                let p = &d.positions[idx].pos;
                let qty = p.filled_qty - p.exited_qty;
                d.audit(
                    "execution",
                    format!("paper reconcile {client_id}: exchange qty {qty}"),
                    Value::Null,
                );
                let more = d.positions[idx].pos.on_reconcile(qty, now as u64);
                q.extend(more);
            }
            Action::Incident { kind, detail } => d.incident(&kind, detail),
        }
    }
}

fn close_trade(d: &mut Desk, idx: usize, pnl: f64, exit_price: f64, reason: &str) {
    let now = now_ms();
    let fees_inr = {
        let pp = &d.positions[idx];
        (pp.pos.entry_fees + pp.pos.exit_fees) * pp.fx
    };
    d.cap.on_realised(dec(pnl), dec(fees_inr));
    let eq = d.cap.equity_f64();
    d.gov.record_realised(pnl, eq, now as u64);
    for a in std::mem::take(&mut d.gov.audit) {
        d.store
            .append("governor", &serde_json::to_value(&a).unwrap_or_default());
    }
    let pp = &d.positions[idx];
    let p = &pp.pos;
    let s = if p.order.side == Side::Long {
        1.0
    } else {
        -1.0
    };
    let gross = (p.exit_value - p.avg_entry * p.exited_qty) * s * pp.fx;
    let fees = (p.entry_fees + p.exit_fees) * pp.fx;
    let held = p.age_ms(now as u64).unwrap_or(0) as f64 / 1000.0;
    let rec = json!({"id": p.order.trace_id, "pair": p.order.pair, "side": if s > 0.0 {"long"} else {"short"},
        "entry_price": p.avg_entry, "exit_price": exit_price, "qty": p.exited_qty, "gross_pnl": gross,
        "fees": fees, "net_pnl": pnl, "held_s": held, "exit_reason": reason, "closed_ms": now,
        "leverage": p.order.leverage, "currency": "INR", "paper": true, "equity_after": eq,
        "notional": p.order.notional_account,
        "mode": if d.gov.policy.is_gamble() { "gamble" } else { "standard" }});
    if d.gov.policy.is_gamble() {
        let k = d
            .gamble_trades
            .entry((p.order.pair.clone(), p.order.side == Side::Long))
            .or_default();
        k.n += 1;
        k.hits += (pnl > 0.0) as u32;
    }
    let msg = format!(
        "paper {} {} closed ({reason}) net INR {pnl:.4} held {held:.1}s",
        rec["side"], p.order.pair
    );
    d.store.append("closed", &rec);
    d.store.append(
        "equity",
        &json!({"ts_ms": now, "equity": eq, "pnl": pnl, "paper": true}),
    );
    d.equity_curve.push((now, eq));
    d.audit("trade", msg, rec.clone());
    d.closed.push_front(rec);
    d.closed.truncate(200);
    crate::learn::on_closed(d, idx, exit_price, reason, now);
    experiment_stop_check(d);
    gamble_stop_check(d);
}

/// Drop closed/rejected positions; a rejected entry frees its governor slot.
pub fn sweep(d: &mut Desk) {
    let now = now_ms() as u64;
    let mut keep = vec![];
    for p in std::mem::take(&mut d.positions) {
        match p.pos.state {
            State::Closed => {}
            State::Rejected => {
                let eq = d.cap.equity_f64();
                d.gov.record_realised(0.0, eq, now);
            }
            _ => keep.push(p),
        }
    }
    d.positions = keep;
}

/// Expected value of continuing to hold, from the current price to the original
/// barriers over the remaining time (first-passage lattice). The entry cost is
/// sunk and the exit cost is paid either way, so the exit rule compares this
/// hold edge against -exit_cost.
pub fn hold_edge(d: &Desk, pp: &PaperPos, now: i64) -> f64 {
    let p = &pp.pos;
    let Some(sig) = d.hist.get(&p.order.pair).and_then(sigma_per_s) else {
        return 0.0;
    };
    let mu = d
        .forecasts
        .get(&p.order.pair)
        .and_then(|f| f["mu_per_s"].as_f64())
        .unwrap_or(0.0);
    let mid = pp.mark;
    let (m, tp_r, sl_r) = if p.order.side == Side::Long {
        (
            mu,
            (p.order.take_profit / mid).ln(),
            (mid / p.order.stop_loss).ln(),
        )
    } else {
        (
            -mu,
            (mid / p.order.take_profit).ln(),
            (p.order.stop_loss / mid).ln(),
        )
    };
    let t_rem = p.remaining_ms(now as u64).unwrap_or(0) as f64 / 1000.0;
    if !(tp_r > 0.0 && sl_r > 0.0 && t_rem >= 1.0 && sig > 0.0) {
        return 0.0;
    }
    let fp = prob::first_passage(m, sig, tp_r, sl_r, t_rem, 0.5);
    let exit_cost = pp.taker_fee + d.settings.paper_slippage_bps / 1e4;
    prob::bracket_ev(&fp, tp_r, sl_r, 0.0) + exit_cost
}

/// Independent server-side lifecycle tick (called every 250 ms by its own task,
/// not by any browser timer): barriers, edge, data integrity, 120 s deadline.
pub fn lifecycle_tick(d: &mut Desk) {
    let now = now_ms();
    for idx in 0..d.positions.len() {
        if d.positions[idx].maker.is_some() {
            maker_tick(d, idx, now);
            continue;
        }
        let pair = d.positions[idx].pos.order.pair.clone();
        let book = d.books.get(&pair).filter(|b| b.is_sane()).cloned();
        let actions = match book {
            Some(b) => {
                let (bid, ask) = (b.best_bid().unwrap(), b.best_ask().unwrap());
                d.positions[idx].mark = micro::mid(bid, ask);
                if (d.gov.policy.is_experiment() || d.gov.policy.is_gamble())
                    && d.positions[idx].pos.liquidation_hit(bid, ask)
                {
                    liquidate(d, idx);
                    continue;
                }
                let data_ok = now - b.ts < 3_000;
                // gamble bets are pure brackets: SL / TP / 120 s, no edge exit
                let edge = if d.gov.policy.is_gamble() {
                    0.0
                } else {
                    hold_edge(d, &d.positions[idx], now)
                };
                d.positions[idx]
                    .pos
                    .on_market_tick(bid, ask, edge, data_ok, now as u64)
            }
            None => d.positions[idx].pos.on_timer(now as u64),
        };
        if !actions.is_empty() {
            run_actions(d, idx, actions);
        }
    }
    sweep(d);
    gamble_stop_check(d);
    capital_tick(d, now);
    report_tick(d, now);
    crate::learn::resolve(d, now);
}

/// Quantity that public trades printed strictly through a resting limit after
/// it was placed (buy limit: prints below it; sell limit: prints above it).
pub fn maker_through_qty(trades: &VecDeque<timeframe::Trade>, buy: bool, m: &MakerEntry) -> f64 {
    trades
        .iter()
        .filter(|t| t.ts_ms >= m.placed_ms)
        .filter(|t| {
            if buy {
                t.price < m.price
            } else {
                t.price > m.price
            }
        })
        .map(|t| t.qty)
        .sum()
}

/// Advance a resting paper maker entry: fill it when enough volume printed
/// through its price; at the deadline fill what printed through (if it meets
/// the exchange minimum) and cancel the rest. No taker chase.
fn maker_tick(d: &mut Desk, idx: usize, now: i64) {
    let Some(m) = d.positions[idx].maker.clone() else {
        return;
    };
    let pp = &d.positions[idx];
    let pair = pp.pos.order.pair.clone();
    let buy = pp.pos.order.side == Side::Long;
    let want = pp.pos.order.qty;
    let client_id = pp.pos.entry_id();
    let through = d
        .trades
        .get(&pair)
        .map_or(0.0, |t| maker_through_qty(t, buy, &m));
    let expired = now >= m.deadline_ms;
    let (inc, minq) = d
        .instruments
        .get(&format!("{pair}|{TRADE_MARGIN}"))
        .map(|c| capability(&c.1))
        .map_or((0.0, 0.0), |c| (c.quantity_increment, c.min_quantity));
    let qty = if through + 1e-12 >= want {
        want
    } else if expired {
        let q = if inc > 0.0 {
            (through / inc + 1e-9).floor() * inc
        } else {
            through
        };
        if q >= minq.max(1e-12) {
            q
        } else {
            0.0
        }
    } else {
        return;
    };
    d.positions[idx].maker = None;
    if qty <= 0.0 {
        d.maker_stats.cancelled += 1;
        d.audit(
            "execution",
            format!("paper maker entry {client_id} unfilled after {} ms: cancelled, no taker chase; re-evaluating", now - m.placed_ms),
            Value::Null,
        );
        let more = d.positions[idx].pos.on_entry_rejected();
        run_actions(d, idx, more);
        return;
    }
    let partial = qty + 1e-12 < want;
    if partial {
        d.maker_stats.partial += 1;
    } else {
        d.maker_stats.filled += 1;
    }
    let f = qe_risk::Fill {
        qty,
        price: m.price,
        fee: qty * m.price * d.positions[idx].maker_fee,
    };
    let ref_mid = d
        .books
        .get(&pair)
        .filter(|b| b.is_sane())
        .map_or(m.price, |b| {
            micro::mid(b.best_bid().unwrap(), b.best_ask().unwrap())
        });
    fill_rec_liq(
        d,
        idx,
        buy,
        f,
        ref_mid,
        if partial { "entry_partial" } else { "entry" },
        "maker",
    );
    if let Some(fr) = d.fills.front_mut() {
        fr["latency_ms"] = json!(now - m.placed_ms);
        fr["partial"] = json!(partial);
    }
    d.audit(
        "execution",
        format!(
            "paper maker entry {client_id} filled {qty} @ {} after {} ms",
            m.price,
            now - m.placed_ms
        ),
        Value::Null,
    );
    let mut more = d.positions[idx]
        .pos
        .on_fill(qty, m.price, f.fee, now as u64);
    if partial {
        more.extend(d.positions[idx].pos.on_entry_done_partial());
    }
    run_actions(d, idx, more);
}

/// Drive the capital plan: stage transitions and the checkpoint workflow
/// (stop entries, flatten, cancel entry orders, verify flat, query the PAPER
/// ledger balance, reconcile, prepare the withdrawal for the user).
pub fn capital_tick(d: &mut Desk, now: i64) {
    if let Some(ev) = d.cap.on_clock(now as u64) {
        d.store
            .append("capital", &serde_json::to_value(&ev).unwrap_or_default());
        d.audit(
            "capital",
            format!("{:?} -> {:?}: {}", ev.from, ev.to, ev.note),
            Value::Null,
        );
        match ev.to {
            Stage::Stage1Growth => {
                // fresh stage: session-loss and HWM limits measured from this stage start
                let (paused, killed) = (d.gov.entries_paused, d.gov.killed);
                let pol = d.gov.policy.clone();
                let eq = d.cap.equity_f64();
                d.gov = RiskGovernor::new(pol, eq, d.settings.stage1_target_inr);
                d.gov.entries_paused = paused;
                d.gov.killed = killed;
            }
            Stage::Checkpoint1 | Stage::FinalCheckpoint => {
                d.checkpoint_handled = true;
                let _ = d.gov.pause_entries(true, now as u64);
                d.controls.entries_paused = true;
                let _ = d.cap.report_step(Step::StopEntries, true, "entries paused");
                let n = flatten(d, "checkpoint");
                let _ = d
                    .cap
                    .report_step(Step::Flatten, true, &format!("{n} exit action(s)"));
                let _ = d.cap.report_step(
                    Step::CancelEntryOrders,
                    true,
                    "paper entries are immediate (no resting entry orders); remainders cancelled on fill",
                );
                d.risk_events.push_front(format!(
                    "{}: {:?} - entries stopped, flatten issued",
                    now, ev.to
                ));
            }
            _ => {}
        }
    }
    let Some(cp) = d.cap.checkpoint.clone() else {
        return;
    };
    if cp.status(Step::VerifyFlat) != Some(qe_engine::capital::StepStatus::Done) {
        if d.positions.is_empty() {
            let _ = d
                .cap
                .report_step(Step::VerifyFlat, true, "no open paper positions");
        } else {
            return;
        }
    }
    if cp.balance.is_none() {
        let mut src = qe_engine::capital::PaperLedgerBalance(d.cap.equity);
        if let Err(e) = d.cap.record_balance(&mut src, now as u64) {
            d.incident("checkpoint", e);
        } else if let Some(cp) = &d.cap.checkpoint {
            d.store
                .append("capital", &serde_json::to_value(cp).unwrap_or_default());
            d.audit(
                "capital",
                "checkpoint: flat verified, PAPER ledger balance queried and reconciled; any withdrawal is prepared for the user only".into(),
                Value::Null,
            );
        }
    }
}

/// Paper experiment: the adverse move reached the estimated liquidation price,
/// so the position is closed as liquidated and its margin (plus the entry fee)
/// is lost. Equity never goes below zero.
fn liquidate(d: &mut Desk, idx: usize) {
    let fx = d.positions[idx].fx;
    let liq = d.positions[idx].pos.order.liquidation_price.unwrap_or(0.0);
    let pnl = d.positions[idx]
        .pos
        .close_liquidated(fx)
        .max(-d.cap.equity_f64());
    d.positions[idx].pos.realised_pnl = Some(pnl);
    d.incident(
        "paper_liquidation",
        format!(
            "{} liquidated at est. {liq}: margin lost, net INR {pnl:.4}",
            d.positions[idx].pos.order.pair
        ),
    );
    close_trade(d, idx, pnl, liq, "liquidated");
}

/// Experiment stop: equity below the smallest minimum notional the account can
/// still reach at maximum leverage (over cached INR-margined instruments).
fn experiment_stop_check(d: &mut Desk) {
    if !d.gov.policy.is_experiment() || d.gov.entries_paused {
        return;
    }
    let fx = d.fx_usdtinr().unwrap_or(f64::NAN);
    let eq = d.cap.equity_f64();
    let min_margin = d
        .instruments
        .values()
        .filter(|(_, i)| i.margin_currency_short_name == TRADE_MARGIN && i.status == "active")
        .map(|(_, i)| {
            let c = capability(i);
            c.min_notional * fx / c.max_leverage_long.max(c.max_leverage_short).max(1.0)
        })
        .filter(|x| x.is_finite())
        .fold(f64::INFINITY, f64::min);
    let min_margin = if min_margin.is_finite() {
        min_margin
    } else {
        0.0
    };
    if eq <= 0.0 || eq < min_margin {
        let now = now_ms() as u64;
        let _ = d.gov.pause_entries(true, now);
        d.controls.entries_paused = true;
        d.audit(
            "capital",
            format!("{EXPERIMENT_BANNER}: equity INR {eq:.4} below minimum tradable notional (margin INR {min_margin:.4} at max leverage); trading stopped"),
            Value::Null,
        );
    }
}

/// Forward-monitor totals for the global recalibration of P(win).
pub fn global_counts(c: &Calib) -> qe_risk::gamble::HitCounts {
    qe_risk::gamble::HitCounts {
        n: c.resolved.len() as u32,
        hits: c.resolved.iter().filter(|x| x.1).count() as u32,
        sum_p: c.resolved.iter().map(|x| x.0).sum(),
    }
}

/// Smallest margin (INR) a bet needs at the owner leverage over cached,
/// tradable INR-margined instruments.
pub fn gamble_min_margin(d: &Desk) -> f64 {
    let Some(g) = d.gov.policy.gamble.as_ref() else {
        return 0.0;
    };
    // Only with full INR metadata coverage: a partial cache would overstate the
    // smallest bet and fire the permanent stop spuriously.
    let inr = |k: &&String| k.ends_with(&format!("|{TRADE_MARGIN}"));
    let cached = d.instruments.keys().filter(inr).count() + d.meta_fail.keys().filter(inr).count();
    if d.fut_inr.is_empty() || cached < d.fut_inr.len() {
        return 0.0;
    }
    let fx = d.fx_usdtinr().unwrap_or(f64::NAN);
    let m = d
        .instruments
        .values()
        .filter(|(_, i)| i.margin_currency_short_name == TRADE_MARGIN && i.is_tradable())
        .map(|(_, i)| {
            let c = capability(i);
            let lev = (g.leverage as f64)
                .min(c.max_leverage_long.max(c.max_leverage_short))
                .max(1.0);
            c.min_notional * fx / lev
        })
        .filter(|x| x.is_finite())
        .fold(f64::INFINITY, f64::min);
    if m.is_finite() {
        m
    } else {
        0.0
    }
}

/// Gamble floor stop: permanent until the owner re-arms (marker file).
pub fn gamble_stop_check(d: &mut Desk) {
    if !d.gov.policy.is_gamble() {
        return;
    }
    let eq = d.cap.equity_f64();
    let min_margin = if crate::live_exec::gamble_floor_hit() {
        f64::INFINITY // a live bet hit or would breach the real-balance floor
    } else {
        gamble_min_margin(d)
    };
    if let Some(why) = d.gov.gamble_floor_check(eq, min_margin, now_ms() as u64) {
        d.controls.entries_paused = true;
        let path = d.store.dir.join(GAMBLE_STOP_FILE);
        let rec = json!({"ts_ms": now_ms(), "reason": why, "equity": eq});
        if let Err(e) = std::fs::write(&path, rec.to_string()) {
            d.incident("gamble", format!("floor marker {}: {e}", path.display()));
        }
        d.audit(
            "capital",
            format!("{GAMBLE_BANNER}: floor stop - {why}; stopped until the owner re-arms"),
            rec,
        );
    }
}

/// Epoch ms -> ISO-8601 UTC (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn iso_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + (month <= 2) as i64;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

/// Closed gamble bets: (trades, wins, losses, net P&L INR, per-notional returns).
fn gamble_closed(d: &Desk) -> (usize, usize, usize, f64, Vec<f64>) {
    let g: Vec<&Value> = d.closed.iter().filter(|c| c["mode"] == "gamble").collect();
    let pnl = |c: &Value| c["net_pnl"].as_f64().unwrap_or(0.0);
    let rets = g
        .iter()
        .filter_map(|c| Some(pnl(c) / c["notional"].as_f64().filter(|n| *n > 0.0)?))
        .collect();
    (
        g.len(),
        g.iter().filter(|c| pnl(c) > 0.0).count(),
        g.iter().filter(|c| pnl(c) < 0.0).count(),
        g.iter().map(|c| pnl(c)).sum(),
        rets,
    )
}

/// Target-probability Monte Carlo for the dashboard (`qe_math::gamble::target_odds`):
/// P(reach the target before the deadline) and P(hit the floor) under the
/// owner's stake policy. Win rate and payoff come from realised gamble bets
/// once >= 10 exist, otherwise from the best fresh candidate's estimate
/// (P(win) conditional on a barrier being hit; time exits are not modelled).
pub fn refresh_gamble_sim(d: &mut Desk, now: i64) {
    use qe_math::gamble as gm;
    let Some(params) = d.gov.policy.gamble.clone() else {
        return;
    };
    let (n, wins, _, _, rets) = gamble_closed(d);
    let best = d
        .gamble_board
        .values()
        .filter(|b| now - b.ts_ms <= GAMBLE_BOARD_MS)
        .max_by(|a, b| a.score.total_cmp(&b.score))
        .cloned();
    let cost = best.as_ref().map_or(0.0015, |b| b.cost_frac);
    let (win_p, payoff, source) = if n >= 10 {
        let w: Vec<f64> = rets.iter().copied().filter(|x| *x > 0.0).collect();
        let l: Vec<f64> = rets
            .iter()
            .copied()
            .filter(|x| *x <= 0.0)
            .map(|x| -x)
            .collect();
        let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
        let payoff = if l.is_empty() {
            f64::INFINITY
        } else {
            avg(&w) / avg(&l).max(1e-12)
        };
        (
            wins as f64 / n as f64,
            payoff,
            format!("{n} realised gamble bets"),
        )
    } else if let Some(b) = &best {
        (
            b.p_win / (b.p_win + b.p_stop).max(1e-12),
            b.tp_frac / b.sl_frac.max(1e-12),
            format!(
                "estimate for the best current bet {} {} (fewer than 10 realised bets)",
                b.pair,
                if b.long { "long" } else { "short" }
            ),
        )
    } else {
        d.gamble_sim = Some(json!({"p_target": null, "p_floor": null,
            "source": "no candidate estimate yet", "computed_ms": now}));
        return;
    };
    let hours = ((d.settings.stage2_deadline_ms - now) as f64 / 3.6e6).max(0.0);
    let el_h = ((now - d.started_ms) as f64 / 3.6e6).max(1e-9);
    // one position at a time, each held <= 120 s: measured rate once >= 5 bets
    let bph = if n >= 5 {
        (n as f64 / el_h).min(30.0)
    } else {
        20.0
    };
    let fx = d.fx_usdtinr().unwrap_or(f64::NAN);
    let min_notional = d
        .instruments
        .values()
        .filter(|(_, i)| i.margin_currency_short_name == TRADE_MARGIN && i.is_tradable())
        .map(|(_, i)| i.min_notional * fx)
        .filter(|x| x.is_finite())
        .fold(f64::INFINITY, f64::min);
    let min_notional = if min_notional.is_finite() {
        min_notional
    } else {
        0.0
    };
    let input = gm::TargetInputs {
        equity: dec(d.cap.equity_f64()),
        target: dec(d.settings.stage1_target_inr),
        floor: params.floor_inr,
        win_p,
        payoff_ratio: if payoff.is_finite() { payoff } else { 1e6 },
        cost_frac: cost,
        leverage: Decimal::from(params.leverage),
        bets_remaining: (hours * bph) as u32,
        min_notional: dec(min_notional),
        max_notional: Decimal::from(1_000_000_000u64),
        policy: match params.stake_policy {
            qe_risk::gamble::StakePolicy::Fixed => gm::StakePolicy::Fixed {
                stake_inr: params.stake_inr,
            },
            qe_risk::gamble::StakePolicy::TargetOptimal => gm::StakePolicy::BoldPlay,
        },
    };
    let r = gm::target_odds(&input, 2_000, 42);
    d.gamble_sim = Some(json!({"p_target": r.p_target, "p_floor": r.p_floor,
        "expected_bets": r.expected_bets, "median_final_equity": r.median_final_equity,
        "bets_remaining": input.bets_remaining, "sims": 2_000, "seed": 42,
        "win_p": win_p, "payoff_ratio": input.payoff_ratio, "cost_frac": cost,
        "barriers": if d.settings.payoff.is_some() { "payoff plan (qe_risk::gamble::payoff)" } else { "sigma barriers" },
        "bets_per_hour": bph, "hours_remaining": hours,
        "source": source, "computed_ms": now}));
}

/// `/api/state.gamble` (docs/DASHBOARD_API.md): null unless gamble mode runs.
pub fn gamble_json(d: &Desk) -> Value {
    let Some(g) = d
        .gov
        .policy
        .gamble
        .as_ref()
        .filter(|_| d.gov.policy.is_gamble())
    else {
        return Value::Null;
    };
    let (trades, wins, losses, net, _) = gamble_closed(d);
    let eq = d.cap.equity_f64();
    let sim = d.gamble_sim.clone().unwrap_or(Value::Null);
    let now = now_ms();
    let mut board: Vec<&qe_risk::gamble::BetEstimate> = d
        .gamble_board
        .values()
        .filter(|b| now - b.ts_ms <= GAMBLE_BOARD_MS)
        .collect();
    board.sort_by(|a, b| b.score.total_cmp(&a.score));
    json!({
        "enabled": d.gov.gamble_stopped.is_none() && !d.gov.killed,
        "policy": match g.stake_policy { qe_risk::gamble::StakePolicy::Fixed => "fixed",
            qe_risk::gamble::StakePolicy::TargetOptimal => "bold_play" },
        "stake_inr": g.stake_inr.to_string(), "leverage": g.leverage.to_string(),
        "floor_inr": g.floor_inr.to_string(), "equity_inr": format!("{:.4}", eq),
        "target_inr": format!("{}", d.settings.stage1_target_inr),
        "deadline": iso_utc(d.settings.stage2_deadline_ms),
        "trades": trades, "wins": wins, "losses": losses,
        "win_rate": if trades > 0 { Some(wins as f64 / trades as f64) } else { None },
        "net_pnl_inr": format!("{net:.4}"),
        "distance_to_floor_inr": format!("{:.4}", eq - g.floor()),
        "p_target": sim["p_target"], "p_floor": sim["p_floor"],
        "stopped_reason": d.gov.gamble_stopped.clone().or_else(|| d.gov.killed.then(|| "kill switch engaged".to_string())),
        "paper": !crate::live_exec::live_installed(),
        "simulation": sim,
        "entry": if d.settings.gamble_maker_entry { "maker" } else { "taker" },
        "maker": {"placed": d.maker_stats.placed, "filled": d.maker_stats.filled,
            "partial": d.maker_stats.partial, "cancelled": d.maker_stats.cancelled,
            "fill_rate": if d.maker_stats.placed > 0 {
                Some((d.maker_stats.filled + d.maker_stats.partial) as f64 / d.maker_stats.placed as f64) } else { None },
            "timeout_ms": d.settings.maker_timeout_ms,
            "note": "paper fills only when trades print strictly through the resting price (queue position unknown, assumed last); CoinDCX futures post_only is documented as unsupported, so live limits can still fill as taker"},
        "candidates": board.iter().take(8).collect::<Vec<_>>(),
        "ev_note": "Each bet's estimated expected value after costs is shown per candidate; it is usually negative (round trip ~13-16 bps, no measured edge). Gamble mode bets anyway by the owner's choice; the ranking only picks the least-bad bet.",
    })
}

pub fn flatten(d: &mut Desk, why: &str) -> usize {
    let now = now_ms() as u64;
    let mut n = 0;
    for idx in 0..d.positions.len() {
        if d.positions[idx].maker.take().is_some() {
            d.maker_stats.cancelled += 1;
            d.positions[idx].pos.on_entry_rejected();
            n += 1;
            continue;
        }
        let a = d.positions[idx].pos.emergency_exit(now);
        n += a.len();
        run_actions(d, idx, a);
    }
    sweep(d);
    d.audit(
        "control",
        format!("flatten ({why}): {n} exit actions"),
        Value::Null,
    );
    n
}

pub fn position_json(pp: &PaperPos, now: i64) -> Value {
    let p = &pp.pos;
    let s = if p.order.side == Side::Long {
        1.0
    } else {
        -1.0
    };
    let open_qty = p.filled_qty - p.exited_qty;
    json!({"id": p.order.trace_id, "pair": p.order.pair, "side": if s > 0.0 {"long"} else {"short"},
        "qty": open_qty, "entry_price": p.avg_entry, "mark_price": pp.mark, "tp_price": p.order.take_profit,
        "sl_price": p.order.stop_loss, "leverage": p.order.leverage, "notional": p.order.notional_account,
        "margin": p.order.margin_account, "liquidation_price": p.order.liquidation_price,
        "opened_ms": p.opened_at_ms, "age_s": p.age_ms(now as u64).map(|x| x as f64 / 1000.0),
        "time_left_s": p.remaining_ms(now as u64).map(|x| x as f64 / 1000.0),
        "unrealized_pnl": (pp.mark - p.avg_entry) * open_qty * s * pp.fx, "fees_paid": p.entry_fees * pp.fx,
        "state": format!("{:?}", p.state), "margin_currency": pp.margin, "paper": true,
        "entry_ref_mid": pp.entry_ref_mid, "trace": pp.proposal["trace_id"],
        "risk_at_stop": p.order.risk_at_stop, "exit_reason": p.exit_reason.map(exit_reason_str)})
}

/// Active profile for the dashboard/status: the experiment banner is set only
/// when the opt-in paper experiment is running.
pub fn profile_json(d: &Desk) -> Value {
    if d.gov.policy.is_gamble() {
        return json!({"name": GAMBLE_PROFILE, "experiment": false, "gamble": true,
            "banner": GAMBLE_BANNER,
            "rules": ["owner armed (configs/gamble.toml)", "calibration and net-EV gates skipped",
                "owner stake / leverage (exchange-capped) / floor", "floor is a permanent stop",
                "one position, SL + TP, 120 s time exit, kill switch", "no size increase after a loss",
                "best-ranked bet only (estimated P(win) and payoff after costs)"]});
    }
    let exp = d.gov.policy.is_experiment();
    json!({"name": d.settings.profile, "experiment": exp,
        "banner": if exp { json!(EXPERIMENT_BANNER) } else { Value::Null },
        "rules": if exp { json!(["paper only", "calibration gate disabled", "no session-loss / HWM stop",
            "all paper equity as margin at the highest exchange-permitted leverage",
            "one position at a time, profits compounded", "raw EV > 0 after fees/spread/slippage",
            "model SL/TP, 120 s time stop, liquidation modelled (margin lost)",
            "stops when equity is below the minimum tradable notional"]) } else { json!([]) }})
}

pub fn policy_json(d: &Desk) -> Value {
    let p = &d.gov.policy;
    json!({"profile": profile_json(d),"tier": if p.mode == CapitalMode::Growth {"growth"} else {"protective"},
        "risk_per_trade_frac": p.risk_fraction, "effective_risk_frac": d.gov.effective_risk_fraction(),
        "max_leverage": if p.policy_max_leverage.is_finite() { json!(p.policy_max_leverage) } else { json!("exchange max per instrument/tier") },
        "max_positions": p.max_open_positions, "daily_loss_limit_frac": p.max_session_loss_fraction,
        "max_drawdown_from_hwm": p.max_drawdown_from_hwm, "loss_step_down": p.loss_step_down,
        "liquidation_buffer": p.liquidation_buffer, "exit_lead_ms": p.exit_lead_ms,
        "min_expected_net_edge": p.min_expected_net_edge,
        "kelly_fraction": null, "max_hold_s": 120})
}

pub fn account_json(d: &Desk) -> Value {
    let wins = d
        .closed
        .iter()
        .filter(|c| c["net_pnl"].as_f64().unwrap_or(0.0) > 0.0)
        .count();
    let losses = d
        .closed
        .iter()
        .filter(|c| c["net_pnl"].as_f64().unwrap_or(0.0) < 0.0)
        .count();
    let fees: f64 = d.closed.iter().filter_map(|c| c["fees"].as_f64()).sum();
    let realized: f64 = d.closed.iter().filter_map(|c| c["net_pnl"].as_f64()).sum();
    let unreal = d.unrealized();
    let halt_reason = if d.gov.killed {
        Some("kill switch engaged".to_string())
    } else if d.gov.entries_paused {
        Some("entries paused".to_string())
    } else if !d.controls.paper_running {
        Some("paper trading stopped".to_string())
    } else {
        None
    };
    json!({"currency": "INR", "paper": true, "label": "PAPER equity (configured start value, not a CoinDCX balance)",
        "starting_equity": d.settings.paper_start_equity_inr, "stage_start_equity": d.gov.stage_start_equity,
        "equity": d.cap.equity_f64() + unreal, "realised_equity": d.cap.equity_f64(),
        "realized_pnl": realized, "unrealized_pnl": unreal, "fees_paid": fees, "day_pnl": d.gov.session_realised_pnl,
        "used_margin": d.used_margin(), "available": (d.cap.equity_f64() - d.used_margin()).max(0.0),
        "trades": d.closed.len(), "wins": wins, "losses": losses, "halted": halt_reason.is_some(),
        "halt_reason": halt_reason, "high_water_mark": d.gov.high_water_mark,
        "consecutive_losses": d.gov.consecutive_losses, "stage": format!("{:?}", d.cap.stage),
        "withdrawals": d.cap.withdrawals, "policy": policy_json(d)})
}

/// Target controller: required growth, trade-frequency scenarios, actual stats
/// and a seeded stochastic simulation (cached in `d.sim`).
pub fn target_json(d: &Desk, now: i64) -> Value {
    let s = &d.settings;
    let _ = s;
    let stage = match d.cap.stage {
        Stage::Stage0Paper => 0,
        Stage::Stage1Growth | Stage::Checkpoint1 => 1,
        _ => 2,
    };
    let start = dec_f64(d.cap.stage_start_equity);
    let (t, dl) = d.cap.target();
    let target = dec_f64(t);
    let deadline = dl as i64;
    let eq = d.cap.equity_f64();
    let hrs = ((deadline - now) as f64 / 3.6e6).max(0.0);
    let scen: Vec<Value> = qe_math::barrier::scenarios(eq, target, hrs)
        .into_iter()
        .map(|x| json!({"trades_per_hour": x.trades_per_hour, "n_trades": x.n_trades, "required_return_per_trade": x.required_return}))
        .collect();
    let pnl: Vec<f64> = d
        .closed
        .iter()
        .filter_map(|c| c["net_pnl"].as_f64())
        .collect();
    let wins: Vec<f64> = pnl.iter().copied().filter(|x| *x > 0.0).collect();
    let losses: Vec<f64> = pnl.iter().copied().filter(|x| *x < 0.0).collect();
    let el_h = ((now - d.started_ms) as f64 / 3.6e6).max(1e-9);
    let eqs: Vec<f64> = d.equity_curve.iter().map(|x| x.1).collect();
    let r_needed_at = |r: f64| qe_math::barrier::trades_per_hour_needed(eq, target, r, hrs);
    json!({"stage": stage, "start_equity": start, "target_equity": target, "equity": eq, "deadline_ms": deadline,
        "hours_remaining": hrs, "growth_required": target / eq - 1.0,
        "log_growth_required": (target / eq).ln(), "scenarios": scen,
        "trades_per_hour_needed": {"r=0.5%": r_needed_at(0.005), "r=1%": r_needed_at(0.01), "r=2%": r_needed_at(0.02)},
        "actual_trades_per_hour": if pnl.is_empty() { None } else { Some(pnl.len() as f64 / el_h) },
        "avg_win": stats::mean(&wins), "avg_loss": stats::mean(&losses), "expectancy": stats::mean(&pnl),
        "geometric_growth": qe_math::target::geometric_growth(&eqs),
        "simulation": d.sim, "paper": true,
        "stage_name": format!("{:?}", d.cap.stage),
        "requirement": d.cap.required(now as u64),
        "reference_tables": {
            "stage1": qe_engine::frequency::Requirement::new(100.0, 3_000.0, 2.5),
            "stage2": qe_engine::frequency::Requirement::new(1_000.0, 50_000.0, 18.5)},
        "plan": d.cap.plan, "checkpoint": d.cap.checkpoint, "history": d.cap.history,
        "withdrawals": d.cap.withdrawals,
        "stage_trades": d.cap.stage_trades, "stage_wins": d.cap.stage_wins,
        "stage_realised_pnl": d.cap.stage_realised_pnl, "stage_fees": d.cap.stage_fees,
        "high_water_mark": d.cap.high_water_mark,
        "note": "a trade quota never forces an entry; every entry still needs governor approval"})
}

/// Seeded Monte-Carlo of the stage target from an estimated per-trade return
/// distribution: realised paper trades once >= 10 exist, otherwise the current
/// best candidates' model P(TP first) with returns at the governor's risk budget.
pub fn refresh_sim(d: &mut Desk, now: i64) {
    let t = target_json(d, now);
    let (eq, target, hrs) = (
        d.cap.equity_f64(),
        t["target_equity"].as_f64().unwrap_or(0.0),
        t["hours_remaining"].as_f64().unwrap_or(0.0),
    );
    let rf = d.gov.effective_risk_fraction();
    let rets: Vec<f64> = d
        .closed
        .iter()
        .filter_map(|c| {
            let pnl = c["net_pnl"].as_f64()?;
            Some(pnl / (c["equity_after"].as_f64()? - pnl))
        })
        .collect();
    let (p_win, win_ret, loss_ret, source) = if rets.len() >= 10 {
        let w: Vec<f64> = rets.iter().copied().filter(|x| *x > 0.0).collect();
        let l: Vec<f64> = rets
            .iter()
            .copied()
            .filter(|x| *x <= 0.0)
            .map(|x| -x)
            .collect();
        (
            w.len() as f64 / rets.len() as f64,
            stats::mean(&w).unwrap_or(0.0),
            stats::mean(&l).unwrap_or(rf),
            format!("{} realised paper trades", rets.len()),
        )
    } else {
        let ps: Vec<f64> = d
            .opportunities
            .iter()
            .take(20)
            .filter_map(|o| o["p_win"].as_f64())
            .collect();
        (
            stats::mean(&ps).unwrap_or(0.0),
            rf * K_TARGET / K_STOP,
            rf,
            "model P(TP first) of current candidates; win = risk*K_TARGET/K_STOP, loss = risk (fees inside the risk budget)".to_string(),
        )
    };
    let max_trades = ((hrs * 20.0) as usize).clamp(1, 2_000);
    let pol = &d.gov.policy;
    let r = qe_math::target::simulate(
        eq,
        target,
        pol.max_session_loss_fraction,
        eq * 0.5,
        p_win,
        win_ret,
        loss_ret,
        max_trades,
        5,
        2_000,
        42,
    );
    d.sim = Some(
        json!({"p_reach_target": r.p_reach_target, "p_session_loss_first": r.p_session_loss_first,
        "p_floor_breach": r.p_floor_breach, "p_losing_streak": r.p_losing_streak,
        "p_unrecovered_drawdown": r.p_unrecovered_drawdown, "median_final_equity": r.median_final_equity,
        "paths": r.paths, "max_trades": max_trades, "assumed_trades_per_hour": 20, "p_win": p_win,
        "win_ret": win_ret, "loss_ret": loss_ret, "seed": 42, "floor": eq * 0.5, "streak_len": 5,
        "source": source, "computed_ms": now}),
    );
}

/// Scanner coverage per universe: discovered, eligible (active, not exit-only, metadata
/// decoded) and actually subscribed. Only the shortlist gets order books and trades;
/// every futures instrument gets 1 Hz `currentPrices`, which is not tick coverage.
pub fn coverage_json(d: &Desk) -> Value {
    let elig = |m: &str| {
        d.instruments
            .iter()
            .filter(|(k, (_, i))| k.ends_with(&format!("|{m}")) && i.is_tradable())
            .count()
    };
    let now = now_ms();
    let books_live = d.ws_book_ms.values().filter(|t| now - **t < 10_000).count();
    json!({
        "spot": {"discovered": d.spot_details.len(), "active": d.spot_active,
                 "eligible": 0, "subscribed": 0,
                 "note": "spot is tracked for basis/fx only; not traded"},
        "usdt_futures": {"discovered": d.fut_usdt.len(), "eligible": elig("USDT"),
                 "subscribed_prices_1hz": d.fut_usdt.len()},
        "inr_futures": {"discovered": d.fut_inr.len(), "eligible": elig("INR"),
                 "metadata_rejected": d.meta_fail.keys().filter(|k| k.ends_with("|INR")).count(),
                 "metadata_cached": d.instruments.keys().filter(|k| k.ends_with("|INR")).count(),
                 "ineligible_null_leverage": d.instruments.iter()
                     .filter(|(k, (_, i))| k.ends_with("|INR") && !i.leverage_known()).count(),
                 "subscribed_prices_1hz": d.fut_inr.len()},
        "order_books_subscribed": d.ws_book_ms.len(), "order_books_fresh_10s": books_live,
        "trade_streams_subscribed": d.trades.len(),
        "tick_coverage": "shortlist only: order books and trades for the top candidates and open positions; all other instruments at 1 Hz prices",
    })
}

/// Execution quality for reports and the dashboard.
pub fn execution_json(d: &Desk) -> Value {
    let fm = &d.fill_model;
    json!({"paper": true, "fill_model": fm.params, "entries": fm.entries, "rejected": fm.rejected,
        "partial": fm.partial, "latency_decision_to_fill": d.latency.summary(),
        "note": "latency = measured decision->paper-fill time plus simulated exchange latency"})
}

/// Build a report of `kind` from the current desk state.
pub fn build_report(d: &Desk, kind: qe_engine::report::ReportKind, now: i64) -> String {
    use qe_engine::report::{render, ReportInput, TradeLine};
    let since = match kind {
        qe_engine::report::ReportKind::Stage2Hourly => now - 3_600_000,
        _ => 0,
    };
    let trades: Vec<TradeLine> = d
        .closed
        .iter()
        .filter(|c| c["closed_ms"].as_i64().unwrap_or(0) >= since)
        .map(|c| TradeLine {
            id: c["id"].as_str().unwrap_or("").into(),
            pair: c["pair"].as_str().unwrap_or("").into(),
            side: c["side"].as_str().unwrap_or("").into(),
            net_pnl_inr: c["net_pnl"].as_f64().unwrap_or(0.0),
            fees_inr: c["fees"].as_f64().unwrap_or(0.0),
            held_s: c["held_s"].as_f64().unwrap_or(0.0),
            exit_reason: c["exit_reason"].as_str().unwrap_or("").into(),
            paper: true,
        })
        .collect();
    let sim = simulate_target(d, now);
    render(&ReportInput {
        kind,
        generated_ms: now as u64,
        capital: &d.cap,
        trades: &trades,
        requirement: d.cap.required(now as u64),
        simulation: Some(&sim),
        forecasts: json!({"forward_monitor": d.calib.status()}),
        calibration: d.calibration_json(),
        learning: learning_json(d),
        risk_events: d.risk_events.iter().cloned().collect(),
        execution: execution_json(d),
        coverage: coverage_json(d),
    })
}

/// Stochastic target simulation (qe-engine) from measured paper trades when at least
/// 10 exist, otherwise from the current candidates' uncalibrated model probabilities.
pub fn simulate_target(d: &Desk, now: i64) -> qe_engine::frequency::SimSummary {
    let req = d.cap.required(now as u64);
    let rf = d.gov.effective_risk_fraction();
    let rets: Vec<f64> = d
        .closed
        .iter()
        .filter_map(|c| {
            let pnl = c["net_pnl"].as_f64()?;
            Some(pnl / (c["equity_after"].as_f64()? - pnl))
        })
        .collect();
    let (p_win, w, l) = if rets.len() >= 10 {
        let wins: Vec<f64> = rets.iter().copied().filter(|x| *x > 0.0).collect();
        let losses: Vec<f64> = rets
            .iter()
            .copied()
            .filter(|x| *x <= 0.0)
            .map(|x| -x)
            .collect();
        (
            wins.len() as f64 / rets.len() as f64,
            stats::mean(&wins).unwrap_or(0.0),
            stats::mean(&losses).unwrap_or(rf),
        )
    } else {
        let ps: Vec<f64> = d
            .opportunities
            .iter()
            .take(20)
            .filter_map(|o| o["p_win"].as_f64())
            .collect();
        (stats::mean(&ps).unwrap_or(0.0), rf * K_TARGET / K_STOP, rf)
    };
    let taken = d
        .decisions
        .iter()
        .filter(|x| x["decision"] == "taken")
        .count() as f64;
    let n_dec = d.decisions.len().max(1) as f64;
    let fm = &d.fill_model;
    qe_engine::frequency::simulate(&qe_engine::frequency::SimParams {
        c0: req.equity,
        target: req.target,
        hours: req.hours,
        windows_per_hour: 30,
        p_win,
        win_ret: w,
        loss_ret: l,
        magnitude_spread: 0.5,
        cost_ret: 0.0,
        funding_ret: 0.0,
        p_no_opportunity: (1.0 - taken / n_dec).clamp(0.0, 1.0),
        p_reject: fm.params.p_reject,
        p_partial: fm.params.p_partial,
        partial_fill_frac: (1.0 + fm.params.min_partial_frac) / 2.0,
        p_downtime: 0.02,
        session_loss_frac: d.gov.policy.max_session_loss_fraction,
        paths: 1_000,
        seed: 42,
    })
}

pub use crate::learn::learning_json;

/// Owner live-readiness / arm status (s3-live). Read-only local state, secret-free;
/// the desk itself never trades live.
pub fn live_json() -> Value {
    let dir = qe_live_check::state::StateDir::from_env();
    json!({"arm_status": qe_live_check::arming::arm_status(&dir, now_ms()),
        "desk_execution": "paper only: this desk build holds no private client",
        "note": "only the account owner arms live, with qe-live-check, after the validation workflows pass"})
}

/// Scheduled reports: 20:00 paper, hourly while trading toward the single INR 50,000
/// target, final at the 17:00 IST final checkpoint (then the plan completes). Written as
/// markdown under `<data dir>/reports/`.
pub fn report_tick(d: &mut Desk, now: i64) {
    use qe_engine::report::ReportKind as K;
    let mut due = vec![];
    if now >= d.settings.stage0_end_ms && !d.reports_done.contains("paper-8pm") {
        due.push((K::Paper8pm, "paper-8pm".to_string()));
    }
    let reconciled = d
        .cap
        .checkpoint
        .as_ref()
        .is_some_and(|c| c.balance.is_some());
    if d.cap.stage == Stage::Checkpoint1
        && reconciled
        && !d.reports_done.contains("stage1-checkpoint")
    {
        due.push((K::Stage1Checkpoint, "stage1-checkpoint".to_string()));
    }
    if matches!(d.cap.stage, Stage::Stage1Growth | Stage::Stage2Protective) {
        let h = (now - d.cap.stage_started_ms as i64) / 3_600_000;
        if h >= 1 && !d.reports_done.contains(&format!("stage2-h{h}")) {
            due.push((K::Stage2Hourly, format!("stage2-h{h}")));
        }
    }
    if d.cap.stage == Stage::FinalCheckpoint && reconciled && !d.reports_done.contains("final") {
        due.push((K::Final, "final".to_string()));
    }
    for (kind, key) in due {
        let md = build_report(d, kind, now);
        let dir = d.store.dir.join("reports");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{key}.md"));
        match std::fs::write(&path, md) {
            Ok(()) => d.audit("report", format!("wrote {}", path.display()), Value::Null),
            Err(e) => d.incident("report", format!("{}: {e}", path.display())),
        }
        d.reports_done.insert(key.clone());
        if kind == K::Final {
            if let Err(e) = d.cap.complete_final(now as u64) {
                d.incident("checkpoint", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(prices: &[(i64, f64)]) -> VecDeque<(i64, f64, f64)> {
        prices.iter().map(|(t, p)| (*t, *p, 0.0)).collect()
    }

    #[test]
    fn calibration_resolves_forward_only() {
        let mut c = Calib::default();
        c.maybe_record(
            "X",
            0,
            [(true, 101.0, 99.0, 0.4), (false, 99.0, 101.0, 0.4)],
        );
        let mut hist = HashMap::new();
        let path: Vec<(i64, f64)> = (1..=120)
            .map(|s| (s * 1000, 100.0 + s as f64 * 0.02))
            .collect();
        hist.insert("X".to_string(), h(&path));
        c.resolve(&hist, 119_000);
        assert_eq!(c.resolved.len(), 0, "not resolved before 120 s");
        c.resolve(&hist, 120_000);
        assert_eq!(c.resolved.len(), 2);
        // price rises 100 -> 102.4: long target (101) hit, short stop (101) hit
        assert_eq!(c.resolved[0], (0.4, true));
        assert_eq!(c.resolved[1], (0.4, false));
        assert!(!c.calibrated());
    }

    #[test]
    fn calibration_drops_gapped_paths() {
        let mut c = Calib::default();
        c.maybe_record(
            "X",
            0,
            [(true, 101.0, 99.0, 0.4), (false, 99.0, 101.0, 0.4)],
        );
        let mut hist = HashMap::new();
        hist.insert("X".to_string(), h(&[(10_000, 100.0), (119_000, 100.0)]));
        c.resolve(&hist, 130_000);
        assert_eq!(c.resolved.len(), 0);
        assert_eq!(c.dropped, 2);
    }

    #[test]
    fn regime_rules_flag_unstable_and_withdrawal() {
        let row = json!({"vol_1s": 0.0001, "z_ret_10s": 2.5, "ret_60s": 0.001, "z_volume": 0.0});
        let r = regimes(None, &row, Some(4.0), false);
        assert!(r.contains(&"unstable".to_string()));
        assert!(r.contains(&"liquidity_withdrawal".to_string()));
        assert!(r.contains(&"momentum_acceleration".to_string()));
    }

    fn book(bid: f64, ask: f64) -> Book {
        Book {
            ts: now_ms(),
            vs: None,
            bids: vec![(bid, 1000.0)],
            asks: vec![(ask, 1000.0)],
        }
    }

    fn order(side: Side) -> qe_risk::ApprovedOrder {
        qe_risk::ApprovedOrder {
            trace_id: "t1".into(),
            pair: "B-X_USDT".into(),
            side,
            qty: 10.0,
            leverage: 5.0,
            entry_price: 100.0,
            stop_loss: if side == Side::Long { 99.0 } else { 101.0 },
            take_profit: if side == Side::Long { 101.5 } else { 98.5 },
            notional_account: 1000.0,
            margin_account: 200.0,
            risk_budget: 10.0,
            risk_at_stop: 10.0,
            liquidation_price: None,
            risk_fraction_used: 0.01,
        }
    }

    fn cap_x() -> InstrumentCapability {
        InstrumentCapability {
            pair: "B-X_USDT".into(),
            margin_currency: "INR".into(),
            active: true,
            exit_only: false,
            max_leverage_long: 10.0,
            max_leverage_short: 10.0,
            tiers: vec![],
            min_notional: 1.0,
            min_quantity: 1.0,
            max_quantity: 1e9,
            quantity_increment: 1.0,
            price_increment: 0.01,
            taker_fee: 0.0005,
            maker_fee: 0.0002,
            maintenance_margin_rate: None,
            margin_modes: vec![],
            supports_market_order: true,
        }
    }

    fn desk_with(side: Side) -> Desk {
        std::env::set_var(
            "QE_DATA_DIR",
            std::env::temp_dir()
                .join("qe-desk-test")
                .to_string_lossy()
                .to_string(),
        );
        let mut d = Desk::new(Settings::default());
        d.books.insert("B-X_USDT".into(), book(99.95, 100.05));
        d.gov.open_positions = 1;
        let c = cap_x();
        open_paper(&mut d, order(side), c, 1.0, 100.0, Value::Null);
        d
    }

    #[test]
    fn paper_entry_fills_and_protects() {
        let d = desk_with(Side::Long);
        assert_eq!(d.positions.len(), 1);
        assert_eq!(d.positions[0].pos.state, State::Protected);
        assert_eq!(d.fills.len(), 1);
        // long lifts the ask plus slippage
        assert!(d.fills[0]["price"].as_f64().unwrap() > 100.05);
    }

    #[test]
    fn stop_hit_closes_and_books_loss() {
        let mut d = desk_with(Side::Long);
        d.books.insert("B-X_USDT".into(), book(98.9, 99.0));
        lifecycle_tick(&mut d);
        assert!(d.positions.is_empty());
        assert_eq!(d.closed.len(), 1);
        assert_eq!(d.closed[0]["exit_reason"], "sl");
        assert!(d.closed[0]["net_pnl"].as_f64().unwrap() < 0.0);
        assert!(d.cap.equity_f64() < 100.0);
        assert_eq!(d.gov.open_positions, 0);
        assert_eq!(d.gov.consecutive_losses, 1);
    }

    #[test]
    fn time_stop_fires_before_120s() {
        let mut d = desk_with(Side::Short);
        // age the position to 117.5 s (exit lead 3 s)
        let opened = d.positions[0].pos.opened_at_ms.unwrap();
        d.positions[0].pos.opened_at_ms = Some(opened - 117_500);
        lifecycle_tick(&mut d);
        assert!(d.positions.is_empty());
        assert_eq!(d.closed[0]["exit_reason"], "time_stop");
        assert!(d.closed[0]["held_s"].as_f64().unwrap() < 120.0);
    }

    #[test]
    fn flatten_closes_everything() {
        let mut d = desk_with(Side::Long);
        let n = flatten(&mut d, "test");
        assert!(n > 0);
        assert!(d.positions.is_empty());
        assert_eq!(d.closed[0]["exit_reason"], "flatten");
    }

    #[test]
    fn no_registry_means_no_authority() {
        let mut d = Desk::new(Settings::default());
        std::env::set_var("QE_CALIBRATION_FILE", "/nonexistent/calibration.json");
        d.load_calibration();
        std::env::remove_var("QE_CALIBRATION_FILE");
        assert!(d.calib_report_err.is_some());
        assert!(!d.authority(Side::Long) && !d.authority(Side::Short));
        assert_eq!(d.calibration_json()["calibrated"], false);
    }

    #[test]
    fn committed_registry_denies_desk_model() {
        let mut d = Desk::new(Settings::default());
        d.load_calibration();
        assert!(d.calib_report_err.is_none(), "{:?}", d.calib_report_err);
        // s2-calib measured: desk.fp.v1 is not calibrated
        assert!(!d.authority(Side::Long) && !d.authority(Side::Short));
    }

    #[test]
    fn capability_maps_live_metadata() {
        let j = r#"{"pair":"B-DOGE_USDT","status":"active","margin_currency_short_name":"INR","settle_currency_short_name":"USDT","quote_currency_short_name":"USDT","underlying_currency_short_name":"DOGE","max_leverage_long":10.0,"max_leverage_short":10.0,"price_increment":1.0e-05,"quantity_increment":1.0,"min_quantity":1.0,"max_quantity":285000000.0,"min_notional":6.0,"maker_fee":0.0236,"taker_fee":0.059,"safety_percentage":1.0,"max_market_order_quantity":30000000.0,"max_notional":1760000.0,"exit_only":false,"order_types":["limit_order","market_order"],"time_in_force_options":[],"dynamic_position_leverage_details":{"10":5850000,"100":10000},"dynamic_safety_margin_details":{"10000":0.5,"325000":1}}"#;
        let i: FuturesInstrument = serde_json::from_str(j).unwrap();
        let c = capability(&i);
        assert_eq!(c.max_quantity, 30_000_000.0);
        assert!((c.taker_fee - 0.00059).abs() < 1e-12);
        assert_eq!(c.maintenance_margin_rate, Some(0.01));
        assert!(c.supports_market_order);
        // exchange max for side caps the 100x tier
        assert_eq!(
            qe_risk::max_permitted_leverage(&c, Side::Long, 5000.0, f64::INFINITY),
            10.0
        );
    }

    #[test]
    fn experiment_guard_refuses_anything_but_paper() {
        assert!(experiment_guard(Some("paper"), Some("paper"), None).is_ok());
        assert!(experiment_guard(Some("paper"), None, Some("paper")).is_ok());
        assert!(experiment_guard(Some("live"), Some("paper"), None).is_err());
        assert!(experiment_guard(None, Some("paper"), None).is_err());
        assert!(experiment_guard(Some("paper"), Some("live"), None).is_err());
        assert!(experiment_guard(Some("paper"), Some("paper"), Some("live")).is_err());
    }

    #[test]
    fn experiment_policy_only_when_opted_in_and_paper() {
        std::env::set_var(
            "QE_DATA_DIR",
            std::env::temp_dir()
                .join("qe-desk-test")
                .to_string_lossy()
                .to_string(),
        );
        let d = Desk::new(Settings::default());
        assert!(!d.gov.policy.is_experiment());
        assert_eq!(profile_json(&d)["banner"], Value::Null);
        let s = Settings {
            profile: EXPERIMENT_PROFILE.into(),
            ..Settings::default()
        };
        let d = Desk::new(s.clone());
        assert!(d.gov.policy.is_experiment());
        assert_eq!(profile_json(&d)["banner"], EXPERIMENT_BANNER);
        let d = Desk::new(Settings {
            execution_mode: "live".into(),
            ..s
        });
        assert!(!d.gov.policy.is_experiment());
    }

    #[test]
    fn experiment_liquidation_closes_with_margin_lost() {
        let mut d = desk_with(Side::Long);
        d.gov.policy = RiskPolicy::experiment_max_leverage();
        // order(): margin 200 at 5x; liquidation estimated at 99.5 (inside the 99 stop)
        d.positions[0].pos.order.liquidation_price = Some(99.5);
        d.books.insert("B-X_USDT".into(), book(99.4, 99.45));
        lifecycle_tick(&mut d);
        assert!(d.positions.is_empty());
        assert_eq!(d.closed[0]["exit_reason"], "liquidated");
        // equity 100 < margin 200 + fee: clamped at zero, never negative
        assert_eq!(d.cap.equity_f64(), 0.0);
        assert!(
            d.gov.entries_paused,
            "experiment stops below the tradable minimum"
        );
    }

    fn gamble_desk(side: Side) -> Desk {
        let mut d = desk_with(side);
        let raw = qe_risk::gamble::RawGamble {
            enabled: Some(true),
            owner_ack: Some(qe_risk::gamble::OWNER_ACK.into()),
            stake_inr: Some("10".into()),
            leverage: Some("10".into()),
            floor_inr: Some("50".into()),
            stake_policy: None,
        };
        d.gov.policy = RiskPolicy::gamble(qe_risk::gamble::validate(&raw, 100.0).unwrap());
        d
    }

    #[test]
    fn gamble_bet_exits_on_120s_and_records_mode() {
        let mut d = gamble_desk(Side::Long);
        let opened = d.positions[0].pos.opened_at_ms.unwrap();
        d.positions[0].pos.opened_at_ms = Some(opened - 117_500);
        lifecycle_tick(&mut d);
        assert!(d.positions.is_empty());
        assert_eq!(d.closed[0]["exit_reason"], "time_stop");
        assert_eq!(d.closed[0]["mode"], "gamble");
        assert_eq!(gamble_json(&d)["trades"], 1);
        assert_eq!(profile_json(&d)["banner"], GAMBLE_BANNER);
    }

    #[test]
    fn gamble_kill_switch_flattens_and_blocks() {
        let mut d = gamble_desk(Side::Short);
        d.gov.kill_switch(0);
        flatten(&mut d, "kill switch");
        assert!(d.positions.is_empty());
        assert_eq!(d.closed[0]["exit_reason"], "flatten");
        assert_eq!(gamble_json(&d)["enabled"], false);
        assert_eq!(gamble_json(&d)["stopped_reason"], "kill switch engaged");
    }

    #[test]
    fn gamble_floor_stop_fires_after_a_loss_to_the_floor() {
        let mut d = gamble_desk(Side::Long);
        // order() margin 200 vs equity 100: a stop-out books a loss; then equity
        // is forced to the floor to check the permanent stop
        d.books.insert("B-X_USDT".into(), book(98.9, 99.0));
        lifecycle_tick(&mut d);
        assert!(d.gov.gamble_stopped.is_none(), "still above the floor");
        d.cap.on_realised(dec(50.0 - d.cap.equity_f64()), dec(0.0));
        gamble_stop_check(&mut d);
        assert!(d.gov.gamble_stopped.is_some());
        assert!(d.gov.entries_paused);
        assert!(!d.gov.pause_entries(false, 0).ok);
        assert!(d.store.dir.join(GAMBLE_STOP_FILE).exists());
        let _ = std::fs::remove_file(d.store.dir.join(GAMBLE_STOP_FILE));
    }

    fn trade(ts_ms: i64, price: f64, qty: f64) -> timeframe::Trade {
        timeframe::Trade {
            ts_ms,
            price,
            qty,
            buyer_aggressor: None,
        }
    }

    fn maker_desk(side: Side) -> Desk {
        let mut d = desk_with(Side::Long);
        d.positions.clear();
        d.gov.policy = gamble_desk(side).gov.policy;
        d.settings.gamble_maker_entry = true;
        d.settings.maker_timeout_ms = 60_000;
        open_paper(&mut d, order(side), cap_x(), 1.0, 100.0, Value::Null);
        d
    }

    #[test]
    fn maker_entry_rests_until_trades_print_through() {
        let mut d = maker_desk(Side::Long);
        let pair = d.positions[0].pos.order.pair.clone();
        let px = d.positions[0].maker.as_ref().unwrap().price;
        let qty = d.positions[0].pos.order.qty;
        assert_eq!(d.positions[0].pos.state, State::EntrySubmitted);
        assert_eq!(d.maker_stats.placed, 1);
        // a print AT the price does not fill (queue position unknown)
        d.trades
            .entry(pair.clone())
            .or_default()
            .push_back(trade(now_ms(), px, qty * 10.0));
        lifecycle_tick(&mut d);
        assert!(d.positions[0].maker.is_some());
        // prints through the price fill at the limit with the maker fee
        d.trades
            .entry(pair)
            .or_default()
            .push_back(trade(now_ms(), px * 0.999, qty));
        lifecycle_tick(&mut d);
        let p = &d.positions[0];
        assert!(p.maker.is_none());
        assert_eq!(p.pos.state, State::Protected);
        assert_eq!(p.pos.avg_entry, px);
        assert!((p.pos.entry_fees - qty * px * p.maker_fee).abs() < 1e-12);
        assert_eq!(d.fills[0]["liquidity"], "maker");
        assert_eq!(d.maker_stats.filled, 1);
    }

    #[test]
    fn maker_entry_times_out_and_is_cancelled_without_a_chase() {
        let mut d = maker_desk(Side::Short);
        let px = d.positions[0].maker.as_ref().unwrap().price;
        let pair = d.positions[0].pos.order.pair.clone();
        // a print below a sell limit is not through it
        d.trades
            .entry(pair)
            .or_default()
            .push_back(trade(now_ms(), px * 0.99, 1e9));
        d.positions[0].maker.as_mut().unwrap().deadline_ms = 0;
        let fills = d.fills.len();
        lifecycle_tick(&mut d);
        assert!(d.positions.is_empty(), "rejected entry swept");
        assert_eq!(d.fills.len(), fills, "no taker fill");
        assert_eq!(d.maker_stats.cancelled, 1);
        assert_eq!(gamble_json(&d)["maker"]["fill_rate"], 0.0);
    }

    #[test]
    fn flatten_cancels_a_resting_maker_entry() {
        let mut d = maker_desk(Side::Long);
        flatten(&mut d, "test");
        assert!(d.positions.is_empty());
        assert_eq!(d.maker_stats.cancelled, 1);
    }

    #[test]
    fn range_120s_is_the_median_bucket_range() {
        let mut h = VecDeque::new();
        // three 120 s buckets with ranges 1%, 2%, 4% of ~100
        for (k, amp) in [(0i64, 4.0), (1, 2.0), (2, 1.0)] {
            for i in 0..60i64 {
                let t = 1_000_000 - k * 120_000 - i * 2_000;
                let p = 100.0 + if i % 2 == 0 { amp / 2.0 } else { -amp / 2.0 };
                h.push_front((t, p, 0.0));
            }
        }
        let r = range_120s_frac(&h).unwrap();
        assert!((r - 0.02).abs() < 1e-9, "{r}");
        assert_eq!(range_120s_frac(&VecDeque::new()), None);
    }

    #[test]
    fn iso_utc_formats_the_deadline() {
        assert_eq!(iso_utc(1_795_519_800_000), "2026-11-24T11:30:00Z");
        assert_eq!(gamble_json(&Desk::new(Settings::default())), Value::Null);
    }
}
