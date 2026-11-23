//! Live-arming switch (4) and kill switch.
//!
//! * [`arm`] needs: no kill switch, every readiness item passed RECENTLY
//!   ([`crate::state::max_age_ms`]), and the owner typing
//!   [`OPERATOR_CONFIRM_PHRASE`] on a terminal. It writes `armed.json` with an
//!   expiry (max [`MAX_ARM_HOURS`]) and the check snapshot, and audits it.
//! * [`gate_for_desk`] is the desk's hook: `LiveGate::from_env` (mode +
//!   env confirmation) plus `armed = true` and the snapshot's readiness only
//!   while the arm record is valid, unexpired and no kill switch is set.
//! * [`kill`] (desk or owner, no confirmation needed) writes `KILL`, removes
//!   the arm record and audits it. Only the owner can clear it ([`clear_kill`],
//!   typed confirmation). A kill never blocks risk-REDUCING calls.
use crate::confirm::Confirm;
use crate::state::{fresh_checks, CheckRecord, StateDir};
use qe_coindcx::live::{CheckItem, CheckStatus, LiveGate, OPERATOR_CONFIRM_PHRASE};
use serde::{Deserialize, Serialize};

pub const MAX_ARM_HOURS: i64 = 24;
pub const CLEAR_KILL_PHRASE: &str = "CLEAR KILL SWITCH";
const ARM_FILE: &str = "armed.json";
const KILL_FILE: &str = "KILL";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArmRecord {
    pub armed_at_ms: i64,
    pub expires_at_ms: i64,
    pub checks: Vec<CheckRecord>,
    pub confirmation: String,
    /// First-genuine-trade protection mode: [`FIRST_TRADE_DEFERRED`] were
    /// NotRun at arming and are verified by the first live strategy trade
    /// (no separate test order). Owner-typed like every arm.
    #[serde(default)]
    pub first_trade_verifies_protection: bool,
}

/// Readiness items a first-trade arm may leave NotRun (never Fail).
pub const FIRST_TRADE_DEFERRED: [CheckItem; 2] = [
    CheckItem::ProtectiveOrdersVerified,
    CheckItem::Closure120sTested,
];

fn deferred(item: CheckItem) -> bool {
    FIRST_TRADE_DEFERRED.contains(&item)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KillRecord {
    pub at_ms: i64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ArmStatus {
    pub armed: bool,
    pub armed_at_ms: Option<i64>,
    pub expires_at_ms: Option<i64>,
    pub killed: Option<KillRecord>,
    pub why_not: Option<String>,
    pub checks: Vec<CheckRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ArmError {
    Killed(String),
    ChecksNotPassed(Vec<String>),
    BadDuration(i64),
    NotConfirmed,
    Io(String),
}

impl std::fmt::Display for ArmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Killed(r) => write!(f, "kill switch is set ({r}); clear it first"),
            Self::ChecksNotPassed(v) => write!(f, "checks not passed recently: {}", v.join("; ")),
            Self::BadDuration(h) => write!(f, "arm duration {h} h outside 1..={MAX_ARM_HOURS}"),
            Self::NotConfirmed => write!(f, "owner did not type the confirmation phrase"),
            Self::Io(e) => write!(f, "state write failed: {e}"),
        }
    }
}

fn io(e: std::io::Error) -> ArmError {
    ArmError::Io(e.to_string())
}

pub fn killed(dir: &StateDir) -> Option<KillRecord> {
    let text = std::fs::read_to_string(dir.file(KILL_FILE)).ok()?;
    Some(serde_json::from_str(&text).unwrap_or(KillRecord {
        at_ms: 0,
        reason: "unreadable kill record (still counts as killed)".into(),
    }))
}

/// Arm for `hours` (1..=24). Every check must pass recently; the owner types
/// the phrase on a terminal. Audited.
pub fn arm<C: Confirm>(
    dir: &StateDir,
    confirm: &mut C,
    hours: i64,
    now_ms: i64,
) -> Result<ArmRecord, ArmError> {
    arm_with(dir, confirm, hours, now_ms, false)
}

/// [`arm`], optionally in first-genuine-trade protection mode: every other
/// item must pass recently; [`FIRST_TRADE_DEFERRED`] may be NotRun (a Fail
/// still refuses). The desk then verifies protection on the first live
/// strategy trade and stops permanently if it cannot.
pub fn arm_with<C: Confirm>(
    dir: &StateDir,
    confirm: &mut C,
    hours: i64,
    now_ms: i64,
    first_trade: bool,
) -> Result<ArmRecord, ArmError> {
    if let Some(k) = killed(dir) {
        return Err(ArmError::Killed(k.reason));
    }
    if !(1..=MAX_ARM_HOURS).contains(&hours) {
        return Err(ArmError::BadDuration(hours));
    }
    let checks = fresh_checks(dir, now_ms);
    let failing: Vec<String> = checks
        .iter()
        .filter(|c| !c.status.is_pass())
        .filter(|c| {
            !(first_trade && deferred(c.item) && matches!(c.status, CheckStatus::NotRun(_)))
        })
        .map(|c| format!("{:?}: {:?}", c.item, c.status))
        .collect();
    if !failing.is_empty() {
        let _ = dir.audit(
            now_ms,
            "arm_refused",
            serde_json::json!({"failing": failing}),
        );
        return Err(ArmError::ChecksNotPassed(failing));
    }
    let prompt = if first_trade {
        format!(
            "ARM LIVE TRADING for {hours} h on your REAL CoinDCX account in FIRST-TRADE PROTECTION mode. Protective orders and the 120 s close have NOT been verified with a test order: the FIRST genuine strategy trade is financially exposed from its fill until its TP/SL are verified on the exchange; if verification fails the desk emergency-closes it and stops permanently. Kill any time with `qe-live-check kill`."
        )
    } else {
        format!(
            "ARM LIVE TRADING for {hours} h on your REAL CoinDCX account. The desk may then place real orders within the risk governor's limits. Kill any time with `qe-live-check kill`."
        )
    };
    if !confirm.confirm(&prompt, OPERATOR_CONFIRM_PHRASE) {
        let _ = dir.audit(
            now_ms,
            "arm_refused",
            serde_json::json!({"failing": ["not confirmed"]}),
        );
        return Err(ArmError::NotConfirmed);
    }
    let rec = ArmRecord {
        armed_at_ms: now_ms,
        expires_at_ms: now_ms + hours * 3_600_000,
        checks,
        confirmation: "typed on terminal".into(),
        first_trade_verifies_protection: first_trade,
    };
    dir.write_json(ARM_FILE, &rec).map_err(io)?;
    dir.audit(
        now_ms,
        "armed",
        serde_json::json!({"expires_at_ms": rec.expires_at_ms,
            "first_trade_verifies_protection": first_trade}),
    )
    .map_err(io)?;
    Ok(rec)
}

/// True while a valid arm record is in first-genuine-trade protection mode.
pub fn first_trade_mode(dir: &StateDir) -> bool {
    dir.read_json::<ArmRecord>(ARM_FILE)
        .is_some_and(|r| r.first_trade_verifies_protection)
}

/// Owner-initiated disarm (no kill flag).
pub fn disarm(dir: &StateDir, now_ms: i64, reason: &str) -> std::io::Result<()> {
    let _ = std::fs::remove_file(dir.file(ARM_FILE));
    dir.audit(now_ms, "disarmed", serde_json::json!({"reason": reason}))
}

/// Kill switch: disarm and block re-arming until the owner clears it.
pub fn kill(dir: &StateDir, reason: &str, now_ms: i64) -> std::io::Result<()> {
    dir.write_json(
        KILL_FILE,
        &KillRecord {
            at_ms: now_ms,
            reason: reason.into(),
        },
    )?;
    let _ = std::fs::remove_file(dir.file(ARM_FILE));
    dir.audit(now_ms, "kill", serde_json::json!({"reason": reason}))
}

pub fn clear_kill<C: Confirm>(
    dir: &StateDir,
    confirm: &mut C,
    now_ms: i64,
) -> Result<(), ArmError> {
    if killed(dir).is_none() {
        return Ok(());
    }
    if !confirm.confirm(
        "Clear the kill switch? Live stays DISARMED; you must re-run the checks and `arm`.",
        CLEAR_KILL_PHRASE,
    ) {
        return Err(ArmError::NotConfirmed);
    }
    std::fs::remove_file(dir.file(KILL_FILE)).map_err(io)?;
    dir.audit(now_ms, "kill_cleared", serde_json::json!({}))
        .map_err(io)
}

pub fn arm_status(dir: &StateDir, now_ms: i64) -> ArmStatus {
    let killed = killed(dir);
    let rec: Option<ArmRecord> = dir.read_json(ARM_FILE);
    let why_not = match (&killed, &rec) {
        (Some(k), _) => Some(format!("kill switch: {}", k.reason)),
        (None, None) => Some("not armed".into()),
        (None, Some(r)) if now_ms >= r.expires_at_ms => Some("arm record expired".into()),
        (None, Some(r))
            if !r.checks.iter().all(|c| {
                c.status.is_pass()
                    || (r.first_trade_verifies_protection
                        && deferred(c.item)
                        && matches!(c.status, CheckStatus::NotRun(_)))
            }) =>
        {
            Some("arm record has non-passing checks".into())
        }
        (None, Some(r)) if r.armed_at_ms > now_ms => Some("arm record from the future".into()),
        _ => None,
    };
    ArmStatus {
        armed: why_not.is_none(),
        armed_at_ms: rec.as_ref().map(|r| r.armed_at_ms),
        expires_at_ms: rec.as_ref().map(|r| r.expires_at_ms),
        killed,
        why_not,
        checks: fresh_checks(dir, now_ms),
    }
}

/// The desk's live gate. Closed (paper) unless env mode/confirmation are set
/// AND the arm record is valid. Credentials status comes from the desk's own
/// `auth::load_credentials`.
pub fn gate_for_desk(dir: &StateDir, credentials_configured: bool, now_ms: i64) -> LiveGate {
    gate_from(LiveGate::from_env(credentials_configured), dir, now_ms)
}

pub fn gate_from(mut gate: LiveGate, dir: &StateDir, now_ms: i64) -> LiveGate {
    let st = arm_status(dir, now_ms);
    if st.armed {
        if let Some(rec) = dir.read_json::<ArmRecord>(ARM_FILE) {
            let ft = rec.first_trade_verifies_protection;
            for c in rec.checks {
                gate.readiness.set(c.item, c.status);
            }
            if ft {
                // Deferred items: the first live trade's recorded result once it
                // exists (a Fail closes the gate), otherwise deferred-pass.
                let now_checks = dir.checks();
                for item in FIRST_TRADE_DEFERRED {
                    let st = now_checks
                        .iter()
                        .find(|c| c.item == item && !matches!(c.status, CheckStatus::NotRun(_)))
                        .map(|c| c.status.clone())
                        .unwrap_or_else(|| {
                            CheckStatus::Pass(
                                "deferred: verified by the first live trade (first-trade protection mode)"
                                    .into(),
                            )
                        });
                    gate.readiness.set(item, st);
                }
            }
            gate.armed = true;
        }
    } else {
        gate.armed = false;
    }
    if !gate.credentials_configured {
        gate.readiness.set(
            qe_coindcx::live::CheckItem::CredentialsConfigured,
            CheckStatus::Fail("not configured".into()),
        );
    }
    gate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confirm::Scripted;
    use crate::state::{record_check, temp_dir};
    use qe_coindcx::live::{
        CheckItem, GateError, Purpose, ENV_EXECUTION_MODE, ENV_OPERATOR_CONFIRM,
    };

    const T: i64 = 1_795_440_000_000;

    fn pass_all(d: &StateDir, at: i64) {
        for i in CheckItem::ALL {
            record_check(d, i, CheckStatus::Pass("ok".into()), at).unwrap();
        }
    }

    fn live_env() -> LiveGate {
        LiveGate::from_lookup(
            |k| match k {
                ENV_EXECUTION_MODE => Some("live".into()),
                ENV_OPERATOR_CONFIRM => Some(OPERATOR_CONFIRM_PHRASE.into()),
                _ => None,
            },
            true,
        )
    }

    #[test]
    fn arm_requires_recent_checks_and_typed_confirmation() {
        let d = temp_dir("arm");
        // Nothing run: refused, audited.
        assert!(matches!(
            arm(&d, &mut Scripted::new(&[true]), 2, T),
            Err(ArmError::ChecksNotPassed(_))
        ));
        pass_all(&d, T);
        // Wrong / missing typing: refused.
        assert_eq!(
            arm(&d, &mut Scripted::new(&[false]), 2, T),
            Err(ArmError::NotConfirmed)
        );
        assert!(matches!(
            arm(&d, &mut Scripted::new(&[true]), 25, T),
            Err(ArmError::BadDuration(25))
        ));
        // Stale account reconciliation (16 min): refused.
        assert!(matches!(
            arm(&d, &mut Scripted::new(&[true]), 2, T + 16 * 60_000),
            Err(ArmError::ChecksNotPassed(v)) if v.iter().any(|s| s.contains("AccountReconciled"))
        ));
        let mut c = Scripted::new(&[true]);
        let rec = arm(&d, &mut c, 2, T + 60_000).unwrap();
        assert_eq!(c.asked, vec![OPERATOR_CONFIRM_PHRASE.to_string()]);
        assert_eq!(rec.expires_at_ms, T + 60_000 + 2 * 3_600_000);
        assert!(arm_status(&d, T + 120_000).armed);
        let events: Vec<String> = d
            .audit_lines()
            .iter()
            .map(|l| l["event"].as_str().unwrap().to_string())
            .collect();
        assert!(
            events.contains(&"armed".to_string()) && events.contains(&"arm_refused".to_string())
        );
    }

    #[test]
    fn desk_gate_opens_only_while_armed_and_kill_closes_it() {
        let d = temp_dir("gate");
        // Not armed: even with live env, opening risk is refused.
        let g = gate_from(live_env(), &d, T);
        assert!(!g.armed);
        let want = if LiveGate::COMPILED {
            GateError::NotArmed
        } else {
            GateError::FeatureNotCompiled
        };
        assert_eq!(g.check_for(Purpose::OpenRisk), Err(want));
        pass_all(&d, T);
        arm(&d, &mut Scripted::new(&[true]), 1, T).unwrap();
        let g = gate_from(live_env(), &d, T + 1000);
        assert!(g.armed && g.readiness.all_pass());
        if LiveGate::COMPILED {
            assert_eq!(g.check_for(Purpose::OpenRisk), Ok(()));
        }
        // Expired.
        assert!(!gate_from(live_env(), &d, T + 3_600_001).armed);
        // Paper env stays closed even when armed.
        let paper = gate_from(LiveGate::from_lookup(|_| None, true), &d, T + 1000);
        assert!(paper.armed && paper.check().is_err());
        // Kill: disarmed, cannot re-arm, but risk-reducing calls stay allowed.
        kill(&d, "operator pressed kill", T + 2000).unwrap();
        let g = gate_from(live_env(), &d, T + 3000);
        assert!(!g.armed);
        if LiveGate::COMPILED {
            assert_eq!(g.check_for(Purpose::ReduceRisk), Ok(()));
        }
        pass_all(&d, T + 3000);
        assert!(matches!(
            arm(&d, &mut Scripted::new(&[true]), 1, T + 3000),
            Err(ArmError::Killed(_))
        ));
        assert_eq!(
            clear_kill(&d, &mut Scripted::new(&[false]), T + 4000),
            Err(ArmError::NotConfirmed)
        );
        clear_kill(&d, &mut Scripted::new(&[true]), T + 4000).unwrap();
        // Clearing the kill does not re-arm.
        assert!(!arm_status(&d, T + 5000).armed);
    }

    #[test]
    fn first_trade_arm_defers_only_protection_and_closure() {
        let d = temp_dir("first-trade");
        for i in CheckItem::ALL {
            if !deferred(i) {
                record_check(&d, i, CheckStatus::Pass("ok".into()), T).unwrap();
            }
        }
        // the normal arm still refuses: protection / closure not run
        assert!(matches!(
            arm(&d, &mut Scripted::new(&[true]), 2, T),
            Err(ArmError::ChecksNotPassed(v)) if v.len() == 2
        ));
        // first-trade arm: still owner-typed
        assert_eq!(
            arm_with(&d, &mut Scripted::new(&[false]), 2, T, true),
            Err(ArmError::NotConfirmed)
        );
        let rec = arm_with(&d, &mut Scripted::new(&[true]), 2, T, true).unwrap();
        assert!(rec.first_trade_verifies_protection);
        assert!(first_trade_mode(&d));
        assert!(arm_status(&d, T + 1000).armed);
        let g = gate_from(live_env(), &d, T + 1000);
        assert!(g.armed && g.readiness.all_pass(), "{:?}", g.readiness);
        // the first trade's FAILED verification closes the gate
        record_check(
            &d,
            CheckItem::ProtectiveOrdersVerified,
            CheckStatus::Fail("source: first live trade t1 - not listed".into()),
            T + 2000,
        )
        .unwrap();
        let g = gate_from(live_env(), &d, T + 3000);
        assert!(!g.readiness.all_pass());
        if LiveGate::COMPILED {
            assert!(g.check_for(Purpose::OpenRisk).is_err());
        }
        // and a Fail can never be deferred at arming
        disarm(&d, T + 4000, "test").unwrap();
        assert!(matches!(
            arm_with(&d, &mut Scripted::new(&[true]), 2, T + 4000, true),
            Err(ArmError::ChecksNotPassed(_))
        ));
        // any other failing item refuses the first-trade arm too
        let d = temp_dir("first-trade-2");
        record_check(
            &d,
            CheckItem::CredentialsConfigured,
            CheckStatus::Pass("ok".into()),
            T,
        )
        .unwrap();
        assert!(matches!(
            arm_with(&d, &mut Scripted::new(&[true]), 2, T, true),
            Err(ArmError::ChecksNotPassed(_))
        ));
    }
}
