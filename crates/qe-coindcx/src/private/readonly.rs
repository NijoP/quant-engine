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
}
