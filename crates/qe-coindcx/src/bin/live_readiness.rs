//! Live-readiness check, run BY THE ACCOUNT OWNER on their own machine.
//!
//! ```text
//! cargo run -p qe-coindcx --bin live_readiness -- [--read-account] [--pair B-BTC_USDT --margin USDT] [--json]
//! ```
//! * Without `--read-account` it makes no authenticated call at all.
//! * `--read-account` performs only read-only-in-effect calls: futures wallets,
//!   positions (USDT + INR), and open/untriggered orders. It never places,
//!   cancels or modifies anything, and never prints the key or secret.
//! * `--pair/--margin` fetches PUBLIC instrument metadata and checks it.
//!
//! Exit status 0 only if every checklist item passes. This sprint-2 command
//! cannot pass "protective orders verified" / "120 s closure tested"; the
//! sprint-3 owner workflow `qe-live-check` (docs/LIVE_READINESS.md) runs the
//! supervised test, records results and arms live.
use qe_coindcx::auth::{load_credentials, signing_self_test};
use qe_coindcx::live::{CheckItem, CheckStatus, LiveGate};
use qe_coindcx::private::types::Side;
use qe_coindcx::private::PrivateClient;
use qe_coindcx::PublicClient;

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let read_account = args.iter().any(|a| a == "--read-account");
    let json = args.iter().any(|a| a == "--json");

    let (creds, cred_status) = load_credentials();
    let mut gate = LiveGate::from_env(cred_status.configured);
    let r = &mut gate.readiness;

    r.set(
        CheckItem::SigningSelfTest,
        match signing_self_test() {
            Ok(()) => CheckStatus::Pass("RFC 4231 + documented body/socket vectors match".into()),
            Err(e) => CheckStatus::Fail(e),
        },
    );
    if let Some(p) = &cred_status.problem {
        r.set(
            CheckItem::CredentialsConfigured,
            CheckStatus::Fail(p.clone()),
        );
    }

    // Instrument requirements (public data only).
    match (arg_value(&args, "--pair"), arg_value(&args, "--margin")) {
        (Some(pair), Some(margin)) => {
            let st = match PublicClient::new().futures_instrument(&pair, &margin).await {
                Ok(i) => {
                    let mut problems = Vec::new();
                    if i.status != "active" {
                        problems.push(format!("status {}", i.status));
                    }
                    if i.exit_only {
                        problems.push("exit-only".into());
                    }
                    if !(i.max_leverage_long >= 1.0 && i.max_leverage_short >= 1.0) {
                        problems.push("max leverage < 1".into());
                    }
                    if !(i.min_notional > 0.0
                        && i.quantity_increment > 0.0
                        && i.price_increment > 0.0)
                    {
                        problems.push("missing min_notional / increments".into());
                    }
                    let summary = format!(
                        "{pair}/{margin}: max_lev long {} short {}, min_notional {}, qty_inc {}, px_inc {}, taker_fee {}, tiers {}, order_types {:?}",
                        i.max_leverage_long,
                        i.max_leverage_short,
                        i.min_notional,
                        i.quantity_increment,
                        i.price_increment,
                        i.taker_fee,
                        i.dynamic_position_leverage_details.len(),
                        i.order_types
                    );
                    if problems.is_empty() {
                        CheckStatus::Pass(summary)
                    } else {
                        CheckStatus::Fail(format!("{summary}; problems: {}", problems.join(", ")))
                    }
                }
                Err(e) => CheckStatus::Fail(format!("instrument fetch failed: {e}")),
            };
            r.set(CheckItem::InstrumentRequirementsVerified, st);
        }
        _ => r.set(
            CheckItem::InstrumentRequirementsVerified,
            CheckStatus::NotRun("pass --pair <PAIR> --margin <USDT|INR>".into()),
        ),
    }

    // Account reconciliation (authenticated, read-only in effect).
    let account = match (read_account, creds) {
        (false, _) => CheckStatus::NotRun("pass --read-account (owner only)".into()),
        (true, None) => CheckStatus::Fail("credentials not configured".into()),
        (true, Some(c)) => {
            let pc = PrivateClient::new(c, LiveGate::default());
            let mut lines = Vec::new();
            let mut ok = true;
            match pc.wallets().await {
                Ok(ws) => {
                    lines.push(
                        "wallet fetch OK: millisecond timestamp + signature accepted".to_string(),
                    );
                    for w in ws {
                        lines.push(format!(
                            "wallet {}: total {:?} (balance {:?} + locked {:?})",
                            w.currency_short_name.clone().unwrap_or_default(),
                            w.total(),
                            w.balance,
                            w.locked_balance
                        ));
                    }
                }
                Err(e) => {
                    ok = false;
                    lines.push(format!("wallet fetch failed: {e}"));
                }
            }
            match pc.positions(&["USDT", "INR"]).await {
                Ok(ps) => {
                    let open: Vec<String> = ps
                        .iter()
                        .filter(|p| p.is_open() || p.has_pending_orders())
                        .map(|p| {
                            format!("{} {:?}", p.pair.clone().unwrap_or_default(), p.active_pos)
                        })
                        .collect();
                    lines.push(format!(
                        "{} positions, {} with exposure/pending orders",
                        ps.len(),
                        open.len()
                    ));
                    if !open.is_empty() {
                        ok = false;
                        lines.push(format!(
                            "open exposure (flatten before live): {}",
                            open.join("; ")
                        ));
                    }
                }
                Err(e) => {
                    ok = false;
                    lines.push(format!("positions fetch failed: {e}"));
                }
            }
            for side in [Side::Buy, Side::Sell] {
                match pc
                    .list_orders(
                        side,
                        "open,partially_filled,untriggered",
                        1,
                        50,
                        &["USDT", "INR"],
                    )
                    .await
                {
                    Ok(os) if os.is_empty() => {}
                    Ok(os) => {
                        ok = false;
                        lines.push(format!("{} open {} orders", os.len(), side.as_str()));
                    }
                    Err(e) => {
                        ok = false;
                        lines.push(format!("{} orders fetch failed: {e}", side.as_str()));
                    }
                }
            }
            if ok {
                CheckStatus::Pass(lines.join(" | "))
            } else {
                CheckStatus::Fail(lines.join(" | "))
            }
        }
    };
    r.set(CheckItem::AccountReconciled, account);
    r.set(
        CheckItem::ProtectiveOrdersVerified,
        CheckStatus::NotRun(
            "needs a supervised live test of create_tpsl (take_profit_market + stop_market); not automatable read-only".into(),
        ),
    );
    r.set(
        CheckItem::Closure120sTested,
        CheckStatus::NotRun(
            "needs a supervised live open -> exit within 120 s; not automatable read-only".into(),
        ),
    );
    r.set(
        CheckItem::RiskGovernorAcceptance,
        CheckStatus::NotRun(
            "run `cargo test -p qe-risk` and review; not wired into this command".into(),
        ),
    );

    let status = gate.status();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status).unwrap_or_default()
        );
    } else {
        println!("QUANT-ENGINE live readiness (no order is placed by this command)");
        println!("  live-orders feature compiled : {}", status.compiled);
        println!(
            "  execution mode               : {:?}",
            status.execution_mode
        );
        println!(
            "  operator confirmation        : {}",
            status.operator_confirmed
        );
        println!(
            "  credentials                  : {}",
            if cred_status.configured {
                format!("configured ({})", cred_status.source.unwrap_or("?"))
            } else {
                "not configured".into()
            }
        );
        for (item, st) in &status.readiness.items {
            let (tag, detail) = match st {
                CheckStatus::Pass(d) => ("PASS  ", d),
                CheckStatus::Fail(d) => ("FAIL  ", d),
                CheckStatus::NotRun(d) => ("NOTRUN", d),
            };
            println!("  [{tag}] {item:?}: {detail}");
        }
        println!(
            "  LIVE EXECUTION: {}",
            match &status.blocked_by {
                None => "ENABLED".to_string(),
                Some(e) => format!("DISABLED ({e:?})"),
            }
        );
    }
    std::process::exit(if status.readiness.all_pass() { 0 } else { 2 });
}
