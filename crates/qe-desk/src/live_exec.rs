//! Live (real-money) execution path for the desk.
//!
//! Paper remains the default. This executor acts only when the venue reports
//! the live gate as armed (execution_mode=live + owner confirmation + recent
//! passing readiness checks + credentials + valid arm record from
//! `qe-live-check arm`), and only on proposals the risk governor approved.
//!
//! Lifecycle (one position at a time):
//! gate/kill check -> leverage (never above governor and exchange max) ->
//! entry with idempotent client order id -> confirm fill (the 120 000 ms timer
//! starts at the CONFIRMED fill) -> cancel any unfilled remainder (partial
//! fill) -> exchange-side TP+SL, both legs must be accepted, else emergency
//! exit + incident -> monitor -> protective exit, kill switch or 120 s time
//! exit -> cancel leftovers -> verify flat -> read the real wallet balance.
//!
//! An entry with an unknown outcome is reconciled against the exchange before
//! anything else; it is never blindly resubmitted.
use qe_coindcx::private::{
    FuturesPosition, FuturesWallet, LedgerState, NewOrder, PrivateClient, PrivateError, Side,
    TpslResult,
};
use serde::Serialize;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// Position lifetime from the confirmed fill (spec: 120 s).
pub const LIFETIME_MS: u64 = 120_000;

/// The exchange operations the executor needs. `PrivateClient` implements it;
/// tests use an in-memory mock.
pub trait Venue: Send + Sync {
    fn armed(&self) -> bool;
    fn update_leverage(
        &self,
        pair: &str,
        margin: &str,
        lev: u32,
    ) -> impl Future<Output = Result<(), PrivateError>> + Send;
    /// Returns the exchange order id.
    fn create_order(
        &self,
        coid: &str,
        o: NewOrder,
    ) -> impl Future<Output = Result<String, PrivateError>> + Send;
    fn reconcile_order(
        &self,
        coid: &str,
    ) -> impl Future<Output = Result<LedgerState, PrivateError>> + Send;
    fn cancel_order(&self, id: &str) -> impl Future<Output = Result<(), PrivateError>> + Send;
    fn position(
        &self,
        pair: &str,
        margin: &str,
    ) -> impl Future<Output = Result<Option<FuturesPosition>, PrivateError>> + Send;
    fn create_tpsl(
        &self,
        position_id: &str,
        tp: f64,
        sl: f64,
    ) -> impl Future<Output = Result<TpslResult, PrivateError>> + Send;
    fn exit_position(
        &self,
        position_id: &str,
    ) -> impl Future<Output = Result<(), PrivateError>> + Send;
    fn cancel_all_for_position(
        &self,
        position_id: &str,
    ) -> impl Future<Output = Result<(), PrivateError>> + Send;
    fn wallets(&self) -> impl Future<Output = Result<Vec<FuturesWallet>, PrivateError>> + Send;
}

impl Venue for PrivateClient {
    fn armed(&self) -> bool {
        self.gate().check().is_ok()
    }
    async fn update_leverage(
        &self,
        pair: &str,
        margin: &str,
        lev: u32,
    ) -> Result<(), PrivateError> {
        PrivateClient::update_leverage(self, pair, margin, lev)
            .await
            .map(|_| ())
    }
    async fn create_order(&self, coid: &str, o: NewOrder) -> Result<String, PrivateError> {
        PrivateClient::create_order(self, coid, o)
            .await
            .map(|r| r.id.unwrap_or_default())
    }
    async fn reconcile_order(&self, coid: &str) -> Result<LedgerState, PrivateError> {
        PrivateClient::reconcile_order(self, coid).await
    }
    async fn cancel_order(&self, id: &str) -> Result<(), PrivateError> {
        PrivateClient::cancel_order(self, id).await.map(|_| ())
    }
    async fn position(
        &self,
        pair: &str,
        margin: &str,
    ) -> Result<Option<FuturesPosition>, PrivateError> {
        Ok(self
            .positions_filtered(&[margin], Some(pair), None)
            .await?
            .into_iter()
            .find(|p| p.pair.as_deref() == Some(pair)))
    }
    async fn create_tpsl(&self, pid: &str, tp: f64, sl: f64) -> Result<TpslResult, PrivateError> {
        PrivateClient::create_tpsl(self, pid, tp, sl).await
    }
    async fn exit_position(&self, pid: &str) -> Result<(), PrivateError> {
        PrivateClient::exit_position(self, pid).await.map(|_| ())
    }
    async fn cancel_all_for_position(&self, pid: &str) -> Result<(), PrivateError> {
        PrivateClient::cancel_all_for_position(self, pid)
            .await
            .map(|_| ())
    }
    async fn wallets(&self) -> Result<Vec<FuturesWallet>, PrivateError> {
        PrivateClient::wallets(self).await
    }
}

/// A governor-approved proposal, already sized by the risk engine.
#[derive(Debug, Clone, Serialize)]
pub struct LiveOrderRequest {
    pub trade_id: String,
    pub pair: String,
    pub margin: String,
    pub long: bool,
    pub qty: f64,
    pub ref_price: f64,
    pub stop_loss: f64,
    pub take_profit: f64,
    /// Leverage the governor approved.
    pub governor_leverage: u32,
    /// Exchange max for this instrument / tier from live metadata.
    pub exchange_max_leverage: Option<u32>,
    pub governor_approved: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveState {
    Refused,
    Rejected,
    NoFill,
    ReconciliationRequired,
    EmergencyClosed,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    ProtectiveOrder,
    TimeExit,
    KillSwitch,
    ProtectionFailed,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveTradeReport {
    pub trade_id: String,
    pub client_order_id: String,
    pub state: LiveState,
    pub exit_reason: Option<ExitReason>,
    pub leverage: Option<u32>,
    pub requested_qty: f64,
    pub filled_qty: f64,
    pub avg_entry: Option<f64>,
    pub fill_confirmed_ms_after_submit: Option<u64>,
    pub held_ms: Option<u64>,
    pub flat_verified: bool,
    /// Real futures wallet balance after the trade (never paper equity).
    pub wallet_balance_after: Option<f64>,
    pub incidents: Vec<String>,
    pub events: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExecConfig {
    pub lifetime: Duration,
    pub fill_timeout: Duration,
    pub poll: Duration,
    pub reconcile_attempts: u32,
}

impl Default for ExecConfig {
    fn default() -> Self {
        Self {
            lifetime: Duration::from_millis(LIFETIME_MS),
            fill_timeout: Duration::from_secs(5),
            poll: Duration::from_millis(500),
            reconcile_attempts: 3,
        }
    }
}

pub struct LiveExecutor<V: Venue> {
    venue: V,
    cfg: ExecConfig,
    kill: Arc<AtomicBool>,
    busy: AtomicBool,
}

fn signed_qty(p: &FuturesPosition) -> f64 {
    p.active_pos.unwrap_or(0.0)
}

impl<V: Venue> LiveExecutor<V> {
    pub fn new(venue: V, cfg: ExecConfig) -> Self {
        Self {
            venue,
            cfg,
            kill: Arc::default(),
            busy: AtomicBool::new(false),
        }
    }
    /// Shared kill switch: once set, no new entries; an open position is flattened.
    pub fn kill_switch(&self) -> Arc<AtomicBool> {
        self.kill.clone()
    }
    pub fn killed(&self) -> bool {
        self.kill.load(Ordering::SeqCst)
    }

    /// Run one trade end to end. Never opens a second position concurrently.
    pub async fn execute(&self, req: &LiveOrderRequest) -> LiveTradeReport {
        let mut r = self.refused(req);
        let coid = r.client_order_id.clone();
        if let Some(why) = self.refusal(req) {
            r.events.push(format!("refused: {why}"));
            return r;
        }
        if self.busy.swap(true, Ordering::SeqCst) {
            r.events
                .push("refused: a live position is already open".into());
            return r;
        }
        self.run(req, &coid, &mut r).await;
        self.busy.store(false, Ordering::SeqCst);
        r
    }

    fn refused(&self, req: &LiveOrderRequest) -> LiveTradeReport {
        LiveTradeReport {
            trade_id: req.trade_id.clone(),
            client_order_id: format!("qe-{}", req.trade_id),
            state: LiveState::Refused,
            exit_reason: None,
            leverage: None,
            requested_qty: req.qty,
            filled_qty: 0.0,
            avg_entry: None,
            fill_confirmed_ms_after_submit: None,
            held_ms: None,
            flat_verified: false,
            wallet_balance_after: None,
            incidents: vec![],
            events: vec![],
        }
    }

    fn refusal(&self, req: &LiveOrderRequest) -> Option<&'static str> {
        let sl_ok = if req.long {
            req.stop_loss < req.ref_price && req.take_profit > req.ref_price
        } else {
            req.stop_loss > req.ref_price && req.take_profit < req.ref_price
        };
        if self.killed() {
            Some("kill switch set")
        } else if !self.venue.armed() {
            Some("live gate not armed")
        } else if !req.governor_approved {
            Some("risk governor did not approve")
        } else if !(req.qty.is_finite() && req.qty > 0.0) {
            Some("non-positive quantity")
        } else if !sl_ok {
            Some("stop loss / take profit missing or on the wrong side")
        } else if req.governor_leverage < 1 {
            Some("governor leverage < 1")
        } else {
            None
        }
    }

    async fn run(&self, req: &LiveOrderRequest, coid: &str, r: &mut LiveTradeReport) {
        // Leverage: never above what the governor approved or the exchange allows.
        let lev = req
            .exchange_max_leverage
            .map_or(req.governor_leverage, |m| m.min(req.governor_leverage))
            .max(1);
        if let Err(e) = self
            .venue
            .update_leverage(&req.pair, &req.margin, lev)
            .await
        {
            r.state = LiveState::Rejected;
            r.events
                .push(format!("leverage {lev}x refused: {e:?}; no order sent"));
            return;
        }
        r.leverage = Some(lev);
        let side = if req.long { Side::Buy } else { Side::Sell };
        let order = NewOrder::market(side, &req.pair, req.qty, lev, &req.margin);
        let submitted = Instant::now();
        let order_id = match self.venue.create_order(coid, order).await {
            Ok(id) => id,
            Err(PrivateError::OutcomeUnknown { message }) => {
                r.events
                    .push(format!("entry outcome unknown ({message}); reconciling"));
                match self.reconcile(coid, r).await {
                    Some(id) => id,
                    None => return,
                }
            }
            Err(e) => {
                r.state = LiveState::Rejected;
                r.events.push(format!("entry rejected: {e:?}"));
                return;
            }
        };
        r.events.push(format!("entry accepted id={order_id}"));

        // Confirm the fill from exchange position state.
        let target = if req.long { req.qty } else { -req.qty };
        let fill_deadline = submitted + self.cfg.fill_timeout;
        let mut pos: Option<FuturesPosition> = None;
        loop {
            match self.venue.position(&req.pair, &req.margin).await {
                Ok(Some(p)) if p.is_open() => {
                    let full = (signed_qty(&p) - target).abs() < 1e-12;
                    pos = Some(p);
                    if full {
                        break;
                    }
                }
                Ok(_) => {}
                Err(e) => r.events.push(format!("position read failed: {e:?}")),
            }
            if Instant::now() >= fill_deadline {
                break;
            }
            tokio::time::sleep(self.cfg.poll).await;
        }
        let filled_at = Instant::now();
        let filled = pos.as_ref().map_or(0.0, |p| signed_qty(p).abs());
        if filled < req.qty {
            // Never leave a working remainder: cancel it before going further.
            if let Err(e) = self.venue.cancel_order(&order_id).await {
                r.incidents
                    .push(format!("cancel of unfilled remainder failed: {e:?}"));
            }
        }
        let Some(p) = pos.filter(|_| filled > 0.0) else {
            r.state = LiveState::NoFill;
            r.events
                .push("no fill before fill timeout; remainder cancelled".into());
            r.flat_verified = self.verify_flat(req, r).await;
            r.wallet_balance_after = self.balance(&req.margin).await;
            return;
        };
        r.filled_qty = filled;
        r.avg_entry = p.avg_price;
        r.fill_confirmed_ms_after_submit = Some((filled_at - submitted).as_millis() as u64);
        if filled < req.qty {
            r.events.push(format!("partial fill {filled}/{}", req.qty));
        }
        let pid = p.id.clone().unwrap_or_default();

        // Exchange-side protection: both legs or emergency exit.
        let protected = match self
            .venue
            .create_tpsl(&pid, req.take_profit, req.stop_loss)
            .await
        {
            Ok(t) if t.fully_protected() => true,
            Ok(t) => {
                r.incidents.push(format!("protection incomplete: {t:?}"));
                false
            }
            Err(e) => {
                r.incidents.push(format!("protection failed: {e:?}"));
                false
            }
        };
        let deadline = filled_at + self.cfg.lifetime;
        let reason = if !protected {
            ExitReason::ProtectionFailed
        } else {
            r.events.push("protected: TP and SL accepted".into());
            self.monitor(req, deadline, r).await
        };
        r.exit_reason = Some(reason);
        if reason != ExitReason::ProtectiveOrder {
            if let Err(e) = self.venue.exit_position(&pid).await {
                r.incidents
                    .push(format!("exit_position error: {e:?}; verifying state"));
            }
        }
        if let Err(e) = self.venue.cancel_all_for_position(&pid).await {
            r.incidents.push(format!("cancel leftovers failed: {e:?}"));
        }
        r.held_ms = Some(filled_at.elapsed().as_millis() as u64);
        r.flat_verified = self.verify_flat(req, r).await;
        r.state = match (reason, r.flat_verified) {
            (_, false) => LiveState::ReconciliationRequired,
            (ExitReason::ProtectionFailed, true) => LiveState::EmergencyClosed,
            _ => LiveState::Closed,
        };
        r.wallet_balance_after = self.balance(&req.margin).await;
    }

    async fn reconcile(&self, coid: &str, r: &mut LiveTradeReport) -> Option<String> {
        for i in 0..self.cfg.reconcile_attempts {
            match self.venue.reconcile_order(coid).await {
                Ok(LedgerState::Reconciled { exchange_order_id })
                | Ok(LedgerState::Acked { exchange_order_id }) => {
                    r.events
                        .push(format!("reconciled: order exists id={exchange_order_id}"));
                    return Some(exchange_order_id);
                }
                Ok(LedgerState::ConfirmedAbsent) | Ok(LedgerState::Rejected { .. }) => {
                    r.state = LiveState::Rejected;
                    r.events
                        .push("reconciled: order never reached the book; not resubmitted".into());
                    return None;
                }
                Ok(s) => r.events.push(format!("reconcile attempt {i}: still {s:?}")),
                Err(e) => r
                    .events
                    .push(format!("reconcile attempt {i} failed: {e:?}")),
            }
            tokio::time::sleep(self.cfg.poll).await;
        }
        r.state = LiveState::ReconciliationRequired;
        r.incidents.push(
            "entry outcome unresolved; desk must stay blocked until the owner reconciles".into(),
        );
        None
    }

    async fn monitor(
        &self,
        req: &LiveOrderRequest,
        deadline: Instant,
        r: &mut LiveTradeReport,
    ) -> ExitReason {
        loop {
            if self.killed() {
                return ExitReason::KillSwitch;
            }
            if Instant::now() >= deadline {
                return ExitReason::TimeExit;
            }
            match self.venue.position(&req.pair, &req.margin).await {
                Ok(Some(p)) if p.is_open() => {}
                Ok(_) => return ExitReason::ProtectiveOrder,
                Err(e) => r.events.push(format!("monitor read failed: {e:?}")),
            }
            let left = deadline.saturating_duration_since(Instant::now());
            tokio::time::sleep(self.cfg.poll.min(left)).await;
        }
    }

    async fn verify_flat(&self, req: &LiveOrderRequest, r: &mut LiveTradeReport) -> bool {
        for _ in 0..5 {
            match self.venue.position(&req.pair, &req.margin).await {
                Ok(p)
                    if !p
                        .as_ref()
                        .is_some_and(|p| p.is_open() || p.has_pending_orders()) =>
                {
                    return true
                }
                Ok(_) => {}
                Err(e) => r.events.push(format!("flat check read failed: {e:?}")),
            }
            tokio::time::sleep(self.cfg.poll).await;
        }
        r.incidents
            .push("could not verify flat: reconciliation required".into());
        false
    }

    async fn balance(&self, margin: &str) -> Option<f64> {
        self.venue
            .wallets()
            .await
            .ok()?
            .into_iter()
            .find(|w| w.currency_short_name.as_deref() == Some(margin))
            .and_then(|w| w.total())
    }
}

// ------------------------------------------------------------ desk wiring

/// One governor-approved order handed from the (sync) desk loop to the live task.
#[derive(Debug, Clone)]
pub struct LiveJob {
    pub req: LiveOrderRequest,
    /// Risk at stop in the margin currency, as sized by the governor.
    pub risk_at_stop: f64,
    /// Governor's per-trade risk fraction (1% growth / 0.5% protective).
    pub risk_fraction: f64,
    /// When the governor approved it; older than [`MAX_JOB_AGE_MS`] is dropped.
    pub created_ms: i64,
}

/// A proposal queued behind an open trade is stale by the time it is read.
pub const MAX_JOB_AGE_MS: i64 = 2_000;

static LIVE_TX: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<LiveJob>> =
    std::sync::OnceLock::new();
static LIVE_REPORTS: std::sync::Mutex<Vec<LiveTradeReport>> = std::sync::Mutex::new(Vec::new());

/// Hand an approved order to the live executor. Returns `false` (the caller
/// keeps the paper path) unless the live task was installed at start-up.
pub fn submit(job: LiveJob) -> bool {
    LIVE_TX.get().is_some_and(|tx| tx.send(job).is_ok())
}

static STALE_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn stale_dropped() -> u64 {
    STALE_DROPPED.load(Ordering::Relaxed)
}

pub fn live_installed() -> bool {
    LIVE_TX.get().is_some()
}

pub fn recent_reports() -> Vec<LiveTradeReport> {
    LIVE_REPORTS.lock().map(|r| r.clone()).unwrap_or_default()
}

/// Real-equity guard: the governor sized from desk equity, so re-check the
/// risk against the REAL wallet balance before any order. Never enlarges.
pub fn risk_fits_real_equity(job: &LiveJob, real_balance: Option<f64>) -> Result<(), String> {
    let bal = real_balance.ok_or("real wallet balance unavailable")?;
    if !(bal.is_finite() && bal > 0.0) {
        return Err(format!("real wallet balance {bal} not positive"));
    }
    let cap = bal * job.risk_fraction;
    if !(job.risk_at_stop.is_finite() && job.risk_at_stop <= cap) {
        return Err(format!(
            "risk at stop {:.4} exceeds {:.2}% of real balance {:.4}",
            job.risk_at_stop,
            job.risk_fraction * 100.0,
            bal
        ));
    }
    Ok(())
}

/// Install the live task when the gate is open at start-up. The gate and the
/// kill switch are re-read from the owner's arm state every second, so expiry,
/// `qlc disarm` and `qlc kill` take effect while running.
pub fn install_if_armed(state: qe_live_check::state::StateDir) -> Result<(), String> {
    let (creds, status) = qe_coindcx::auth::load_credentials();
    let gate =
        qe_live_check::arming::gate_for_desk(&state, status.configured, crate::desk::now_ms());
    gate.check().map_err(|e| e.to_string())?;
    let creds = creds.ok_or("credentials not configured")?;
    let exec = Arc::new(LiveExecutor::new(
        PrivateClient::new(creds, gate),
        ExecConfig::default(),
    ));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<LiveJob>();
    LIVE_TX.set(tx).map_err(|_| "live task already installed")?;
    let (ex_gate, kill) = (exec.clone(), exec.kill_switch());
    tokio::spawn(async move {
        loop {
            let now = crate::desk::now_ms();
            if qe_live_check::arming::killed(&state).is_some() {
                kill.store(true, Ordering::SeqCst);
            }
            ex_gate.venue.set_gate(qe_live_check::arming::gate_for_desk(
                &state,
                status.configured,
                now,
            ));
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            // Proposals queued behind an open trade are stale: drop without
            // touching the exchange (counted, not stored as trade reports).
            if crate::desk::now_ms() - job.created_ms > MAX_JOB_AGE_MS {
                STALE_DROPPED.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let bal = exec.balance(&job.req.margin).await;
            let r = match risk_fits_real_equity(&job, bal) {
                Ok(()) => exec.execute(&job.req).await,
                Err(why) => {
                    let mut r = exec.refused(&job.req);
                    r.events.push(format!("refused: {why}"));
                    r
                }
            };
            if let Ok(mut v) = LIVE_REPORTS.lock() {
                v.push(r);
                let n = v.len();
                if n > 50 {
                    v.drain(..n - 50);
                }
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use qe_coindcx::private::types::{FuturesOrderRecord, TpslLeg};
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockState {
        armed: bool,
        /// Quantity the exchange actually fills.
        fill_qty: f64,
        entry_unknown: bool,
        reconcile_finds: bool,
        tpsl_ok: bool,
        /// Position disappears (TP/SL hit) after this many monitor reads.
        close_after_reads: Option<u32>,
        reads: u32,
        open: bool,
        calls: Vec<String>,
    }

    #[derive(Clone, Default)]
    struct Mock(Arc<Mutex<MockState>>);

    impl Mock {
        fn calls(&self) -> Vec<String> {
            self.0.lock().unwrap().calls.clone()
        }
        fn log(&self, s: &str) {
            self.0.lock().unwrap().calls.push(s.into());
        }
    }

    impl Venue for Mock {
        fn armed(&self) -> bool {
            self.0.lock().unwrap().armed
        }
        async fn update_leverage(&self, _: &str, _: &str, lev: u32) -> Result<(), PrivateError> {
            self.log(&format!("lev:{lev}"));
            Ok(())
        }
        async fn create_order(&self, coid: &str, _: NewOrder) -> Result<String, PrivateError> {
            self.log(&format!("order:{coid}"));
            let mut s = self.0.lock().unwrap();
            s.open = s.fill_qty > 0.0;
            if s.entry_unknown {
                return Err(PrivateError::OutcomeUnknown {
                    message: "timeout".into(),
                });
            }
            Ok("ex-1".into())
        }
        async fn reconcile_order(&self, _: &str) -> Result<LedgerState, PrivateError> {
            self.log("reconcile");
            let mut s = self.0.lock().unwrap();
            if s.reconcile_finds {
                Ok(LedgerState::Reconciled {
                    exchange_order_id: "ex-1".into(),
                })
            } else {
                s.open = false;
                Ok(LedgerState::ConfirmedAbsent)
            }
        }
        async fn cancel_order(&self, id: &str) -> Result<(), PrivateError> {
            self.log(&format!("cancel:{id}"));
            Ok(())
        }
        async fn position(
            &self,
            pair: &str,
            _: &str,
        ) -> Result<Option<FuturesPosition>, PrivateError> {
            let mut s = self.0.lock().unwrap();
            s.reads += 1;
            if let Some(n) = s.close_after_reads {
                if s.reads > n {
                    s.open = false;
                }
            }
            Ok(s.open.then(|| FuturesPosition {
                id: Some("pos-1".into()),
                pair: Some(pair.into()),
                active_pos: Some(s.fill_qty),
                avg_price: Some(100.0),
                ..Default::default()
            }))
        }
        async fn create_tpsl(&self, _: &str, _: f64, _: f64) -> Result<TpslResult, PrivateError> {
            self.log("tpsl");
            let ok = self.0.lock().unwrap().tpsl_ok;
            let placed = TpslLeg::Placed(FuturesOrderRecord {
                id: Some("t".into()),
                ..Default::default()
            });
            Ok(TpslResult {
                take_profit: placed.clone(),
                stop_loss: if ok {
                    placed
                } else {
                    TpslLeg::Failed("rejected".into())
                },
            })
        }
        async fn exit_position(&self, _: &str) -> Result<(), PrivateError> {
            self.log("exit");
            self.0.lock().unwrap().open = false;
            Ok(())
        }
        async fn cancel_all_for_position(&self, _: &str) -> Result<(), PrivateError> {
            self.log("cancel_all");
            Ok(())
        }
        async fn wallets(&self) -> Result<Vec<FuturesWallet>, PrivateError> {
            Ok(serde_json::from_value(serde_json::json!([
                {"currency_short_name":"USDT","balance":"12.5","locked_balance":"0"}
            ]))
            .unwrap())
        }
    }

    fn mock(f: impl FnOnce(&mut MockState)) -> Mock {
        let m = Mock::default();
        {
            let mut s = m.0.lock().unwrap();
            s.armed = true;
            s.fill_qty = 2.0;
            s.tpsl_ok = true;
            s.reconcile_finds = true;
            f(&mut s);
        }
        m
    }

    fn cfg() -> ExecConfig {
        ExecConfig {
            lifetime: Duration::from_millis(300),
            fill_timeout: Duration::from_millis(60),
            poll: Duration::from_millis(10),
            reconcile_attempts: 3,
        }
    }

    fn req() -> LiveOrderRequest {
        LiveOrderRequest {
            trade_id: "t1".into(),
            pair: "B-BTC_USDT".into(),
            margin: "USDT".into(),
            long: true,
            qty: 2.0,
            ref_price: 100.0,
            stop_loss: 99.0,
            take_profit: 101.5,
            governor_leverage: 20,
            exchange_max_leverage: Some(10),
            governor_approved: true,
        }
    }

    #[test]
    fn real_equity_guard_refuses_oversized_risk() {
        let job = |risk| LiveJob {
            req: req(),
            risk_at_stop: risk,
            risk_fraction: 0.01,
            created_ms: 0,
        };
        assert!(risk_fits_real_equity(&job(0.5), Some(100.0)).is_ok());
        assert!(risk_fits_real_equity(&job(1.5), Some(100.0)).is_err());
        assert!(risk_fits_real_equity(&job(0.5), None).is_err());
        assert!(risk_fits_real_equity(&job(0.5), Some(0.0)).is_err());
        assert!(
            !submit(job(0.1)),
            "no live task installed in tests: paper path kept"
        );
    }

    #[test]
    fn production_lifetime_is_120_seconds() {
        assert_eq!(
            ExecConfig::default().lifetime,
            Duration::from_millis(120_000)
        );
    }

    #[tokio::test]
    async fn success_closes_on_protective_order() {
        let m = mock(|s| s.close_after_reads = Some(4));
        let ex = LiveExecutor::new(m.clone(), cfg());
        let r = ex.execute(&req()).await;
        assert_eq!(r.state, LiveState::Closed, "{r:?}");
        assert_eq!(r.exit_reason, Some(ExitReason::ProtectiveOrder));
        assert_eq!(r.leverage, Some(10), "min(governor, exchange max)");
        assert!(r.flat_verified);
        assert_eq!(r.wallet_balance_after, Some(12.5));
        assert_eq!(m.calls(), ["lev:10", "order:qe-t1", "tpsl", "cancel_all"]);
    }

    #[tokio::test]
    async fn partial_fill_cancels_remainder_and_protects_filled_qty() {
        let m = mock(|s| {
            s.fill_qty = 1.0;
            s.close_after_reads = Some(20);
        });
        let r = LiveExecutor::new(m.clone(), cfg()).execute(&req()).await;
        assert_eq!(r.filled_qty, 1.0);
        assert_eq!(r.state, LiveState::Closed, "{r:?}");
        let c = m.calls();
        let cancel = c.iter().position(|x| x == "cancel:ex-1").unwrap();
        let tpsl = c.iter().position(|x| x == "tpsl").unwrap();
        assert!(cancel < tpsl, "{c:?}");
    }

    #[tokio::test]
    async fn protection_failure_emergency_exits() {
        let m = mock(|s| s.tpsl_ok = false);
        let r = LiveExecutor::new(m.clone(), cfg()).execute(&req()).await;
        assert_eq!(r.state, LiveState::EmergencyClosed);
        assert_eq!(r.exit_reason, Some(ExitReason::ProtectionFailed));
        assert!(!r.incidents.is_empty());
        assert!(m.calls().contains(&"exit".to_string()));
    }

    #[tokio::test]
    async fn entry_timeout_reconciles_before_anything_else() {
        let m = mock(|s| {
            s.entry_unknown = true;
            s.close_after_reads = Some(4);
        });
        let r = LiveExecutor::new(m.clone(), cfg()).execute(&req()).await;
        assert_eq!(r.state, LiveState::Closed, "{r:?}");
        assert_eq!(&m.calls()[..3], ["lev:10", "order:qe-t1", "reconcile"]);
        // Absent on the exchange: never resubmitted.
        let m = mock(|s| {
            s.entry_unknown = true;
            s.reconcile_finds = false;
        });
        let r = LiveExecutor::new(m.clone(), cfg()).execute(&req()).await;
        assert_eq!(r.state, LiveState::Rejected);
        assert_eq!(
            m.calls().iter().filter(|c| c.starts_with("order")).count(),
            1
        );
    }

    #[tokio::test]
    async fn time_exit_after_lifetime_from_fill() {
        let m = mock(|_| {}); // never hits TP/SL
        let r = LiveExecutor::new(m.clone(), cfg()).execute(&req()).await;
        assert_eq!(r.exit_reason, Some(ExitReason::TimeExit));
        assert_eq!(r.state, LiveState::Closed);
        assert!(r.held_ms.unwrap() >= 300, "{r:?}");
        assert!(m.calls().contains(&"exit".to_string()));
    }

    #[tokio::test]
    async fn kill_switch_flattens_open_position_and_blocks_entries() {
        let m = mock(|_| {});
        let ex = Arc::new(LiveExecutor::new(
            m.clone(),
            ExecConfig {
                lifetime: Duration::from_secs(30),
                ..cfg()
            },
        ));
        let k = ex.kill_switch();
        let ex2 = ex.clone();
        let h = tokio::spawn(async move { ex2.execute(&req()).await });
        tokio::time::sleep(Duration::from_millis(150)).await;
        k.store(true, Ordering::SeqCst);
        let r = h.await.unwrap();
        assert_eq!(r.exit_reason, Some(ExitReason::KillSwitch));
        assert!(r.flat_verified);
        let r2 = ex
            .execute(&LiveOrderRequest {
                trade_id: "t2".into(),
                ..req()
            })
            .await;
        assert_eq!(r2.state, LiveState::Refused);
        assert_eq!(
            m.calls().iter().filter(|c| c.starts_with("order")).count(),
            1
        );
    }

    #[tokio::test]
    async fn refuses_when_disarmed_unapproved_or_unprotected() {
        let m = mock(|s| s.armed = false);
        let r = LiveExecutor::new(m.clone(), cfg()).execute(&req()).await;
        assert_eq!(r.state, LiveState::Refused);
        let m2 = mock(|_| {});
        let ex = LiveExecutor::new(m2.clone(), cfg());
        let r = ex
            .execute(&LiveOrderRequest {
                governor_approved: false,
                ..req()
            })
            .await;
        assert_eq!(r.state, LiveState::Refused);
        let r = ex
            .execute(&LiveOrderRequest {
                stop_loss: 101.0,
                ..req()
            })
            .await;
        assert_eq!(r.state, LiveState::Refused);
        assert!(m.calls().is_empty() && m2.calls().is_empty());
    }

    #[tokio::test]
    async fn real_private_client_is_not_armed_by_default() {
        let c = PrivateClient::new(
            qe_coindcx::auth::Credentials::new("k".into(), "s".into()).unwrap(),
            qe_coindcx::live::LiveGate::default(),
        )
        .with_base_url("http://127.0.0.1:9");
        let r = LiveExecutor::new(c, cfg()).execute(&req()).await;
        assert_eq!(r.state, LiveState::Refused);
    }
}
