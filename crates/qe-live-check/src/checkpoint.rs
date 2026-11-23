//! Checkpoint tooling (3) for 22:30 IST (Stage 1) and 17:00 IST (Stage 2):
//! cancel all orders, exit every position, verify flat, read the ACTUAL
//! balances and write a reconciliation report. Plus post-withdrawal balance
//! reconciliation: the owner withdraws manually; this module only reads.
//!
//! Nothing here assumes a target was met: every figure is an exchange read
//! taken at `at_ms`, and the report says so.
use crate::api::AccountApi;
use crate::money::dec;
use crate::readiness::{snapshot, MARGINS};
use crate::Poll;
use qe_coindcx::private::PrivateError;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlattenReport {
    pub actions: Vec<String>,
    pub errors: Vec<String>,
    pub flat: bool,
    /// What was still open at the last read (empty when flat).
    pub residual: Vec<String>,
}

/// Cancel all + exit all + verify flat. Every step is attempted even if an
/// earlier one fails; `flat` is set only by a fresh exchange read.
pub async fn flatten_all<A: AccountApi>(api: &A, margins: &[&str], poll: Poll) -> FlattenReport {
    let mut rep = FlattenReport {
        actions: vec![],
        errors: vec![],
        flat: false,
        residual: vec![],
    };
    match api.cancel_all_open_orders(margins).await {
        Ok(a) => rep.actions.push(format!(
            "cancel_all_open_orders {margins:?}: {}",
            if a.is_success() {
                "success"
            } else {
                "not acknowledged"
            }
        )),
        Err(e) => rep.errors.push(format!("cancel_all_open_orders: {e}")),
    }
    match api.positions(margins).await {
        Err(e) => rep.errors.push(format!("positions read: {e}")),
        Ok(ps) => {
            for p in ps.iter().filter(|p| p.is_open() || p.has_pending_orders()) {
                let Some(id) = p.id.clone() else {
                    rep.errors.push("open position without id".into());
                    continue;
                };
                let pair = p.pair.clone().unwrap_or_default();
                match api.cancel_all_for_position(&id).await {
                    Ok(_) => rep.actions.push(format!("cancel_all_for_position {pair}")),
                    Err(e) => rep
                        .errors
                        .push(format!("cancel_all_for_position {pair}: {e}")),
                }
                if p.is_open() {
                    match api.exit_position(&id).await {
                        Ok(a) => rep.actions.push(format!(
                            "exit_position {pair} qty {:?} group {:?}",
                            p.active_pos,
                            a.group_id()
                        )),
                        // OutcomeUnknown: the verify loop below decides; never re-send blindly.
                        Err(e) => rep.errors.push(format!("exit_position {pair}: {e}")),
                    }
                }
            }
        }
    }
    for attempt in 0..poll.attempts.max(1) {
        if attempt > 0 {
            tokio::time::sleep(poll.interval).await;
        }
        match snapshot(api, margins, 0).await {
            Ok(s) if s.is_flat() => {
                rep.flat = true;
                rep.residual.clear();
                break;
            }
            Ok(s) => {
                rep.residual = s
                    .exposed()
                    .iter()
                    .map(|p| format!("{} {:?}", p.pair.clone().unwrap_or_default(), p.active_pos))
                    .chain(s.live_orders.iter().map(|o| {
                        format!(
                            "order {} {} {}",
                            o.pair.clone().unwrap_or_default(),
                            o.order_type.clone().unwrap_or_default(),
                            o.status.clone().unwrap_or_default()
                        )
                    }))
                    .collect();
            }
            Err(e) => rep.residual = vec![format!("verify read failed: {e}")],
        }
    }
    rep
}

/// Actual balances per currency, futures and spot kept separate; currencies
/// are never converted or mixed.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Balances {
    pub at_ms: i64,
    /// Futures wallet total (= balance + locked_balance, per docs).
    pub futures: BTreeMap<String, Decimal>,
    /// Spot balance + locked_balance.
    pub spot: BTreeMap<String, Decimal>,
}

impl Balances {
    pub fn total(&self, ccy: &str) -> Decimal {
        self.futures.get(ccy).copied().unwrap_or_default()
            + self.spot.get(ccy).copied().unwrap_or_default()
    }
}

pub async fn read_balances<A: AccountApi>(api: &A, now_ms: i64) -> Result<Balances, PrivateError> {
    let mut b = Balances {
        at_ms: now_ms,
        ..Default::default()
    };
    for w in api.wallets().await? {
        let ccy = w.currency_short_name.clone().unwrap_or_default();
        let total = w
            .total()
            .and_then(dec)
            .ok_or_else(|| PrivateError::Decode {
                message: format!("futures wallet {ccy}: balance missing"),
            })?;
        *b.futures.entry(ccy).or_default() += total;
    }
    for s in api.spot_balances().await? {
        let ccy = s.currency.clone().unwrap_or_default();
        let total = s
            .balance
            .and_then(dec)
            .ok_or_else(|| PrivateError::Decode {
                message: format!("spot {ccy}: balance missing"),
            })?
            + s.locked_balance.and_then(dec).unwrap_or_default();
        if total != Decimal::ZERO {
            *b.spot.entry(ccy).or_default() += total;
        }
    }
    Ok(b)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    /// 22:30 IST: end of Stage 1 + INR 2,000 withdrawal checkpoint.
    Stage1,
    /// 17:00 IST next day: final.
    Stage2,
    Adhoc,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckpointReport {
    pub kind: CheckpointKind,
    pub at_ms: i64,
    pub flatten: FlattenReport,
    pub balances: Option<Balances>,
    pub balance_error: Option<String>,
    pub currency: String,
    /// Realised equity the desk's own ledger expects (owner passes it in).
    pub desk_expected: Option<Decimal>,
    /// actual - desk_expected.
    pub difference: Option<Decimal>,
    pub withdrawal: Option<WithdrawalPrep>,
    pub verdict: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WithdrawalPrep {
    pub planned: Decimal,
    pub actual_total: Decimal,
    pub in_spot: Decimal,
    pub in_futures_wallet: Decimal,
    pub possible: bool,
    pub instructions: Vec<String>,
}

pub fn prepare_withdrawal(b: &Balances, ccy: &str, planned: Decimal) -> WithdrawalPrep {
    let in_spot = b.spot.get(ccy).copied().unwrap_or_default();
    let in_fut = b.futures.get(ccy).copied().unwrap_or_default();
    let total = in_spot + in_fut;
    let possible = total >= planned;
    let mut instructions = vec![format!(
        "Actual {ccy} balance at this checkpoint: {total} (spot {in_spot}, futures wallet {in_fut})."
    )];
    if possible {
        instructions.push(format!(
            "If you choose to withdraw {ccy} {planned}: move funds from the futures wallet to spot (if needed) and withdraw IN THE COINDCX APP yourself. This tool never transfers or withdraws."
        ));
        instructions.push(
            "Afterwards run `qe-live-check reconcile-withdrawal --before <this report> --amount <what you withdrew>`; Stage 2 starts from the balance it confirms.".into(),
        );
    } else {
        instructions.push(format!(
            "Target NOT reached: balance {total} < planned withdrawal {planned}. Do not assume it was. Risk limits stay unchanged; Stage 2 must start from the actual balance."
        ));
    }
    WithdrawalPrep {
        planned,
        actual_total: total,
        in_spot,
        in_futures_wallet: in_fut,
        possible,
        instructions,
    }
}

pub fn build_report(
    kind: CheckpointKind,
    at_ms: i64,
    flatten: FlattenReport,
    balances: Result<Balances, PrivateError>,
    ccy: &str,
    desk_expected: Option<Decimal>,
    planned_withdrawal: Option<Decimal>,
) -> CheckpointReport {
    let (balances, balance_error) = match balances {
        Ok(b) => (Some(b), None),
        Err(e) => (None, Some(e.to_string())),
    };
    let actual = balances.as_ref().map(|b| b.total(ccy));
    let difference = actual.zip(desk_expected).map(|(a, e)| a - e);
    let withdrawal = match (kind, &balances) {
        (CheckpointKind::Stage1, Some(b)) => Some(prepare_withdrawal(
            b,
            ccy,
            planned_withdrawal.unwrap_or(Decimal::from(2000)),
        )),
        _ => None,
    };
    let verdict = match (&flatten.flat, &actual, &difference) {
        (false, _, _) => "NOT FLAT: positions or orders remain. Resolve in the CoinDCX app, then re-run. Do not trade.".to_string(),
        (true, None, _) => "Flat, but the balance read FAILED: actual equity is unknown; nothing is assumed.".to_string(),
        (true, Some(a), None) => format!("Flat. Actual {ccy} balance {a} (exchange read). No desk expectation given."),
        (true, Some(a), Some(d)) if d.abs() <= Decimal::new(1, 2) => {
            format!("Flat. Actual {ccy} balance {a} reconciles with the desk ledger (difference {d}).")
        }
        (true, Some(a), Some(d)) => format!(
            "Flat. Actual {ccy} balance {a} DIFFERS from the desk ledger by {d}: investigate fees/funding/fills before Stage 2. The actual balance governs."
        ),
    };
    CheckpointReport {
        kind,
        at_ms,
        flatten,
        balances,
        balance_error,
        currency: ccy.into(),
        desk_expected,
        difference,
        withdrawal,
        verdict,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WithdrawalReconciliation {
    pub at_ms: i64,
    pub currency: String,
    pub before: Decimal,
    pub withdrawn: Decimal,
    pub fee: Decimal,
    pub expected_after: Decimal,
    pub actual_after: Decimal,
    pub difference: Decimal,
    pub confirmed: bool,
    /// The capital Stage 2 starts from: always the ACTUAL balance.
    pub retained_capital: Decimal,
    pub verdict: String,
}

/// Compare balances read before and after the owner's manual withdrawal.
pub fn reconcile_withdrawal(
    before: &Balances,
    after: &Balances,
    ccy: &str,
    withdrawn: Decimal,
    fee: Decimal,
    tolerance: Decimal,
) -> WithdrawalReconciliation {
    let b = before.total(ccy);
    let a = after.total(ccy);
    let expected = b - withdrawn - fee;
    let diff = a - expected;
    let confirmed = diff.abs() <= tolerance;
    let verdict = if confirmed {
        format!(
            "Withdrawal of {ccy} {withdrawn} is consistent with balances: {b} -> {a}. Stage 2 capital = {a} (actual)."
        )
    } else {
        format!(
            "NOT CONFIRMED: expected {expected} after withdrawing {withdrawn} (fee {fee}), actual {a} (difference {diff}). Do not assume the withdrawal happened as planned; Stage 2 capital = actual {a}."
        )
    };
    WithdrawalReconciliation {
        at_ms: after.at_ms,
        currency: ccy.into(),
        before: b,
        withdrawn,
        fee,
        expected_after: expected,
        actual_after: a,
        difference: diff,
        confirmed,
        retained_capital: a,
        verdict,
    }
}

/// Convenience for the CLI.
pub fn default_margins() -> [&'static str; 2] {
    MARGINS
}
