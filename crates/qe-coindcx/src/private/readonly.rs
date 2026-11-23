//! Read-only account path: balances, futures wallets, positions and open orders,
//! separated from order execution. [`ReadOnlyAccount`] owns a [`PrivateClient`]
//! but exposes only reads, so code holding it cannot place, modify or cancel
//! orders. The client is built with a CLOSED live gate, so even the wrapped
//! client's mutations would be refused at runtime.
//!
//! NOT TESTED against the real exchange: the account owner runs it (Stage C).
//! The developers never use credentials.

use super::{FuturesOrderRecord, FuturesPosition, FuturesWallet, PrivateClient, PrivateError};
use super::{Side, SpotBalance};
use crate::auth::Credentials;
use crate::live::LiveGate;

/// Account reads only. Debug output never includes credentials.
pub struct ReadOnlyAccount {
    client: PrivateClient,
}

impl std::fmt::Debug for ReadOnlyAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReadOnlyAccount { credentials: <redacted> }")
    }
}

/// Everything a reconciliation needs, read in one pass.
#[derive(Debug, Clone)]
pub struct AccountSnapshot {
    pub spot: Vec<SpotBalance>,
    pub wallets: Vec<FuturesWallet>,
    pub positions: Vec<FuturesPosition>,
    pub open_orders: Vec<FuturesOrderRecord>,
}

impl ReadOnlyAccount {
    pub fn new(creds: Credentials) -> Self {
        Self {
            client: PrivateClient::new(creds, LiveGate::from_lookup(|_| None, true)),
        }
    }

    /// Point at another base URL (tests: a local mock server).
    pub fn with_base_url(mut self, base: &str) -> Self {
        self.client = self.client.with_base_url(base);
        self
    }

    pub async fn spot_balances(&self) -> Result<Vec<SpotBalance>, PrivateError> {
        self.client.spot_balances().await
    }

    pub async fn futures_wallets(&self) -> Result<Vec<FuturesWallet>, PrivateError> {
        self.client.wallets().await
    }

    pub async fn positions(&self, margin: &[&str]) -> Result<Vec<FuturesPosition>, PrivateError> {
        self.client.positions(margin).await
    }

    pub async fn open_orders(
        &self,
        margin: &[&str],
    ) -> Result<Vec<FuturesOrderRecord>, PrivateError> {
        let mut all = self
            .client
            .list_orders(Side::Buy, "open", 1, 100, margin)
            .await?;
        all.extend(
            self.client
                .list_orders(Side::Sell, "open", 1, 100, margin)
                .await?,
        );
        Ok(all)
    }

    /// One reconciliation pass. Any failed read fails the whole snapshot: a
    /// partial account view must never be treated as complete.
    pub async fn snapshot(&self, margin: &[&str]) -> Result<AccountSnapshot, PrivateError> {
        Ok(AccountSnapshot {
            spot: self.spot_balances().await?,
            wallets: self.futures_wallets().await?,
            positions: self.positions(margin).await?,
            open_orders: self.open_orders(margin).await?,
        })
    }
}

/// Verified INR equity of the INR-margined futures wallet: `balance +
/// locked_balance` of the wallet whose currency is INR. USDT wallets are never
/// converted or added. Missing/duplicate/non-finite INR wallet -> error.
pub fn inr_wallet_equity(wallets: &[FuturesWallet]) -> Result<f64, String> {
    let inr: Vec<&FuturesWallet> = wallets
        .iter()
        .filter(|w| w.currency_short_name.as_deref() == Some("INR"))
        .collect();
    match inr.as_slice() {
        [w] => {
            let b = w.balance.ok_or("INR wallet balance missing")?;
            let l = w.locked_balance.unwrap_or(0.0);
            let e = b + l;
            if e.is_finite() && e >= 0.0 {
                Ok(e)
            } else {
                Err(format!("INR wallet balance not usable: {e}"))
            }
        }
        [] => Err("no INR futures wallet in the account".into()),
        _ => Err("more than one INR futures wallet: ambiguous".into()),
    }
}

/// Redacted, human-readable summary for `qe-desk --readonly-check`.
pub fn summary(s: &AccountSnapshot) -> String {
    let mut out = String::from("READ-ONLY account check (no orders, secrets redacted)\n");
    for w in &s.wallets {
        out += &format!(
            "futures wallet {}: balance {:?} locked {:?}\n",
            w.currency_short_name.as_deref().unwrap_or("?"),
            w.balance,
            w.locked_balance
        );
    }
    out += &format!(
        "INR campaign equity: {}\n",
        match inr_wallet_equity(&s.wallets) {
            Ok(e) => format!("INR {e:.2} (INR futures wallet; USDT not converted)"),
            Err(e) => format!("UNAVAILABLE ({e})"),
        }
    );
    let nz: Vec<&SpotBalance> = s
        .spot
        .iter()
        .filter(|b| b.balance.unwrap_or(0.0) != 0.0)
        .collect();
    out += &format!("spot balances (non-zero): {}\n", nz.len());
    for b in nz {
        out += &format!("  {b:?}\n");
    }
    out += &format!("open positions: {}\n", s.positions.len());
    for p in &s.positions {
        out += &format!("  {p:?}\n");
    }
    out += &format!("open orders: {}\n", s.open_orders.len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let c = Credentials::new("KEY-abc123".into(), "SECRET-xyz789".into()).unwrap();
        let ro = ReadOnlyAccount::new(c);
        let s = format!("{ro:?}");
        assert!(
            !s.contains("KEY-abc123") && !s.contains("SECRET-xyz789"),
            "{s}"
        );
        assert!(s.contains("redacted"));
    }

    fn w(cur: &str, bal: Option<f64>, locked: Option<f64>) -> FuturesWallet {
        FuturesWallet {
            id: None,
            currency_short_name: Some(cur.into()),
            balance: bal,
            locked_balance: locked,
            cross_order_margin: None,
            cross_user_margin: None,
        }
    }

    #[test]
    fn inr_equity_uses_the_inr_wallet_only() {
        let ws = [w("USDT", Some(5.0), None), w("INR", Some(90.0), Some(10.0))];
        assert_eq!(inr_wallet_equity(&ws).unwrap(), 100.0);
        assert!(inr_wallet_equity(&[w("USDT", Some(5.0), None)]).is_err());
        assert!(inr_wallet_equity(&[w("INR", None, None)]).is_err());
        assert!(
            inr_wallet_equity(&[w("INR", Some(1.0), None), w("INR", Some(2.0), None)]).is_err()
        );
    }
}
