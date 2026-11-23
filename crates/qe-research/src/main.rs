//! `qe-research` CLI.
//!
//! ```text
//! qe-research record   --out DIR [--minutes 30] [--focus 10]
//! qe-research history  --out DIR [--pairs 30] [--days 7]
//! qe-research evaluate --rec DIR [--hist DIR] --out calibration.json [--folds 5] [--stride 5]
//! qe-research challenge --rec DIR --out models.json [--folds 5] [--stride 2]
//! ```
//! `challenge` writes the champion/challenger registry (see `registry`).
//! `evaluate` prints a compact summary and writes the full `CalibrationReport`.
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::PathBuf;

fn flags(args: &[String]) -> HashMap<String, String> {
    let mut m = HashMap::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(k) = a.strip_prefix("--") {
            m.insert(k.to_string(), it.next().cloned().unwrap_or_default());
        }
    }
    m
}

fn get<T: std::str::FromStr>(f: &HashMap<String, String>, k: &str, d: T) -> T {
    f.get(k).and_then(|v| v.parse().ok()).unwrap_or(d)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_default();
    let f = flags(&args);
    let dir = |k: &str| -> Result<PathBuf> {
        f.get(k)
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("--{k} DIR is required"))
    };
    match cmd.as_str() {
        "record" => {
            let s = qe_research::record::run(qe_research::record::RecordConfig {
                out_dir: dir("out")?,
                minutes: get(&f, "minutes", 30.0),
                focus_pairs: get(&f, "focus", 10),
            })
            .await?;
            println!("{s}");
        }
        "history" => {
            let s = qe_research::history::run(qe_research::history::HistoryConfig {
                out_dir: dir("out")?,
                pairs: get(&f, "pairs", 30),
                days_1m: get(&f, "days", 7.0),
            })
            .await?;
            println!("{s}");
        }
        "evaluate" => {
            let r = qe_research::report::build(
                &dir("rec")?,
                f.get("hist").map(PathBuf::from).as_deref(),
                get(&f, "folds", 5),
                get(&f, "stride", 5.0),
            )?;
            let path = dir("out")?;
            std::fs::write(&path, serde_json::to_vec_pretty(&r)?)?;
            println!("{}", qe_research::report::summary(&r));
        }
        "challenge" => {
            let r = qe_research::challenger::run(
                &dir("rec")?,
                get(&f, "stride", 2.0),
                get(&f, "folds", 5),
            )?;
            std::fs::write(dir("out")?, serde_json::to_vec_pretty(&r)?)?;
            let rows: Vec<_> = r
                .models
                .iter()
                .map(|m| {
                    serde_json::json!({"id": m.id, "promotable": m.promotable,
                    "calibration": m.calibration, "taker_taker": m.trading["taker_taker"],
                    "failures": m.promotion_failures})
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"data": r.data, "champion": r.champion, "reason": r.champion_reason, "models": rows})
                )?
            );
        }
        _ => {
            eprintln!("usage: qe-research record|history|evaluate|challenge (see crate docs)");
            std::process::exit(2);
        }
    }
    Ok(())
}
