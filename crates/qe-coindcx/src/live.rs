//! Live-execution gate and readiness checklist.
//!
//! # Narrow API (for qe-desk)
//! * [`LiveGate::from_env`] builds the gate at startup (never reads secrets
//!   itself; pass `CredentialStatus::configured`).
//! * [`LiveGate::check`] / [`LiveGate::enabled`]: every order-mutating private
//!   call goes through `check()` first (see `private::PrivateClient`).
//! * [`LiveGate::status`]: JSON-serialisable, secret-free state for the dashboard.
//!
//! Live order submission needs ALL of:
//! 1. the `live-orders` cargo feature compiled in;
//! 2. `QE_EXECUTION_MODE=live` (default and every unknown value: `paper`);
//! 3. `QE_LIVE_OPERATOR_CONFIRM` equal to [`OPERATOR_CONFIRM_PHRASE`], set by the
//!    account owner by hand;
//! 4. credentials configured (env or 0600 file);
//! 5. every [`CheckItem`] in the readiness checklist `Pass`.
//!
//! Items 5 "protective orders verified" and "120 s closure tested" cannot be
//! established by read-only calls, so in this build they stay `NotRun` and the
//! gate stays closed. No schedule, target controller or prompt can open it.
use serde::Serialize;
use std::collections::BTreeMap;

pub const ENV_EXECUTION_MODE: &str = "QE_EXECUTION_MODE";
pub const ENV_OPERATOR_CONFIRM: &str = "QE_LIVE_OPERATOR_CONFIRM";
pub const OPERATOR_CONFIRM_PHRASE: &str = "I-AM-THE-ACCOUNT-OWNER-AND-ACCEPT-LIVE-TRADING-RISK";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    #[default]
    Paper,
    Live,
}

impl ExecutionMode {
    /// Only the exact string "live" selects live; anything else is paper.
    pub fn parse(s: &str) -> Self {
        if s.trim() == "live" {
            Self::Live
        } else {
            Self::Paper
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckItem {
    CredentialsConfigured,
    SigningSelfTest,
    AccountReconciled,
    InstrumentRequirementsVerified,
    ProtectiveOrdersVerified,
    Closure120sTested,
    RiskGovernorAcceptance,
}

impl CheckItem {
    pub const ALL: [CheckItem; 7] = [
        CheckItem::CredentialsConfigured,
        CheckItem::SigningSelfTest,
        CheckItem::AccountReconciled,
        CheckItem::InstrumentRequirementsVerified,
        CheckItem::ProtectiveOrdersVerified,
        CheckItem::Closure120sTested,
        CheckItem::RiskGovernorAcceptance,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum CheckStatus {
    Pass(String),
    Fail(String),
    NotRun(String),
}

impl CheckStatus {
    pub fn is_pass(&self) -> bool {
        matches!(self, CheckStatus::Pass(_))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Readiness {
    pub items: BTreeMap<CheckItem, CheckStatus>,
}

impl Default for Readiness {
    fn default() -> Self {
        let items = CheckItem::ALL
            .iter()
            .map(|i| (*i, CheckStatus::NotRun("not run".into())))
            .collect();
        Self { items }
    }
}

impl Readiness {
    pub fn set(&mut self, item: CheckItem, status: CheckStatus) {
        self.items.insert(item, status);
    }
    pub fn failing(&self) -> Vec<CheckItem> {
        self.items
            .iter()
            .filter(|(_, s)| !s.is_pass())
            .map(|(i, _)| *i)
            .collect()
    }
    pub fn all_pass(&self) -> bool {
        self.failing().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", content = "items", rename_all = "snake_case")]
pub enum GateError {
    FeatureNotCompiled,
    ModeIsPaper,
    OperatorConfirmationMissing,
    CredentialsMissing,
    ReadinessIncomplete(Vec<CheckItem>),
    /// Kept for the sprint-1 API: submission paths that are not validated.
    NotValidatedInThisBuild,
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "live gate closed: {self:?}")
    }
}

/// Live-execution gate. `Default` is fully closed (paper mode).
#[derive(Debug, Clone, Default)]
pub struct LiveGate {
    pub mode: ExecutionMode,
    pub operator_confirmed: bool,
    pub credentials_configured: bool,
    pub readiness: Readiness,
}

#[derive(Debug, Clone, Serialize)]
pub struct GateStatus {
    pub compiled: bool,
    pub execution_mode: ExecutionMode,
    pub operator_confirmed: bool,
    pub credentials_configured: bool,
    pub enabled: bool,
    pub blocked_by: Option<GateError>,
    pub readiness: Readiness,
}

impl LiveGate {
    pub const COMPILED: bool = cfg!(feature = "live-orders");

    /// Read mode and operator confirmation from the environment.
    pub fn from_env(credentials_configured: bool) -> Self {
        Self::from_lookup(|k| std::env::var(k).ok(), credentials_configured)
    }

    pub fn from_lookup(env: impl Fn(&str) -> Option<String>, credentials_configured: bool) -> Self {
        let mut readiness = Readiness::default();
        readiness.set(
            CheckItem::CredentialsConfigured,
            if credentials_configured {
                CheckStatus::Pass("configured".into())
            } else {
                CheckStatus::Fail("not configured".into())
            },
        );
        Self {
            mode: env(ENV_EXECUTION_MODE)
                .map(|s| ExecutionMode::parse(&s))
                .unwrap_or_default(),
            operator_confirmed: env(ENV_OPERATOR_CONFIRM).as_deref()
                == Some(OPERATOR_CONFIRM_PHRASE),
            credentials_configured,
            readiness,
        }
    }

    pub fn check(&self) -> Result<(), GateError> {
        self.check_with(Self::COMPILED)
    }

    fn check_with(&self, compiled: bool) -> Result<(), GateError> {
        if !compiled {
            return Err(GateError::FeatureNotCompiled);
        }
        if self.mode != ExecutionMode::Live {
            return Err(GateError::ModeIsPaper);
        }
        if !self.operator_confirmed {
            return Err(GateError::OperatorConfirmationMissing);
        }
        if !self.credentials_configured {
            return Err(GateError::CredentialsMissing);
        }
        let failing = self.readiness.failing();
        if !failing.is_empty() {
            return Err(GateError::ReadinessIncomplete(failing));
        }
        Ok(())
    }

    pub fn enabled(&self) -> bool {
        self.check().is_ok()
    }

    pub fn status(&self) -> GateStatus {
        let blocked_by = self.check().err();
        GateStatus {
            compiled: Self::COMPILED,
            execution_mode: self.mode,
            operator_confirmed: self.operator_confirmed,
            credentials_configured: self.credentials_configured,
            enabled: blocked_by.is_none(),
            blocked_by,
            readiness: self.readiness.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    fn all_pass(g: &mut LiveGate) {
        for i in CheckItem::ALL {
            g.readiness.set(i, CheckStatus::Pass("test".into()));
        }
    }

    #[test]
    fn default_is_closed_paper() {
        let g = LiveGate::default();
        assert!(!g.enabled());
        assert_eq!(g.mode, ExecutionMode::Paper);
        assert!(g.status().blocked_by.is_some());
    }

    #[test]
    fn each_condition_is_required() {
        let live = [
            (ENV_EXECUTION_MODE, "live"),
            (ENV_OPERATOR_CONFIRM, OPERATOR_CONFIRM_PHRASE),
        ];
        let mut g = LiveGate::from_lookup(env_of(&live), true);
        all_pass(&mut g);
        assert_eq!(g.check_with(true), Ok(()));
        assert_eq!(g.check_with(false), Err(GateError::FeatureNotCompiled));

        let mut p = g.clone();
        p.mode = ExecutionMode::Paper;
        assert_eq!(p.check_with(true), Err(GateError::ModeIsPaper));

        let wrong = [(ENV_EXECUTION_MODE, "live"), (ENV_OPERATOR_CONFIRM, "yes")];
        let mut w = LiveGate::from_lookup(env_of(&wrong), true);
        all_pass(&mut w);
        assert_eq!(
            w.check_with(true),
            Err(GateError::OperatorConfirmationMissing)
        );

        let mut c = g.clone();
        c.credentials_configured = false;
        assert_eq!(c.check_with(true), Err(GateError::CredentialsMissing));

        let mut r = g.clone();
        r.readiness.set(
            CheckItem::Closure120sTested,
            CheckStatus::NotRun("x".into()),
        );
        assert_eq!(
            r.check_with(true),
            Err(GateError::ReadinessIncomplete(vec![
                CheckItem::Closure120sTested
            ]))
        );
    }

    #[test]
    fn unknown_mode_is_paper_and_creds_item_tracks_status() {
        assert_eq!(ExecutionMode::parse("LIVE"), ExecutionMode::Paper);
        assert_eq!(ExecutionMode::parse(" live "), ExecutionMode::Live);
        let g = LiveGate::from_lookup(|_| None, false);
        assert!(!g.readiness.items[&CheckItem::CredentialsConfigured].is_pass());
        let s = serde_json::to_value(g.status()).unwrap();
        assert_eq!(s["execution_mode"], "paper");
        assert_eq!(s["enabled"], false);
    }
}
