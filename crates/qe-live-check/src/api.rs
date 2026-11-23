//! The account operations the owner tools need, as a trait so they run
//! against [`PrivateClient`] in production and an in-memory mock in tests.
use qe_coindcx::private::types::{Ack, NewOrder, Side, SpotBalance};
use qe_coindcx::private::{
    FuturesOrderRecord, FuturesPosition, FuturesWallet, PrivateClient, PrivateError, TpslResult,
};

/// Order statuses that still carry risk or will create it.
pub const LIVE_ORDER_STATUSES: &str = "open,partially_filled,untriggered";

#[allow(async_fn_in_trait)]
pub trait AccountApi {
    async fn wallets(&self) -> Result<Vec<FuturesWallet>, PrivateError>;
    async fn spot_balances(&self) -> Result<Vec<SpotBalance>, PrivateError>;
    async fn positions(&self, margins: &[&str]) -> Result<Vec<FuturesPosition>, PrivateError>;
    /// Open, partially filled and untriggered orders, both sides.
    async fn live_orders(&self, margins: &[&str]) -> Result<Vec<FuturesOrderRecord>, PrivateError>;
    async fn update_leverage(
        &self,
        pair: &str,
        margin: &str,
        lev: u32,
    ) -> Result<Ack, PrivateError>;
    async fn create_order(
        &self,
        client_order_id: &str,
        order: NewOrder,
    ) -> Result<FuturesOrderRecord, PrivateError>;
    async fn create_tpsl(
        &self,
        position_id: &str,
        take_profit: f64,
        stop_loss: f64,
    ) -> Result<TpslResult, PrivateError>;
    async fn exit_position(&self, position_id: &str) -> Result<Ack, PrivateError>;
    async fn cancel_all_for_position(&self, position_id: &str) -> Result<Ack, PrivateError>;
    async fn cancel_all_open_orders(&self, margins: &[&str]) -> Result<Ack, PrivateError>;
}

impl AccountApi for PrivateClient {
    async fn wallets(&self) -> Result<Vec<FuturesWallet>, PrivateError> {
        PrivateClient::wallets(self).await
    }
    async fn spot_balances(&self) -> Result<Vec<SpotBalance>, PrivateError> {
        PrivateClient::spot_balances(self).await
    }
    async fn positions(&self, margins: &[&str]) -> Result<Vec<FuturesPosition>, PrivateError> {
        PrivateClient::positions(self, margins).await
    }
    async fn live_orders(&self, margins: &[&str]) -> Result<Vec<FuturesOrderRecord>, PrivateError> {
        let mut all = Vec::new();
        for side in [Side::Buy, Side::Sell] {
            all.extend(
                self.list_orders(side, LIVE_ORDER_STATUSES, 1, 100, margins)
                    .await?,
            );
        }
        Ok(all)
    }
    async fn update_leverage(
        &self,
        pair: &str,
        margin: &str,
        lev: u32,
    ) -> Result<Ack, PrivateError> {
        PrivateClient::update_leverage(self, pair, margin, lev).await
    }
    async fn create_order(
        &self,
        client_order_id: &str,
        order: NewOrder,
    ) -> Result<FuturesOrderRecord, PrivateError> {
        PrivateClient::create_order(self, client_order_id, order).await
    }
    async fn create_tpsl(
        &self,
        position_id: &str,
        take_profit: f64,
        stop_loss: f64,
    ) -> Result<TpslResult, PrivateError> {
        PrivateClient::create_tpsl(self, position_id, take_profit, stop_loss).await
    }
    async fn exit_position(&self, position_id: &str) -> Result<Ack, PrivateError> {
        PrivateClient::exit_position(self, position_id).await
    }
    async fn cancel_all_for_position(&self, position_id: &str) -> Result<Ack, PrivateError> {
        PrivateClient::cancel_all_for_position(self, position_id).await
    }
    async fn cancel_all_open_orders(&self, margins: &[&str]) -> Result<Ack, PrivateError> {
        PrivateClient::cancel_all_open_orders(self, margins).await
    }
}
