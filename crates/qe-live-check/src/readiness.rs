//! Readiness check (1): credentials configured (reported only as configured /
//! not), signing self-test, account snapshot (wallets, positions, live
//! orders) reconciled against what the desk believes is open, and public
//! instrument requirements. Read-only in effect: no order is placed.
use crate::api::AccountApi;
use crate::money::dec;
use qe_coindcx::auth::{signing_self_test, CredentialStatus};
use qe_coindcx::live::{CheckItem, CheckStatus};
use qe_coindcx::private::{FuturesOrderRecord, FuturesPosition, FuturesWallet, PrivateError};
use qe_coindcx::types::FuturesInstrument;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

pub const MARGINS: [&str; 2] = ["USDT", "INR"];

/// What the desk believes is open on the exchange. The desk writes this to
/// `<state>/desk_state.json`; absent means "the desk believes it is flat".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeskState {
    /// Pairs the desk believes hold live exposure, with signed quantity.
    #[serde(default)]
    pub open: Vec<DeskPosition>,
    #[serde(default)]
    pub written_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeskPosition {
    pub pair: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub quantity: Decimal,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountSnapshot {
    pub at_ms: i64,
    pub wallets: Vec<FuturesWallet>,
    pub positions: Vec<FuturesPosition>,
    pub live_orders: Vec<FuturesOrderRecord>,
}

impl AccountSnapshot {
    pub fn exposed(&self) -> Vec<&FuturesPosition> {
        self.positions
            .iter()
            .filter(|p| p.is_open() || p.has_pending_orders())
            .collect()
    }
    pub fn is_flat(&self) -> bool {
        self.exposed().is_empty() && self.live_orders.is_empty()
    }
}

pub async fn snapshot<A: AccountApi>(
    api: &A,
    margins: &[&str],
    now_ms: i64,
) -> Result<AccountSnapshot, PrivateError> {
    Ok(AccountSnapshot {
        at_ms: now_ms,
        wallets: api.wallets().await?,
        positions: api.positions(margins).await?,
        live_orders: api.live_orders(margins).await?,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct Reconciliation {
    pub ok: bool,
    /// On the exchange, not in the desk's book.
    pub unexpected: Vec<String>,
    /// In the desk's book, not on the exchange.
    pub missing: Vec<String>,
    /// Both sides know the pair but quantities differ.
    pub quantity_mismatch: Vec<String>,
    pub live_orders: usize,
    pub lines: Vec<String>,
}

/// Compare exchange positions with the desk's belief. Live orders are
/// reported; with an otherwise flat book they also fail reconciliation,
/// because the desk never leaves orders behind when it believes it is flat.
pub fn reconcile(snap: &AccountSnapshot, desk: &DeskState) -> Reconciliation {
    let mut r = Reconciliation {
        ok: true,
        unexpected: vec![],
        missing: vec![],
        quantity_mismatch: vec![],
        live_orders: snap.live_orders.len(),
        lines: vec![],
    };
    for p in snap.exposed() {
        let pair = p.pair.clone().unwrap_or_default();
        let qty = p.active_pos.and_then(dec).unwrap_or_default();
        match desk.open.iter().find(|d| d.pair == pair) {
            None => r.unexpected.push(format!("{pair} active_pos {qty}")),
            Some(d) if d.quantity != qty => r
                .quantity_mismatch
                .push(format!("{pair}: exchange {qty}, desk {}", d.quantity)),
            Some(_) => {}
        }
    }
    for d in &desk.open {
        let on_exchange = snap
            .exposed()
            .iter()
            .any(|p| p.pair.as_deref() == Some(d.pair.as_str()) && p.is_open());
        if !on_exchange {
            r.missing
                .push(format!("{} desk qty {}", d.pair, d.quantity));
        }
    }
    if desk.open.is_empty() && r.live_orders > 0 {
        r.lines.push(format!(
            "{} live (open/untriggered) orders while the desk believes it is flat",
            r.live_orders
        ));
        r.ok = false;
    }
    if !(r.unexpected.is_empty() && r.missing.is_empty() && r.quantity_mismatch.is_empty()) {
        r.ok = false;
    }
    for w in &snap.wallets {
        r.lines.push(format!(
            "wallet {}: total {}",
            w.currency_short_name.clone().unwrap_or_default(),
            w.total()
                .and_then(dec)
                .map(|d| d.to_string())
                .unwrap_or_else(|| "unknown".into())
        ));
    }
    r.lines.push(format!(
        "{} positions listed, {} with exposure/pending; unexpected {:?}; missing {:?}; mismatched {:?}",
        snap.positions.len(),
        snap.exposed().len(),
        r.unexpected,
        r.missing,
        r.quantity_mismatch
    ));
    r
}

/// Public instrument requirements for trading `pair` at all.
pub fn instrument_check(i: &FuturesInstrument) -> CheckStatus {
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
    if !(i.min_notional > 0.0 && i.quantity_increment > 0.0 && i.price_increment > 0.0) {
        problems.push("missing min_notional / increments".into());
    }
    for t in ["market_order", "take_profit_market", "stop_market"] {
        if !i.order_types.iter().any(|o| o == t) {
            problems.push(format!("order type {t} not listed"));
        }
    }
    let summary = format!(
        "{}/{}: max_lev long {} short {}, min_qty {}, min_notional {}, qty_inc {}, px_inc {}, taker_fee {}, tiers {}",
        i.pair,
        i.margin_currency_short_name,
        i.max_leverage_long,
        i.max_leverage_short,
        i.min_quantity,
        i.min_notional,
        i.quantity_increment,
        i.price_increment,
        i.taker_fee,
        i.dynamic_position_leverage_details.len()
    );
    if problems.is_empty() {
        CheckStatus::Pass(summary)
    } else {
        CheckStatus::Fail(format!("{summary}; problems: {}", problems.join(", ")))
    }
}

pub fn credentials_check(c: &CredentialStatus) -> CheckStatus {
    match (c.configured, &c.problem) {
        (true, _) => CheckStatus::Pass(format!("configured ({})", c.source.unwrap_or("?"))),
        (false, Some(p)) => CheckStatus::Fail(format!("not configured: {p}")),
        (false, None) => CheckStatus::Fail("not configured".into()),
    }
}

pub fn signing_check() -> CheckStatus {
    match signing_self_test() {
        Ok(()) => CheckStatus::Pass("RFC 4231 + documented body/socket vectors match".into()),
        Err(e) => CheckStatus::Fail(e),
    }
}

/// Account part of the readiness check. A failed fetch is a `Fail`, never a
/// fabricated balance.
pub async fn account_check<A: AccountApi>(
    api: &A,
    desk: &DeskState,
    now_ms: i64,
) -> (CheckStatus, Option<AccountSnapshot>) {
    match snapshot(api, &MARGINS, now_ms).await {
        Err(e) => (CheckStatus::Fail(format!("account read failed: {e}")), None),
        Ok(s) => {
            let r = reconcile(&s, desk);
            let mut lines = vec![
                "wallet/positions/orders read OK (timestamp + signature accepted)".to_string(),
            ];
            lines.extend(r.lines.clone());
            let st = if r.ok {
                CheckStatus::Pass(lines.join(" | "))
            } else {
                CheckStatus::Fail(lines.join(" | "))
            };
            (st, Some(s))
        }
    }
}

/// What each readiness item proves, for status/readiness output. The
/// serialized keys (`checks.json`, `armed.json`) are unchanged.
pub fn check_label(item: CheckItem) -> &'static str {
    match item {
        CheckItem::CredentialsConfigured => "credentials configured",
        CheckItem::SigningSelfTest => "signing self-test",
        CheckItem::AccountReconciled => "account reconciled",
        CheckItem::InstrumentRequirementsVerified => "instrument requirements verified",
        CheckItem::ProtectiveOrdersVerified => "protective orders verified (validation)",
        CheckItem::Closure120sTested => crate::validate::CLOSURE_CHECK_LABEL,
        CheckItem::RiskGovernorAcceptance => "risk-governor acceptance",
    }
}

/// Item list for the readiness command.
pub fn checklist_order() -> [CheckItem; 7] {
    CheckItem::ALL
}
