//! Persistent, secret-free state of the owner tools, under one directory
//! (`QE_LIVE_STATE_DIR`, default `data/live`, gitignored):
//!
//! * `checks.json`  latest result + timestamp per readiness item;
//! * `armed.json`   the owner's arm record (absent = not armed);
//! * `KILL`         kill switch (present = disarmed until the owner clears it);
//! * `audit.jsonl`  append-only audit log of every check, arm, kill, order step;
//! * `reports/`     checkpoint and withdrawal reconciliation reports.
use qe_coindcx::live::{CheckItem, CheckStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const ENV_STATE_DIR: &str = "QE_LIVE_STATE_DIR";
pub const DEFAULT_STATE_DIR: &str = "data/live";

#[derive(Debug, Clone)]
pub struct StateDir(PathBuf);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckRecord {
    pub item: CheckItem,
    pub status: CheckStatus,
    pub at_ms: i64,
}

impl StateDir {
    pub fn from_env() -> Self {
        Self::new(std::env::var(ENV_STATE_DIR).unwrap_or_else(|_| DEFAULT_STATE_DIR.into()))
    }
    pub fn new(p: impl Into<PathBuf>) -> Self {
        Self(p.into())
    }
    pub fn root(&self) -> &Path {
        &self.0
    }
    pub fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// Write via temp file + rename so a crash never leaves half a record.
    pub fn write_json<T: Serialize>(&self, name: &str, v: &T) -> std::io::Result<PathBuf> {
        let path = self.file(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    pub fn read_json<T: for<'de> Deserialize<'de>>(&self, name: &str) -> Option<T> {
        let text = std::fs::read_to_string(self.file(name)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Append one JSON line to `audit.jsonl`. Audit failures are returned, never ignored.
    pub fn audit(&self, at_ms: i64, event: &str, detail: Value) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.0)?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.file("audit.jsonl"))?;
        let line = serde_json::json!({"at_ms": at_ms, "event": event, "detail": detail});
        writeln!(f, "{line}")
    }

    pub fn audit_lines(&self) -> Vec<Value> {
        std::fs::read_to_string(self.file("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    pub fn checks(&self) -> Vec<CheckRecord> {
        self.read_json("checks.json").unwrap_or_default()
    }
}

/// Record the latest result of one readiness item (and audit it).
pub fn record_check(
    dir: &StateDir,
    item: CheckItem,
    status: CheckStatus,
    at_ms: i64,
) -> std::io::Result<()> {
    let mut all = dir.checks();
    all.retain(|r| r.item != item);
    all.push(CheckRecord {
        item,
        status: status.clone(),
        at_ms,
    });
    all.sort_by_key(|r| r.item);
    dir.write_json("checks.json", &all)?;
    dir.audit(
        at_ms,
        "check",
        serde_json::json!({"item": item, "status": status}),
    )
}

/// How long a passing result counts as "recent" for arming.
pub fn max_age_ms(item: CheckItem) -> i64 {
    const MIN: i64 = 60_000;
    match item {
        CheckItem::CredentialsConfigured
        | CheckItem::SigningSelfTest
        | CheckItem::AccountReconciled => 15 * MIN,
        CheckItem::InstrumentRequirementsVerified => 6 * 60 * MIN,
        CheckItem::ProtectiveOrdersVerified
        | CheckItem::Closure120sTested
        | CheckItem::RiskGovernorAcceptance => 24 * 60 * MIN,
    }
}

/// The latest record per item with staleness applied: a pass older than
/// [`max_age_ms`] becomes `NotRun("stale ...")`; a missing item is `NotRun`.
pub fn fresh_checks(dir: &StateDir, now_ms: i64) -> Vec<CheckRecord> {
    let recorded = dir.checks();
    CheckItem::ALL
        .iter()
        .map(|item| match recorded.iter().find(|r| r.item == *item) {
            Some(r) if r.status.is_pass() && now_ms - r.at_ms > max_age_ms(*item) => CheckRecord {
                item: *item,
                status: CheckStatus::NotRun(format!(
                    "stale: passed {} min ago, limit {} min; re-run",
                    (now_ms - r.at_ms) / 60_000,
                    max_age_ms(*item) / 60_000
                )),
                at_ms: r.at_ms,
            },
            Some(r) => r.clone(),
            None => CheckRecord {
                item: *item,
                status: CheckStatus::NotRun("never run".into()),
                at_ms: 0,
            },
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn temp_dir(tag: &str) -> StateDir {
    let p = std::env::temp_dir().join(format!(
        "qe-live-check-{tag}-{}-{}",
        std::process::id(),
        crate::now_ms()
    ));
    let _ = std::fs::remove_dir_all(&p);
    StateDir::new(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_persist_and_go_stale() {
        let d = temp_dir("state");
        let t0 = 1_000_000_000_000;
        record_check(
            &d,
            CheckItem::AccountReconciled,
            CheckStatus::Pass("flat".into()),
            t0,
        )
        .unwrap();
        record_check(
            &d,
            CheckItem::AccountReconciled,
            CheckStatus::Pass("flat again".into()),
            t0 + 1,
        )
        .unwrap();
        assert_eq!(d.checks().len(), 1);
        let fresh = fresh_checks(&d, t0 + 60_000);
        let acct = fresh
            .iter()
            .find(|r| r.item == CheckItem::AccountReconciled)
            .unwrap();
        assert!(acct.status.is_pass());
        let stale = fresh_checks(&d, t0 + 16 * 60_000);
        let acct = stale
            .iter()
            .find(|r| r.item == CheckItem::AccountReconciled)
            .unwrap();
        assert!(matches!(&acct.status, CheckStatus::NotRun(m) if m.starts_with("stale")));
        assert_eq!(fresh.len(), CheckItem::ALL.len());
        assert_eq!(d.audit_lines().len(), 2);
    }
}
