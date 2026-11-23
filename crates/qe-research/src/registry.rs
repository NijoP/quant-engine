//! Champion/challenger model registry (`models.json`), produced offline by
//! `qe-research challenge` and loaded by s3-learn / s3-desk.
//!
//! # API (narrow, stable)
//! ```no_run
//! use qe_research::registry::ModelRegistry;
//! // missing or unreadable file => only the champion, nothing promotable
//! let r = ModelRegistry::load_or_default(std::path::Path::new("models.json"));
//! let champ = r.champion();            // id of the model the desk runs (advisory)
//! let ok = r.is_promotable("logit.flow.v2"); // passed every predefined OOS gate
//! if let Some(card) = r.model("logit.flow.v2") {
//!     let _ = (&card.version, &card.features, &card.calibration, &card.limitations);
//! }
//! ```
//! Promotion is necessary, never sufficient: a promotable model still has no
//! authority over the risk governor, and `rollback` names the version to return to.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const SCHEMA: &str = "qe.model-registry.v1";
pub const DEFAULT_CHAMPION: &str = "desk.fp.v1";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FeatureDef {
    pub name: String,
    pub definition: String,
}

/// Out-of-sample probability scores for one target (walk-forward folds 1..k).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct TargetScore {
    pub n_oos: usize,
    pub oos_folds: usize,
    pub base_rate: f64,
    pub mean_p: f64,
    pub brier: f64,
    pub brier_clim: f64,
    /// 1 - brier / brier_clim (climatology = training-fold base rate)
    pub skill: f64,
    pub ece: f64,
    pub log_loss: f64,
    pub folds_positive_skill: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCard {
    pub id: String,
    pub version: String,
    /// champion | challenger
    pub role: String,
    pub kind: String,
    pub params: serde_json::Value,
    pub training_window: serde_json::Value,
    pub validation_window: serde_json::Value,
    pub features: Vec<FeatureDef>,
    /// target -> OOS scores
    pub calibration: BTreeMap<String, TargetScore>,
    /// cost scenario -> trading results (bps of notional per trade)
    pub trading: serde_json::Value,
    pub assumptions: Vec<String>,
    pub limitations: Vec<String>,
    pub promotable: bool,
    pub promotion_failures: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRegistry {
    pub schema: String,
    pub generated_ms: i64,
    pub champion: String,
    pub champion_reason: String,
    /// version to roll back to if a promoted model degrades
    pub rollback: String,
    pub promotion_criteria: Vec<String>,
    pub data: serde_json::Value,
    pub models: Vec<ModelCard>,
    pub promotion_history: Vec<serde_json::Value>,
    /// full parameter/threshold grid (selection-biased; information only)
    pub grid: serde_json::Value,
}

impl Default for ModelRegistry {
    fn default() -> Self {
        ModelRegistry {
            schema: SCHEMA.into(),
            generated_ms: 0,
            champion: DEFAULT_CHAMPION.into(),
            champion_reason: "no registry loaded".into(),
            rollback: DEFAULT_CHAMPION.into(),
            promotion_criteria: vec![],
            data: serde_json::Value::Null,
            models: vec![],
            promotion_history: vec![],
            grid: serde_json::Value::Null,
        }
    }
}

impl ModelRegistry {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let r: ModelRegistry = serde_json::from_slice(&std::fs::read(path)?)?;
        anyhow::ensure!(
            r.schema == SCHEMA,
            "unexpected registry schema {}",
            r.schema
        );
        Ok(r)
    }
    pub fn load_or_default(path: &std::path::Path) -> Self {
        Self::load(path).unwrap_or_default()
    }
    pub fn champion(&self) -> &str {
        &self.champion
    }
    pub fn model(&self, id: &str) -> Option<&ModelCard> {
        self.models.iter().find(|m| m.id == id)
    }
    pub fn is_promotable(&self, id: &str) -> bool {
        self.model(id).is_some_and(|m| m.promotable)
    }
    pub fn promotable(&self) -> impl Iterator<Item = &ModelCard> {
        self.models.iter().filter(|m| m.promotable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_registry_is_champion_only() {
        let r = ModelRegistry::load_or_default(std::path::Path::new("/nonexistent/models.json"));
        assert_eq!(r.champion(), DEFAULT_CHAMPION);
        assert!(!r.is_promotable("logit.flow.v2"));
        assert_eq!(r.promotable().count(), 0);
    }

    #[test]
    fn committed_registry_parses() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("models.json");
        if p.exists() {
            let r = ModelRegistry::load(&p).unwrap();
            assert!(r.model(r.champion()).is_some());
            // every promotable card must have no recorded gate failure
            assert!(r.promotable().all(|m| m.promotion_failures.is_empty()));
        }
    }
}
