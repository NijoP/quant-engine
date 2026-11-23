//! Live-execution gate and readiness checklist.
//!
//! # Narrow API (for qe-desk)
//! * [`LiveGate::from_env`] builds the gate at startup (never reads secrets
//!   itself; pass `CredentialStatus::configured`).
//! * [`LiveGate::check`] / [`LiveGate::enabled`]: every order-mutating private
//!   call goes through `check()` first (see `private::PrivateClient`).
//! * [`LiveGate::check_for`]: the same with a [`Purpose`]. Risk-REDUCING calls
//!   (cancel, exit, cancel-all) need only conditions 1-4 below, so a disarm or
//!   kill never strands an open position; risk-OPENING calls need everything.
//! * [`LiveGate::status`]: JSON-serialisable, secret-free state for the dashboard.
//! * Arming and the kill switch are persisted by `qe-live-check`
//!   (`qe_live_check::arming::gate_for_desk`), which sets [`LiveGate::armed`].
//!
//! Live order submission (risk-opening) needs ALL of:
//! 1. the `live-orders` cargo feature compiled in;
//! 2. `QE_EXECUTION_MODE=live` (default and every unknown value: `paper`);
//! 3. `QE_LIVE_OPERATOR_CONFIRM` equal to [`OPERATOR_CONFIRM_PHRASE`], set by the
//!    account owner by hand;
//! 4. credentials configured (env or 0600 file);
//! 5. every [`CheckItem`] in the readiness checklist `Pass` (recently: the
//!    arming tool drops stale results to `NotRun`);
//! 6. [`LiveGate::armed`]: the owner armed live with typed confirmation and
//!    no kill switch is set.
//!
//! "Protective orders verified" and "120 s closure tested" pass only after the
//! owner runs `qe-live-check validate` (an [`OwnerSession::Validation`] gate,
//! which may open risk only for that supervised minimum-size test). No
//! schedule, target controller, agent or prompt can arm the gate.
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// What a mutating call does to exposure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// Leverage update, new order, attaching TP/SL.
    OpenRisk,
    /// Cancel, exit, cancel-all: never blocked by disarm / kill switch.
    ReduceRisk,
}

/// A gate scoped to one owner-run `qe-live-check` command; never built by the desk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerSession {
    /// Supervised minimum-size protective-order validation: may open risk once
    /// credentials, signing, flat account and instrument checks pass.
    Validation,
    /// Campaign checkpoint (23:00 IST; earlier 22:30 / 22:00 superseded): risk-reducing calls only.
    Checkpoint,
}

/// Items an owner validation session needs before it may open risk.
pub const VALIDATION_PREREQUISITES: [CheckItem; 4] = [
    CheckItem::CredentialsConfigured,
    CheckItem::SigningSelfTest,
    CheckItem::AccountReconciled,
    CheckItem::InstrumentRequirementsVerified,
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", content = "items", rename_all = "snake_case")]
pub enum GateError {
    /// Live has not been armed by the owner (or the kill switch is set).
    NotArmed,
    /// A checkpoint session may only reduce risk.
    SessionCannotOpenRisk,
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
    /// Set only by `qe_live_check::arming` from a valid, unexpired arm record
    /// with no kill switch. `false` by default and from `from_env`.
    pub armed: bool,
    /// Set only by the owner-run `qe-live-check` binary.
    pub owner_session: Option<OwnerSession>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GateStatus {
    pub armed: bool,
    pub owner_session: Option<OwnerSession>,
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
            armed: false,
            owner_session: None,
        }
    }

    /// Gate for an owner-run `qe-live-check` session. The caller must already
    /// have obtained the owner's typed confirmation; readiness items are set
    /// by the session's own checks.
    pub fn owner_session(session: OwnerSession, credentials_configured: bool) -> Self {
        let mut g = Self::from_lookup(|_| None, credentials_configured);
        g.owner_session = Some(session);
        g
    }

    /// Risk-opening check (the strictest). Kept for the sprint-2 API.
    pub fn check(&self) -> Result<(), GateError> {
        self.check_for(Purpose::OpenRisk)
    }

    pub fn check_for(&self, purpose: Purpose) -> Result<(), GateError> {
        self.check_with(Self::COMPILED, purpose)
    }

    fn check_with(&self, compiled: bool, purpose: Purpose) -> Result<(), GateError> {
        if !compiled {
            return Err(GateError::FeatureNotCompiled);
        }
        if !self.credentials_configured {
            return Err(GateError::CredentialsMissing);
        }
        match (self.owner_session, purpose) {
            (Some(_), Purpose::ReduceRisk) => return Ok(()),
            (Some(OwnerSession::Checkpoint), Purpose::OpenRisk) => {
                return Err(GateError::SessionCannotOpenRisk)
            }
            (Some(OwnerSession::Validation), Purpose::OpenRisk) => {
                let failing: Vec<CheckItem> = VALIDATION_PREREQUISITES
                    .iter()
                    .copied()
                    .filter(|i| !self.readiness.items.get(i).is_some_and(|s| s.is_pass()))
                    .collect();
                return if failing.is_empty() {
                    Ok(())
                } else {
                    Err(GateError::ReadinessIncomplete(failing))
                };
            }
            (None, _) => {}
        }
        if self.mode != ExecutionMode::Live {
            return Err(GateError::ModeIsPaper);
        }
        if !self.operator_confirmed {
            return Err(GateError::OperatorConfirmationMissing);
        }
        if purpose == Purpose::ReduceRisk {
            return Ok(());
        }
        if !self.armed {
            return Err(GateError::NotArmed);
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
            armed: self.armed,
            owner_session: self.owner_session,
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
        g.armed = true;
    }

    const O: Purpose = Purpose::OpenRisk;
    const R: Purpose = Purpose::ReduceRisk;

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
        assert_eq!(g.check_with(true, O), Ok(()));
        assert_eq!(g.check_with(false, O), Err(GateError::FeatureNotCompiled));

        let mut p = g.clone();
        p.mode = ExecutionMode::Paper;
        assert_eq!(p.check_with(true, O), Err(GateError::ModeIsPaper));

        let wrong = [(ENV_EXECUTION_MODE, "live"), (ENV_OPERATOR_CONFIRM, "yes")];
        let mut w = LiveGate::from_lookup(env_of(&wrong), true);
        all_pass(&mut w);
        assert_eq!(
            w.check_with(true, O),
            Err(GateError::OperatorConfirmationMissing)
        );

        let mut d = g.clone();
        d.armed = false;
        assert_eq!(d.check_with(true, O), Err(GateError::NotArmed));
        // Disarmed (or killed) never blocks closing risk.
        assert_eq!(d.check_with(true, R), Ok(()));

        let mut c = g.clone();
        c.credentials_configured = false;
        assert_eq!(c.check_with(true, O), Err(GateError::CredentialsMissing));

        let mut r = g.clone();
        r.readiness.set(
            CheckItem::Closure120sTested,
            CheckStatus::NotRun("x".into()),
        );
        assert_eq!(
            r.check_with(true, O),
            Err(GateError::ReadinessIncomplete(vec![
                CheckItem::Closure120sTested
            ]))
        );
    }

    #[test]
    fn owner_sessions_are_scoped() {
        let mut cp = LiveGate::owner_session(OwnerSession::Checkpoint, true);
        all_pass(&mut cp);
        assert_eq!(cp.check_with(true, R), Ok(()));
        assert_eq!(
            cp.check_with(true, O),
            Err(GateError::SessionCannotOpenRisk)
        );
        assert_eq!(cp.check_with(false, R), Err(GateError::FeatureNotCompiled));
        let nocreds = LiveGate::owner_session(OwnerSession::Checkpoint, false);
        assert_eq!(
            nocreds.check_with(true, R),
            Err(GateError::CredentialsMissing)
        );

        let mut v = LiveGate::owner_session(OwnerSession::Validation, true);
        assert!(matches!(
            v.check_with(true, O),
            Err(GateError::ReadinessIncomplete(_))
        ));
        for i in VALIDATION_PREREQUISITES {
            v.readiness.set(i, CheckStatus::Pass("t".into()));
        }
        // Protective / closure / governor items are what validation establishes.
        assert_eq!(v.check_with(true, O), Ok(()));
        v.readiness.set(
            CheckItem::AccountReconciled,
            CheckStatus::Fail("open".into()),
        );
        assert_eq!(
            v.check_with(true, O),
            Err(GateError::ReadinessIncomplete(vec![
                CheckItem::AccountReconciled
            ]))
        );
    }

    #[test]
    fn from_env_is_never_armed_and_paper_reduce_is_closed() {
        let g = LiveGate::from_lookup(|_| None, true);
        assert!(!g.armed && g.owner_session.is_none());
        assert_eq!(g.check_with(true, R), Err(GateError::ModeIsPaper));
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
