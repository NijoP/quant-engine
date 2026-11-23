//! Append-only JSONL audit store under a gitignored data directory.
//!
//! One file per record kind (`decisions`, `fills`, `incidents`, `equity`,
//! `audit`). Lines are never rewritten, so a session can be audited after the
//! fact. Default directory: `<workspace>/data/desk` (override with `QE_DATA_DIR`).
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;

pub struct Store {
    pub dir: PathBuf,
    pub errors: u64,
    pub written: u64,
}

impl Store {
    pub fn open() -> Self {
        let dir = std::env::var("QE_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/desk"));
        let _ = std::fs::create_dir_all(&dir);
        Self {
            dir,
            errors: 0,
            written: 0,
        }
    }

    pub fn append(&mut self, kind: &str, v: &Value) {
        let path = self.dir.join(format!("{kind}.jsonl"));
        let r = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| writeln!(f, "{v}"));
        match r {
            Ok(()) => self.written += 1,
            Err(_) => self.errors += 1,
        }
    }
}
