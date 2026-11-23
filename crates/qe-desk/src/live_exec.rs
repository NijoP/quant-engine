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
//!
//! Post-fill verification (every live trade, owner directive 24 Nov 2026): the
//! exchange position read that confirmed the fill must report a positive
//! quantity, a fill price, the leverage that was set and `margin_type ==
//! isolated`; after `create_tpsl` both protective legs must be LISTED as open
//! (`untriggered`) on the exchange for the pair. Any failure: no further risk,
//! emergency exit, verify flat, incident, and a PERMANENT stop
//! ([`protection_stop`]: the desk stops the campaign and sets the kill switch).
use qe_coindcx::private::types::{FuturesOrderRecord, OrderType, TimeInForce};
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
    /// Live (open / partially filled / untriggered) orders in this margin currency.
    fn open_orders(
        &self,
        margin: &str,
    ) -> impl Future<Output = Result<Vec<FuturesOrderRecord>, PrivateError>> + Send;
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
    async fn open_orders(&self, margin: &str) -> Result<Vec<FuturesOrderRecord>, PrivateError> {
        let mut all = Vec::new();
        for side in [Side::Buy, Side::Sell] {
            all.extend(
                self.list_orders(
                    side,
                    qe_live_check::api::LIVE_ORDER_STATUSES,
                    1,
                    100,
                    &[margin],
                )
                .await?,
            );
        }
        Ok(all)
    }
}

/// Exchange-reported checks on the position that confirmed the fill. Returns
/// the failures (empty = verified).
pub fn verify_position(p: &FuturesPosition, req_qty: f64, lev: u32) -> Vec<String> {
    let mut bad = vec![];
    let q = signed_qty(p).abs();
    if !(q > 0.0 && q <= req_qty + 1e-12) {
        bad.push(format!("quantity {q} outside (0, {req_qty}]"));
    }
    if !p.avg_price.is_some_and(|x| x.is_finite() && x > 0.0) {
        bad.push(format!("fill price not reported ({:?})", p.avg_price));
    }
    match p.leverage {
        Some(l) if (l - lev as f64).abs() < 1e-9 => {}
        other => bad.push(format!("leverage {other:?} != set {lev}x")),
    }
    if !p
        .margin_type
        .as_deref()
        .is_some_and(|m| m.trim().eq_ignore_ascii_case("isolated"))
    {
        bad.push(format!(
            "margin_type {:?}, expected \"isolated\"",
            p.margin_type
        ));
    }
    bad
}

/// Untriggered (resting protective) orders listed for `pair`.
pub fn listed_protective(os: &[FuturesOrderRecord], pair: &str) -> usize {
    os.iter()
        .filter(|o| o.pair.as_deref() == Some(pair))
        .filter(|o| {
            o.status
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("untriggered"))
        })
        .count()
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
    /// Gamble maker entry: a passive limit (GTC) at this price instead of a
    /// market order. No `post_only` flag is sent: CoinDCX documents it as "not
    /// supported at the moment" for futures, so the order can still fill as
    /// taker if the book moved through the price before it arrived.
    #[serde(default)]
    pub entry_limit: Option<f64>,
    /// Override the fill timeout (ms) for this entry; the unfilled remainder
    /// is then cancelled and never chased with a market order.
    #[serde(default)]
    pub fill_timeout_ms: Option<u64>,
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
    /// Exchange-reported margin type of the position (post-fill read).
    pub margin_type: Option<String>,
    /// Position verified (qty, fill price, leverage, isolated) AND both TP and
    /// SL listed open on the exchange.
    pub protection_verified: bool,
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
            margin_type: None,
            protection_verified: false,
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
        let mut order = NewOrder::market(side, &req.pair, req.qty, lev, &req.margin);
        if let Some(px) = req.entry_limit.filter(|p| p.is_finite() && *p > 0.0) {
            order.order_type = OrderType::LimitOrder;
            order.price = Some(format!("{px}"));
            order.time_in_force = Some(TimeInForce::GoodTillCancel);
        }
        let fill_timeout = req
            .fill_timeout_ms
            .map_or(self.cfg.fill_timeout, Duration::from_millis);
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
        let fill_deadline = submitted + fill_timeout;
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
            // A fill can land between the last read and the cancel: re-read once.
            match self.venue.position(&req.pair, &req.margin).await {
                Ok(Some(p)) if p.is_open() => pos = Some(p),
                Ok(_) => {}
                Err(e) => r
                    .events
                    .push(format!("post-cancel position read failed: {e:?}")),
            }
        }
        let filled = pos.as_ref().map_or(0.0, |p| signed_qty(p).abs());
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
        r.margin_type = p.margin_type.clone();

        // Verify the exchange position before adding anything to it.
        let bad = verify_position(&p, req.qty, lev);
        let verified = if bad.is_empty() {
            r.events.push(format!(
                "position verified: qty {filled} avg {:?} leverage {lev}x margin_type {:?}",
                p.avg_price, p.margin_type
            ));
            true
        } else {
            r.incidents
                .push(format!("position verification failed: {}", bad.join("; ")));
            false
        };
        // Exchange-side protection: both legs accepted AND listed, or emergency exit.
        let accepted = verified
            && match self
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
        // An accepted API response is not proof: the legs must be listed open.
        let protected = accepted
            && match self.venue.open_orders(&req.margin).await {
                Ok(os) => {
                    let n = listed_protective(&os, &req.pair);
                    if n >= 2 {
                        r.events
                            .push(format!("TP and SL listed open on the exchange ({n})"));
                        true
                    } else {
                        r.incidents.push(format!(
                            "protection not listed: {n} untriggered order(s) for {}",
                            req.pair
                        ));
                        false
                    }
                }
                Err(e) => {
                    r.incidents
                        .push(format!("protection listing read failed: {e:?}"));
                    false
                }
            };
        r.protection_verified = protected;
        if !protected {
            set_protection_stop(format!(
                "trade {}: {}",
                req.trade_id,
                r.incidents.last().cloned().unwrap_or_default()
            ));
        }
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
        let held = filled_at.elapsed();
        if held > self.cfg.lifetime + Duration::from_millis(CLOSE_GRACE_MS) {
            set_protection_stop(format!(
                "trade {}: position open {} ms, beyond the {} ms limit",
                req.trade_id,
                held.as_millis(),
                self.cfg.lifetime.as_millis()
            ));
        }
        if !r.flat_verified {
            set_protection_stop(format!(
                "trade {}: account not verified flat after exit",
                req.trade_id
            ));
        }
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

/// First-genuine-trade protection mode (`qe-live-check arm
/// --first-trade-verifies-protection`): the first live trade's verified
/// protection and its closure are recorded as the readiness results that the
/// owner could not obtain without a separate test order. Audited.
pub fn record_first_trade(state: &qe_live_check::state::StateDir, r: &LiveTradeReport, now: i64) {
    use qe_coindcx::live::{CheckItem, CheckStatus};
    if r.state == LiveState::Refused {
        return;
    }
    let _ = state.audit(
        now,
        "live_trade",
        serde_json::to_value(r).unwrap_or_default(),
    );
    if r.filled_qty <= 0.0 || !qe_live_check::arming::first_trade_mode(state) {
        return;
    }
    // Only the FIRST filled trade is the source; later trades never overwrite it.
    let from_first = state.checks().iter().any(|c| {
        c.item == CheckItem::ProtectiveOrdersVerified
            && matches!(&c.status, CheckStatus::Pass(d) | CheckStatus::Fail(d) if d.starts_with(FIRST_TRADE_SOURCE))
    });
    if from_first {
        return;
    }
    let prot = if r.protection_verified {
        CheckStatus::Pass(format!(
            "{FIRST_TRADE_SOURCE} {} - position isolated ({:?}), qty {} and leverage {:?} verified, TP+SL listed open on the exchange",
            r.trade_id, r.margin_type, r.filled_qty, r.leverage
        ))
    } else {
        CheckStatus::Fail(format!(
            "{FIRST_TRADE_SOURCE} {} - {}",
            r.trade_id,
            r.incidents.join("; ")
        ))
    };
    let _ =
        qe_live_check::state::record_check(state, CheckItem::ProtectiveOrdersVerified, prot, now);
    let held = r.held_ms.unwrap_or(u64::MAX);
    let closed_in_time = r.flat_verified && held <= LIFETIME_MS + CLOSE_GRACE_MS;
    let close = if closed_in_time {
        CheckStatus::Pass(format!(
            "{FIRST_TRADE_SOURCE} {} - closed ({:?}) {held} ms after the fill, flat verified",
            r.trade_id, r.exit_reason
        ))
    } else {
        CheckStatus::Fail(format!(
            "{FIRST_TRADE_SOURCE} {} - held {held} ms, flat verified {}",
            r.trade_id, r.flat_verified
        ))
    };
    let _ = qe_live_check::state::record_check(state, CheckItem::Closure120sTested, close, now);
}

/// Detail prefix of readiness results recorded from the first live trade.
pub const FIRST_TRADE_SOURCE: &str = "source: first live trade";
/// Exit + flat verification allowance after the 120 s lifetime.
pub const CLOSE_GRACE_MS: u64 = 15_000;

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
    /// Gamble mode: owner balance floor (INR). The guard then requires the
    /// isolated margin to fit above the floor in the REAL wallet balance.
    pub gamble_floor: Option<f64>,
    /// Isolated margin of the order (margin currency).
    pub margin: f64,
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

/// Permanent stop raised by the executor (protection / verification /
/// reconciliation failure). Never cleared automatically.
static PROTECTION_STOP: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
/// True while a live trade is in flight (submit .. verified flat).
static LIVE_BUSY: AtomicBool = AtomicBool::new(false);
/// Wall-clock ms of the last trade start / end (for reconciliation reads).
static LAST_START_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
static LAST_END_MS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
/// Set at the campaign deadline: the live task refuses every new job.
static ENTRIES_CLOSED: AtomicBool = AtomicBool::new(false);

fn set_protection_stop(why: String) {
    if let Ok(mut g) = PROTECTION_STOP.lock() {
        if g.is_none() {
            *g = Some(why);
        }
    }
}

/// The executor's permanent-stop reason, if any.
pub fn protection_stop() -> Option<String> {
    PROTECTION_STOP.lock().ok().and_then(|g| g.clone())
}

/// Close the live task to new entries (campaign deadline / exit-only).
pub fn close_entries() {
    ENTRIES_CLOSED.store(true, Ordering::SeqCst);
}

pub fn entries_closed() -> bool {
    ENTRIES_CLOSED.load(Ordering::SeqCst)
}

/// No live trade in flight and none started or ended at/after `since_ms`:
/// an exchange read that STARTED at `since_ms` then reflects the executor's
/// settled state (used for reconciliation and the deadline flat check).
pub fn idle_since(since_ms: i64) -> bool {
    !LIVE_BUSY.load(Ordering::SeqCst)
        && LAST_START_MS.load(Ordering::SeqCst) < since_ms
        && LAST_END_MS.load(Ordering::SeqCst) < since_ms
}

pub fn live_busy() -> bool {
    LIVE_BUSY.load(Ordering::SeqCst)
}

/// Set when a live gamble bet would breach, or a live trade ended at/below,
/// the owner floor; the desk then stops gamble mode permanently.
static GAMBLE_FLOOR_HIT: AtomicBool = AtomicBool::new(false);

pub fn gamble_floor_hit() -> bool {
    GAMBLE_FLOOR_HIT.load(Ordering::SeqCst)
}

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
    if let Some(floor) = job.gamble_floor {
        // Gamble: the worst case (isolated margin lost) must stay above the floor.
        if !(job.margin.is_finite() && bal - job.margin >= floor) {
            GAMBLE_FLOOR_HIT.store(true, Ordering::SeqCst);
            return Err(format!(
                "gamble floor: real balance {bal:.4} minus margin {:.4} is below floor {floor:.4}",
                job.margin
            ));
        }
        return Ok(());
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
    let gate_state = state.clone();
    tokio::spawn(async move {
        let state = gate_state;
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
            if entries_closed() || protection_stop().is_some() {
                STALE_DROPPED.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            LIVE_BUSY.store(true, Ordering::SeqCst);
            LAST_START_MS.store(crate::desk::now_ms(), Ordering::SeqCst);
            let bal = exec.balance(&job.req.margin).await;
            let r = match risk_fits_real_equity(&job, bal) {
                Ok(()) => exec.execute(&job.req).await,
                Err(why) => {
                    let mut r = exec.refused(&job.req);
                    r.events.push(format!("refused: {why}"));
                    r
                }
            };
            let end = crate::desk::now_ms();
            record_first_trade(&state, &r, end);
            if let Some(why) = protection_stop() {
                // permanent: kill switch (owner-only clear) + incident record
                let _ = qe_live_check::arming::kill(
                    &state,
                    &format!("desk protection stop: {why}"),
                    end,
                );
                let _ = state.write_json(&format!("incident-{}.json", r.trade_id), &r);
            }
            LAST_END_MS.store(end, Ordering::SeqCst);
            LIVE_BUSY.store(false, Ordering::SeqCst);
            if let (Some(f), Some(b)) = (job.gamble_floor, r.wallet_balance_after) {
                if b <= f {
                    GAMBLE_FLOOR_HIT.store(true, Ordering::SeqCst);
                }
            }
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
        last_order: Option<NewOrder>,
        /// Leverage last set (reported back on the position).
        lev: u32,
        /// Margin type the exchange reports (default "isolated").
        margin_type: Option<String>,
        /// Untriggered protective orders listed after create_tpsl.
        listed: usize,
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
            self.0.lock().unwrap().lev = lev;
            Ok(())
        }
        async fn create_order(&self, coid: &str, o: NewOrder) -> Result<String, PrivateError> {
            self.log(&format!("order:{coid}"));
            let mut s = self.0.lock().unwrap();
            s.last_order = Some(o);
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
                leverage: Some(s.lev as f64),
                margin_type: s.margin_type.clone(),
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
        async fn open_orders(&self, _: &str) -> Result<Vec<FuturesOrderRecord>, PrivateError> {
            self.log("listed");
            let n = self.0.lock().unwrap().listed;
            Ok((0..n)
                .map(|i| FuturesOrderRecord {
                    id: Some(format!("p{i}")),
                    pair: Some("B-BTC_USDT".into()),
                    status: Some("untriggered".into()),
                    ..Default::default()
                })
                .collect())
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
            s.margin_type = Some("isolated".into());
            s.listed = 2;
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
            entry_limit: None,
            fill_timeout_ms: None,
        }
    }

    #[tokio::test]
    async fn maker_entry_is_a_passive_gtc_limit_and_unfilled_is_cancelled() {
        let m = mock(|s| s.fill_qty = 0.0);
        let mut q = req();
        q.entry_limit = Some(99.95);
        q.fill_timeout_ms = Some(40);
        let r = LiveExecutor::new(m.clone(), cfg()).execute(&q).await;
        assert_eq!(r.state, LiveState::NoFill, "{r:?}");
        let o = m.0.lock().unwrap().last_order.clone().unwrap();
        assert_eq!(o.order_type, OrderType::LimitOrder);
        assert_eq!(o.price.as_deref(), Some("99.95"));
        assert_eq!(o.time_in_force, Some(TimeInForce::GoodTillCancel));
        assert!(o.validate().is_ok());
        let body = serde_json::to_value(&o).unwrap();
        assert_eq!(body["order_type"], "limit_order");
        assert!(
            body.get("post_only").is_none(),
            "venue: post_only unsupported"
        );
        let c = m.calls();
        assert!(c.contains(&"cancel:ex-1".to_string()), "{c:?}");
        assert!(
            !c.iter().any(|x| x == "tpsl" || x == "exit"),
            "no chase: {c:?}"
        );
        assert_eq!(c.iter().filter(|x| x.starts_with("order:")).count(), 1);
    }

    #[tokio::test]
    async fn maker_entry_filled_is_protected_like_a_market_entry() {
        let m = mock(|s| s.close_after_reads = Some(4));
        let mut q = req();
        q.entry_limit = Some(99.95);
        let r = LiveExecutor::new(m.clone(), cfg()).execute(&q).await;
        assert_eq!(r.state, LiveState::Closed, "{r:?}");
        assert_eq!(
            m.calls(),
            ["lev:10", "order:qe-t1", "tpsl", "listed", "cancel_all"]
        );
    }

    #[test]
    fn real_equity_guard_refuses_oversized_risk() {
        let job = |risk| LiveJob {
            req: req(),
            risk_at_stop: risk,
            risk_fraction: 0.01,
            created_ms: 0,
            gamble_floor: None,
            margin: 5.0,
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
    fn gamble_live_guard_keeps_margin_above_the_real_floor() {
        let job = |margin| LiveJob {
            req: req(),
            risk_at_stop: 5.0,
            risk_fraction: 0.01,
            created_ms: 0,
            gamble_floor: Some(50.0),
            margin,
        };
        // risk 5 > 1% of 100 would fail the standard guard; gamble uses the floor
        assert!(risk_fits_real_equity(&job(10.0), Some(100.0)).is_ok());
        assert!(risk_fits_real_equity(&job(10.0), Some(55.0)).is_err());
        assert!(gamble_floor_hit());
        assert!(risk_fits_real_equity(&job(10.0), None).is_err());
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
        assert_eq!(
            m.calls(),
            ["lev:10", "order:qe-t1", "tpsl", "listed", "cancel_all"]
        );
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

    #[tokio::test]
    async fn success_records_verified_protection() {
        let m = mock(|s| s.close_after_reads = Some(4));
        let r = LiveExecutor::new(m.clone(), cfg()).execute(&req()).await;
        assert!(r.protection_verified, "{r:?}");
        assert_eq!(r.margin_type.as_deref(), Some("isolated"));
        assert!(r.events.iter().any(|e| e.contains("listed open")), "{r:?}");
    }

    #[tokio::test]
    async fn non_isolated_position_is_emergency_closed_without_tpsl() {
        let m = mock(|s| s.margin_type = Some("crossed".into()));
        let r = LiveExecutor::new(m.clone(), cfg()).execute(&req()).await;
        assert_eq!(r.state, LiveState::EmergencyClosed, "{r:?}");
        assert!(!r.protection_verified);
        let c = m.calls();
        assert!(!c.contains(&"tpsl".to_string()), "{c:?}");
        assert!(c.contains(&"exit".to_string()), "{c:?}");
        assert!(r.flat_verified);
        assert!(protection_stop().is_some(), "permanent stop raised");
        // missing margin type is not "isolated" either
        let m = mock(|s| s.margin_type = None);
        let r = LiveExecutor::new(m, cfg()).execute(&req()).await;
        assert_eq!(r.state, LiveState::EmergencyClosed, "{r:?}");
    }

    #[tokio::test]
    async fn accepted_but_unlisted_protection_is_emergency_closed() {
        let m = mock(|s| s.listed = 1);
        let r = LiveExecutor::new(m.clone(), cfg()).execute(&req()).await;
        assert_eq!(r.state, LiveState::EmergencyClosed, "{r:?}");
        assert_eq!(r.exit_reason, Some(ExitReason::ProtectionFailed));
        assert!(
            r.incidents.iter().any(|i| i.contains("not listed")),
            "{r:?}"
        );
        assert!(m.calls().contains(&"exit".to_string()));
    }

    #[test]
    fn position_verification_checks_qty_price_leverage_and_margin() {
        let p = FuturesPosition {
            active_pos: Some(-2.0),
            avg_price: Some(100.0),
            leverage: Some(5.0),
            margin_type: Some("ISOLATED".into()),
            ..Default::default()
        };
        assert!(verify_position(&p, 2.0, 5).is_empty());
        assert_eq!(verify_position(&p, 2.0, 4).len(), 1, "leverage mismatch");
        assert_eq!(verify_position(&p, 1.0, 5).len(), 1, "over-filled");
        let none = FuturesPosition {
            active_pos: Some(1.0),
            ..Default::default()
        };
        assert_eq!(verify_position(&none, 2.0, 5).len(), 3);
    }

    fn report(verified: bool, held: u64, flat: bool) -> LiveTradeReport {
        LiveTradeReport {
            trade_id: "t1".into(),
            client_order_id: "qe-t1".into(),
            state: LiveState::Closed,
            exit_reason: Some(ExitReason::TimeExit),
            leverage: Some(5),
            requested_qty: 1.0,
            filled_qty: 1.0,
            avg_entry: Some(100.0),
            fill_confirmed_ms_after_submit: Some(50),
            held_ms: Some(held),
            flat_verified: flat,
            wallet_balance_after: Some(201.0),
            margin_type: Some("isolated".into()),
            protection_verified: verified,
            incidents: vec![],
            events: vec![],
        }
    }

    fn first_trade_dir(tag: &str) -> qe_live_check::state::StateDir {
        use qe_coindcx::live::{CheckItem, CheckStatus};
        let p = std::env::temp_dir().join(format!(
            "qe-desk-ft-{tag}-{}-{}",
            std::process::id(),
            crate::desk::now_ms()
        ));
        let d = qe_live_check::state::StateDir::new(p);
        let now = crate::desk::now_ms();
        for i in CheckItem::ALL {
            if !qe_live_check::arming::FIRST_TRADE_DEFERRED.contains(&i) {
                qe_live_check::state::record_check(&d, i, CheckStatus::Pass("ok".into()), now)
                    .unwrap();
            }
        }
        struct Yes;
        impl qe_live_check::confirm::Confirm for Yes {
            fn confirm(&mut self, _: &str, _: &str) -> bool {
                true
            }
        }
        qe_live_check::arming::arm_with(&d, &mut Yes, 2, now, true).unwrap();
        d
    }

    #[test]
    fn first_trade_records_protection_and_closure_once() {
        use qe_coindcx::live::CheckItem;
        let d = first_trade_dir("ok");
        let now = crate::desk::now_ms();
        record_first_trade(&d, &report(true, 120_400, true), now);
        let get = |i: CheckItem| d.checks().into_iter().find(|c| c.item == i).unwrap();
        let p = get(CheckItem::ProtectiveOrdersVerified);
        assert!(p.status.is_pass(), "{p:?}");
        assert!(format!("{:?}", p.status).contains(FIRST_TRADE_SOURCE));
        assert!(get(CheckItem::Closure120sTested).status.is_pass());
        // a later trade never overwrites the first trade's record
        record_first_trade(&d, &report(false, 500_000, false), now + 1);
        assert!(get(CheckItem::ProtectiveOrdersVerified).status.is_pass());
        let ev: Vec<String> = d
            .audit_lines()
            .iter()
            .filter_map(|l| l["event"].as_str().map(String::from))
            .collect();
        assert!(
            ev.iter().filter(|e| *e == "live_trade").count() == 2,
            "{ev:?}"
        );
    }

    #[test]
    fn first_trade_failure_is_recorded_as_fail() {
        use qe_coindcx::live::CheckItem;
        let d = first_trade_dir("fail");
        let now = crate::desk::now_ms();
        record_first_trade(&d, &report(false, 200_000, true), now);
        let get = |i: CheckItem| d.checks().into_iter().find(|c| c.item == i).unwrap();
        assert!(!get(CheckItem::ProtectiveOrdersVerified).status.is_pass());
        assert!(!get(CheckItem::Closure120sTested).status.is_pass());
        // the gate closes on the recorded failure
        let g = qe_live_check::arming::gate_for_desk(&d, true, now + 1);
        assert!(!g.readiness.all_pass());
    }
}
