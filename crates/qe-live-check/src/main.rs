//! `qe-live-check`: owner-run live-readiness, validation, checkpoint and arming CLI.
//! See docs/LIVE_READINESS.md for the exact order of commands.
use qe_coindcx::auth::load_credentials;
use qe_coindcx::live::{CheckItem, CheckStatus, LiveGate, OwnerSession};
use qe_coindcx::private::PrivateClient;
use qe_coindcx::PublicClient;
use qe_live_check::checkpoint::{self, CheckpointKind};
use qe_live_check::confirm::{Confirm, Terminal};
use qe_live_check::readiness::{self, check_label, DeskState, MARGINS};
use qe_live_check::state::{fresh_checks, record_check, StateDir};
use qe_live_check::{arming, now_ms, validate, Poll};
use rust_decimal::Decimal;
use std::str::FromStr;

const USAGE: &str = "qe-live-check <command> [options]   (state dir: $QE_LIVE_STATE_DIR, default data/live)
  status                                   gate, arm, kill and check status (no network)
  readiness [--read-account] [--pair P --margin M] [--json]
                                           credentials (configured/not), signing self-test, public
                                           instrument check, read-only account reconciliation
  governor                                 run `cargo test -p qe-risk` and record the result
  validate --pair P --margin M [--leverage N]
                                           SUPERVISED REAL minimum-size protective-order test:
                                           verifies isolated margin, TP/SL, and fill-to-flat within
                                           120 s during validation (immediate close); does NOT prove
                                           the desk's automatic 120 s holding-time enforcement
                                           (needs --features live-orders; typed confirmations)
  checkpoint --kind stage1|stage2|adhoc [--currency INR] [--expected X] [--withdrawal 2000]
                                           disarm, cancel all, exit all, verify flat, read actual
                                           balances, write reconciliation report (typed confirmation)
  balances [--json]                        read actual balances (read-only)
  reconcile-withdrawal --before REPORT.json --amount X [--fee F] [--currency INR] [--tolerance T]
                                           read balances after YOUR manual withdrawal and reconcile
  arm [--hours H]                          arm live (all checks recent + typed confirmation)
  arm --hours H --first-trade-verifies-protection
                                           arm with ProtectiveOrdersVerified / Closure120sTested
                                           NotRun: the first genuine strategy trade verifies them
                                           (failure: emergency close + permanent stop)
  disarm                                   disarm live
  kill [--reason TEXT]                     kill switch: disarm and block re-arming
  clear-kill                               clear the kill switch (typed confirmation; stays disarmed)";

fn val(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn decimal(args: &[String], name: &str) -> Result<Option<Decimal>, String> {
    val(args, name)
        .map(|s| Decimal::from_str(&s).map_err(|e| format!("{name}: {e}")))
        .transpose()
}

fn tag(s: &CheckStatus) -> (&'static str, &str) {
    match s {
        CheckStatus::Pass(d) => ("PASS  ", d),
        CheckStatus::Fail(d) => ("FAIL  ", d),
        CheckStatus::NotRun(d) => ("NOTRUN", d),
    }
}

fn record(dir: &StateDir, item: CheckItem, st: CheckStatus) {
    if let Err(e) = record_check(dir, item, st, now_ms()) {
        eprintln!("WARNING: could not record {item:?}: {e}");
    }
}

fn audit(dir: &StateDir, event: &str, v: serde_json::Value) {
    if let Err(e) = dir.audit(now_ms(), event, v) {
        eprintln!("WARNING: audit write failed: {e}");
    }
}

fn write_report<T: serde::Serialize>(dir: &StateDir, name: &str, v: &T) {
    match dir.write_json(&format!("reports/{name}"), v) {
        Ok(p) => println!("report written: {}", p.display()),
        Err(e) => eprintln!("WARNING: report write failed: {e}"),
    }
}

fn desk_state(dir: &StateDir) -> DeskState {
    dir.read_json("desk_state.json").unwrap_or_default()
}

fn print_checks(dir: &StateDir) {
    for c in fresh_checks(dir, now_ms()) {
        let (t, d) = tag(&c.status);
        println!("  [{t}] {:?} ({}): {d}", c.item, check_label(c.item));
    }
    println!("  [NOTE  ] {}", validate::DESK_HOLD_ENFORCEMENT_NOTE);
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = StateDir::from_env();
    let cmd = args.first().cloned().unwrap_or_default();
    let code = match cmd.as_str() {
        "status" => status(&dir),
        "readiness" => readiness_cmd(&dir, &args).await,
        "governor" => governor(&dir),
        "validate" => validate_cmd(&dir, &args).await,
        "checkpoint" => checkpoint_cmd(&dir, &args).await,
        "balances" => balances_cmd(&args).await,
        "reconcile-withdrawal" => withdrawal_cmd(&dir, &args).await,
        "arm" => arm_cmd(&dir, &args),
        "disarm" => match arming::disarm(&dir, now_ms(), "owner disarm") {
            Ok(()) => {
                println!("live DISARMED");
                0
            }
            Err(e) => {
                eprintln!("disarm failed: {e}");
                1
            }
        },
        "kill" => {
            let reason = val(&args, "--reason").unwrap_or_else(|| "owner kill switch".into());
            match arming::kill(&dir, &reason, now_ms()) {
                Ok(()) => {
                    println!("KILL SWITCH SET: live disarmed; risk-reducing calls stay allowed. Open positions are NOT closed by this command: run `qe-live-check checkpoint --kind adhoc` to flatten.");
                    0
                }
                Err(e) => {
                    eprintln!("kill failed: {e}");
                    1
                }
            }
        }
        "clear-kill" => match arming::clear_kill(&dir, &mut Terminal, now_ms()) {
            Ok(()) => {
                println!("kill switch cleared; live remains DISARMED");
                0
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        },
        _ => {
            println!("{USAGE}");
            if cmd.is_empty() || cmd == "--help" || cmd == "help" {
                0
            } else {
                2
            }
        }
    };
    std::process::exit(code);
}

fn status(dir: &StateDir) -> i32 {
    let (_, cred) = load_credentials();
    let gate = arming::gate_for_desk(dir, cred.configured, now_ms());
    let st = arming::arm_status(dir, now_ms());
    println!("QUANT-ENGINE live status (no network call)");
    println!("  live-orders compiled : {}", LiveGate::COMPILED);
    println!(
        "  credentials          : {}",
        if cred.configured {
            "configured"
        } else {
            "not configured"
        }
    );
    println!("  execution mode (env) : {:?}", gate.mode);
    println!(
        "  armed                : {} {}",
        st.armed,
        st.why_not.clone().unwrap_or_default()
    );
    if let Some(k) = &st.killed {
        println!("  KILL SWITCH          : set at {} ({})", k.at_ms, k.reason);
    }
    print_checks(dir);
    println!(
        "  desk may open risk   : {}",
        match gate.check() {
            Ok(()) => "YES (LIVE)".to_string(),
            Err(e) => format!("no ({e:?})"),
        }
    );
    0
}

async fn readiness_cmd(dir: &StateDir, args: &[String]) -> i32 {
    let (creds, cred) = load_credentials();
    record(
        dir,
        CheckItem::CredentialsConfigured,
        readiness::credentials_check(&cred),
    );
    record(dir, CheckItem::SigningSelfTest, readiness::signing_check());
    if let (Some(pair), Some(margin)) = (val(args, "--pair"), val(args, "--margin")) {
        let st = match PublicClient::new().futures_instrument(&pair, &margin).await {
            Ok(i) => readiness::instrument_check(&i),
            Err(e) => CheckStatus::Fail(format!("instrument fetch failed: {e}")),
        };
        record(dir, CheckItem::InstrumentRequirementsVerified, st);
    }
    if flag(args, "--read-account") {
        let st = match creds {
            None => CheckStatus::Fail("credentials not configured".into()),
            Some(c) => {
                // Default gate: every mutation refused; reads only.
                let pc = PrivateClient::new(c, LiveGate::default());
                readiness::account_check(&pc, &desk_state(dir), now_ms())
                    .await
                    .0
            }
        };
        record(dir, CheckItem::AccountReconciled, st);
    }
    let fresh = fresh_checks(dir, now_ms());
    if flag(args, "--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&fresh).unwrap_or_default()
        );
    } else {
        println!("QUANT-ENGINE live readiness (no order is placed by this command)");
        print_checks(dir);
    }
    if fresh.iter().all(|c| c.status.is_pass()) {
        0
    } else {
        2
    }
}

fn governor(dir: &StateDir) -> i32 {
    println!("running: cargo test -p qe-risk -j2 --quiet");
    let st = match std::process::Command::new("cargo")
        .args(["test", "-p", "qe-risk", "-j2", "--quiet"])
        .status()
    {
        Ok(s) if s.success() => {
            CheckStatus::Pass("cargo test -p qe-risk passed on this machine".into())
        }
        Ok(s) => CheckStatus::Fail(format!("cargo test -p qe-risk failed ({s})")),
        Err(e) => CheckStatus::Fail(format!("could not run cargo: {e}")),
    };
    let ok = st.is_pass();
    record(dir, CheckItem::RiskGovernorAcceptance, st);
    if ok {
        0
    } else {
        2
    }
}

async fn validate_cmd(dir: &StateDir, args: &[String]) -> i32 {
    let (Some(pair), Some(margin)) = (val(args, "--pair"), val(args, "--margin")) else {
        eprintln!("validate needs --pair <PAIR> --margin <USDT|INR>");
        return 2;
    };
    let leverage: u32 = val(args, "--leverage")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    if !LiveGate::COMPILED {
        eprintln!("REFUSED: built without live orders. Re-run with: cargo run -p qe-live-check --features live-orders -- validate ...");
        return 2;
    }
    if let Some(k) = arming::killed(dir) {
        eprintln!(
            "REFUSED: kill switch set ({}). `qe-live-check clear-kill` first.",
            k.reason
        );
        return 2;
    }
    let (creds, cred) = load_credentials();
    let Some(creds) = creds else {
        eprintln!(
            "REFUSED: credentials not configured ({})",
            cred.problem.unwrap_or_default()
        );
        return 2;
    };
    println!("== Protective-order validation on {pair}/{margin} (REAL account, smallest permitted order) ==");
    let public = PublicClient::new();
    let instr = match public.futures_instrument(&pair, &margin).await {
        Ok(i) => i,
        Err(e) => {
            eprintln!("instrument fetch failed: {e}");
            return 2;
        }
    };
    let price = match public.futures_rt().await {
        Ok(rt) => rt.prices.get(&pair).and_then(|p| p.ls.or(p.mp)),
        Err(e) => {
            eprintln!("price fetch failed: {e}");
            return 2;
        }
    };
    let Some(price) = price else {
        eprintln!("no current price for {pair}");
        return 2;
    };
    let mut gate = LiveGate::owner_session(OwnerSession::Validation, true);
    gate.readiness
        .set(CheckItem::SigningSelfTest, readiness::signing_check());
    let inst_st = readiness::instrument_check(&instr);
    gate.readiness
        .set(CheckItem::InstrumentRequirementsVerified, inst_st.clone());
    record(dir, CheckItem::InstrumentRequirementsVerified, inst_st);
    // The validation gate refuses risk until AccountReconciled passes below.
    let pc = PrivateClient::new(creds, gate.clone());
    let acct = readiness::account_check(&pc, &DeskState::default(), now_ms())
        .await
        .0;
    gate.readiness
        .set(CheckItem::AccountReconciled, acct.clone());
    pc.set_gate(gate.clone());
    record(dir, CheckItem::AccountReconciled, acct);
    if let Err(e) = gate.check() {
        eprintln!("REFUSED before any order: {e:?}");
        for (i, s) in &gate.readiness.items {
            let (t, d) = tag(s);
            println!("  [{t}] {i:?} ({}): {d}", check_label(*i));
        }
        return 2;
    }
    let plan = match validate::plan(&instr, price, leverage) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("REFUSED: {e}");
            return 2;
        }
    };
    println!("plan: {}", serde_json::to_string(&plan).unwrap_or_default());
    audit(dir, "validation_start", serde_json::json!({"plan": plan}));
    let out = validate::run(&pc, &mut Terminal, plan, Poll::DEFAULT, &now_ms, true).await;
    if out.entry_sent {
        audit(
            dir,
            "validation_margin_type",
            serde_json::json!({
                "pair": out.plan.pair,
                "margin_type": out.margin_type,
                "verified_isolated": out.margin_type_verified,
            }),
        );
        println!(
            "margin_type reported by the exchange: {:?} (isolated verified: {})",
            out.margin_type, out.margin_type_verified
        );
    }
    for (item, st) in out.checks() {
        record(dir, item, st);
    }
    audit(
        dir,
        "validation_end",
        serde_json::to_value(&out).unwrap_or_default(),
    );
    write_report(dir, &format!("validation-{}.json", now_ms()), &out);
    if !out.flat_at_end && out.entry_sent {
        println!("!!! NOT VERIFIED FLAT. Open the CoinDCX app NOW and close {pair} manually, then run `qe-live-check checkpoint --kind adhoc`.");
    }
    println!(
        "Result: protective {:?}; {}: {:?}",
        out.protective_status(),
        validate::CLOSURE_CHECK_LABEL,
        out.closure_status()
    );
    println!("NOTE: {}", validate::DESK_HOLD_ENFORCEMENT_NOTE);
    if out.protective_verified && out.closure_status().is_pass() {
        0
    } else {
        2
    }
}

async fn checkpoint_cmd(dir: &StateDir, args: &[String]) -> i32 {
    let kind = match val(args, "--kind").as_deref() {
        Some("stage1") => CheckpointKind::Stage1,
        Some("stage2") => CheckpointKind::Stage2,
        Some("adhoc") => CheckpointKind::Adhoc,
        _ => {
            eprintln!("checkpoint needs --kind stage1|stage2|adhoc");
            return 2;
        }
    };
    let ccy = val(args, "--currency").unwrap_or_else(|| "INR".into());
    let (expected, withdrawal) = match (decimal(args, "--expected"), decimal(args, "--withdrawal"))
    {
        (Ok(e), Ok(w)) => (e, w),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("{e}");
            return 2;
        }
    };
    // Stop new entries first, whatever happens next.
    let stop = match kind {
        CheckpointKind::Stage2 => arming::kill(dir, "final checkpoint (stage 2)", now_ms()),
        _ => arming::disarm(dir, now_ms(), &format!("checkpoint {kind:?}")),
    };
    if let Err(e) = stop {
        eprintln!("WARNING: could not write disarm state: {e}");
    }
    println!("live DISARMED for checkpoint {kind:?}: no new entries");
    if !LiveGate::COMPILED {
        eprintln!("REFUSED: flattening needs --features live-orders. Re-run: cargo run -p qe-live-check --features live-orders -- checkpoint ...");
        return 2;
    }
    let (creds, cred) = load_credentials();
    let Some(creds) = creds else {
        eprintln!(
            "REFUSED: credentials not configured ({})",
            cred.problem.unwrap_or_default()
        );
        return 2;
    };
    if !Terminal.confirm(
        "This cancels ALL open futures orders (USDT + INR) and exits ALL futures positions on your REAL account, then reads balances.",
        "FLATTEN ALL",
    ) {
        audit(dir, "checkpoint_aborted", serde_json::json!({"kind": kind}));
        return 2;
    }
    let pc = PrivateClient::new(
        creds,
        LiveGate::owner_session(OwnerSession::Checkpoint, true),
    );
    let flat = checkpoint::flatten_all(&pc, &MARGINS, Poll::DEFAULT).await;
    for a in &flat.actions {
        println!("  action: {a}");
    }
    for e in &flat.errors {
        println!("  ERROR : {e}");
    }
    let bal = checkpoint::read_balances(&pc, now_ms()).await;
    let rep = checkpoint::build_report(kind, now_ms(), flat, bal, &ccy, expected, withdrawal);
    audit(
        dir,
        "checkpoint",
        serde_json::to_value(&rep).unwrap_or_default(),
    );
    println!("{}", serde_json::to_string_pretty(&rep).unwrap_or_default());
    println!("VERDICT: {}", rep.verdict);
    if let Some(w) = &rep.withdrawal {
        for l in &w.instructions {
            println!("  {l}");
        }
    }
    write_report(
        dir,
        &format!("checkpoint-{kind:?}-{}.json", rep.at_ms).to_lowercase(),
        &rep,
    );
    if rep.flatten.flat && rep.balances.is_some() {
        0
    } else {
        2
    }
}

async fn balances_cmd(args: &[String]) -> i32 {
    let (creds, cred) = load_credentials();
    let Some(creds) = creds else {
        eprintln!(
            "credentials not configured ({})",
            cred.problem.unwrap_or_default()
        );
        return 2;
    };
    let pc = PrivateClient::new(creds, LiveGate::default());
    match checkpoint::read_balances(&pc, now_ms()).await {
        Ok(b) => {
            if flag(args, "--json") {
                println!("{}", serde_json::to_string_pretty(&b).unwrap_or_default());
            } else {
                println!(
                    "ACTUAL balances read at {} ms (exchange read; currencies never mixed)",
                    b.at_ms
                );
                for (c, v) in &b.futures {
                    println!("  futures {c}: {v}");
                }
                for (c, v) in &b.spot {
                    println!("  spot    {c}: {v}");
                }
            }
            0
        }
        Err(e) => {
            eprintln!("balance read failed: {e} (actual balance unknown; nothing assumed)");
            2
        }
    }
}

async fn withdrawal_cmd(dir: &StateDir, args: &[String]) -> i32 {
    let Some(before_path) = val(args, "--before") else {
        eprintln!(
            "reconcile-withdrawal needs --before <checkpoint report.json> --amount <withdrawn>"
        );
        return 2;
    };
    let amount = match decimal(args, "--amount") {
        Ok(Some(a)) => a,
        _ => {
            eprintln!("--amount <withdrawn> is required");
            return 2;
        }
    };
    let fee = decimal(args, "--fee").ok().flatten().unwrap_or_default();
    let tol = decimal(args, "--tolerance")
        .ok()
        .flatten()
        .unwrap_or(Decimal::new(1, 2));
    let ccy = val(args, "--currency").unwrap_or_else(|| "INR".into());
    let before: Option<checkpoint::CheckpointReport> = std::fs::read_to_string(&before_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    let Some(before) = before.and_then(|r| r.balances) else {
        eprintln!("{before_path}: no balances in that report");
        return 2;
    };
    let (creds, cred) = load_credentials();
    let Some(creds) = creds else {
        eprintln!(
            "credentials not configured ({})",
            cred.problem.unwrap_or_default()
        );
        return 2;
    };
    let pc = PrivateClient::new(creds, LiveGate::default());
    let after = match checkpoint::read_balances(&pc, now_ms()).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("balance read failed: {e}");
            return 2;
        }
    };
    let r = checkpoint::reconcile_withdrawal(&before, &after, &ccy, amount, fee, tol);
    audit(
        dir,
        "withdrawal_reconciliation",
        serde_json::to_value(&r).unwrap_or_default(),
    );
    println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
    println!("VERDICT: {}", r.verdict);
    write_report(dir, &format!("withdrawal-{}.json", r.at_ms), &r);
    if r.confirmed {
        0
    } else {
        2
    }
}

fn arm_cmd(dir: &StateDir, args: &[String]) -> i32 {
    let hours: i64 = val(args, "--hours")
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let first_trade = flag(args, "--first-trade-verifies-protection");
    match arming::arm_with(dir, &mut Terminal, hours, now_ms(), first_trade) {
        Ok(r) => {
            println!(
                "LIVE ARMED until {} ms{}. The desk also needs QE_EXECUTION_MODE=live and QE_LIVE_OPERATOR_CONFIRM in its environment. Kill: `qe-live-check kill`.",
                r.expires_at_ms,
                if first_trade { " (FIRST-TRADE PROTECTION mode: the first live trade verifies TP/SL + isolated margin or the desk stops permanently)" } else { "" }
            );
            0
        }
        Err(e) => {
            eprintln!("NOT ARMED: {e}");
            2
        }
    }
}
