//! qe-learn: the learn-from-mistakes engine (intent §6.1–6.6).
//!
//! Pure library: no network, no credentials, no orders. It records, measures,
//! classifies and reports; it can never raise leverage, size or risk, or
//! remove a stop (see [`safety`]). The risk governor remains the authority.
//!
//! # API for s3-desk (narrow, stable)
//! ```no_run
//! use qe_learn::{EngineConfig, LearningEngine, Filter, Mode, MarketObs};
//! # fn snapshot() -> qe_learn::DecisionSnapshot { unimplemented!() }
//! # fn outcome() -> qe_learn::TradeOutcome { unimplemented!() }
//! let mut learn = LearningEngine::open(EngineConfig::default(),
//!     std::path::Path::new("data/learn.jsonl")).unwrap(); // replays + verifies
//! // 1. BEFORE sending any order (approved or rejected proposal):
//! let receipt = learn.record_decision(snapshot()).unwrap();
//! // 2. after the position is closed AND every horizon (<= 120 s) has elapsed:
//! let rec = learn.record_outcome(outcome()).unwrap();     // derives P&L/costs, classifies
//! let why = learn.classify(&rec.outcome.trade_id).unwrap().clone(); // §6.3 MistakeReport
//! let lesson = learn.lesson(&rec.outcome.trade_id).unwrap(); // §6.5 D
//! // 3. per tick / per book snapshot, for regime tracking:
//! learn.observe_market("B-BTC_USDT", 1_700_000_000_000,
//!     MarketObs { spread_bps: Some(1.2), ..Default::default() }).unwrap();
//! let sig = learn.signal("B-BTC_USDT"); // shrink-only: multipliers in [0, 1]
//! // 4. dashboard JSON (always pass a mode: paper and live never mix silently)
//! let dash = learn.dashboard(i64::MAX, Some(Mode::Paper));
//! let json = serde_json::to_value(&dash).unwrap();
//! let m = learn.metrics(i64::MAX, &Filter { mode: Some(Mode::Paper), ..Default::default() });
//! // 5. models: learn.registry_mut().register / add_eval / promote / rollback
//! # let _ = (receipt, why, lesson, sig, json, m);
//! ```
//! Money (prices, quantities, fees, P&L, leverage, budgets) is `rust_decimal::Decimal`;
//! statistics (returns, probabilities, errors) are `f64`. Every output type is
//! `serde`-serialisable. Rules and thresholds: docs/LEARNING_ENGINE.md.
pub mod calibration;
pub mod classify;
pub mod engine;
pub mod lesson;
pub mod metrics;
pub mod outcome;
pub mod regime;
pub mod registry;
pub mod safety;
pub mod snapshot;
pub mod types;

pub use classify::{Cause, Certainty, ClassifierRules, Finding, MistakeReport, Verdict};
pub use engine::{DecisionReceipt, EngineConfig, LearningDashboard, LearningEngine, MarketObs};
pub use lesson::LessonReport;
pub use metrics::{Filter, MetricsReport};
pub use outcome::{ExitReason, Fill, HorizonMove, OutcomeFlags, OutcomeRecord, TradeOutcome};
pub use registry::{EvalKind, EvalResult, ModelSpec, PromotionCriteria, Registry};
pub use safety::{vet_adjustment, AdjustmentRequest, LearningSignal, Motive, RiskParams};
pub use snapshot::{DataQuality, DecisionSnapshot, HorizonForecast, RiskDecision};
pub use types::{Direction, LearnError, Mode, HORIZONS_S, LIFETIME_MS};
