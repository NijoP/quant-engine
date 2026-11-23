//! Owner-workflow tests against an in-memory exchange (no network).
use qe_coindcx::live::CheckStatus;
use qe_coindcx::private::types::{Ack, NewOrder, SpotBalance};
use qe_coindcx::private::{
    FuturesOrderRecord, FuturesPosition, FuturesWallet, PrivateError, TpslResult,
};
use qe_coindcx::types::FuturesInstrument;
use qe_live_check::api::AccountApi;
use qe_live_check::checkpoint::{self, CheckpointKind};
use qe_live_check::confirm::Scripted;
use qe_live_check::readiness::{self, DeskPosition, DeskState};
use qe_live_check::{validate, Poll};
use rust_decimal::Decimal;
use std::cell::Cell;
use std::str::FromStr;
use std::sync::Mutex;

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

#[derive(Default)]
struct Ex {
    positions: Vec<FuturesPosition>,
    orders: Vec<FuturesOrderRecord>,
    calls: Vec<String>,
}

#[derive(Default)]
struct Mock {
    ex: Mutex<Ex>,
    fill_price: f64,
    tpsl_tp_fails: bool,
    exit_does_nothing: bool,
    futures_inr: f64,
    spot_inr: f64,
}

impl Mock {
    fn new() -> Self {
        Self {
            fill_price: 100.0,
            futures_inr: 2950.5,
            spot_inr: 100.0,
            ..Default::default()
        }
    }
    fn calls(&self) -> Vec<String> {
        self.ex.lock().unwrap().calls.clone()
    }
    fn log(&self, c: &str) {
        self.ex.lock().unwrap().calls.push(c.into());
    }
    fn open_position(&self, pair: &str, qty: f64) {
        self.ex.lock().unwrap().positions.push(FuturesPosition {
            id: Some(format!("pos-{pair}")),
            pair: Some(pair.into()),
            active_pos: Some(qty),
            avg_price: Some(self.fill_price),
            margin_currency_short_name: Some("INR".into()),
            ..Default::default()
        });
    }
    fn add_order(&self, pair: &str, status: &str) {
        let mut ex = self.ex.lock().unwrap();
        let n = ex.orders.len();
        ex.orders.push(FuturesOrderRecord {
            id: Some(format!("o-{n}")),
            pair: Some(pair.into()),
            status: Some(status.into()),
            order_type: Some("stop_market".into()),
            ..Default::default()
        });
    }
}

fn ok() -> Ack {
    serde_json::from_value(serde_json::json!({"message":"success","status":200,"code":200}))
        .unwrap()
}

impl AccountApi for Mock {
    async fn wallets(&self) -> Result<Vec<FuturesWallet>, PrivateError> {
        Ok(vec![FuturesWallet {
            currency_short_name: Some("INR".into()),
            balance: Some(self.futures_inr),
            locked_balance: Some(0.0),
            ..Default::default()
        }])
    }
    async fn spot_balances(&self) -> Result<Vec<SpotBalance>, PrivateError> {
        Ok(vec![SpotBalance {
            currency: Some("INR".into()),
            balance: Some(self.spot_inr),
            locked_balance: Some(0.0),
        }])
    }
    async fn positions(&self, _m: &[&str]) -> Result<Vec<FuturesPosition>, PrivateError> {
        Ok(self.ex.lock().unwrap().positions.clone())
    }
    async fn live_orders(&self, _m: &[&str]) -> Result<Vec<FuturesOrderRecord>, PrivateError> {
        Ok(self.ex.lock().unwrap().orders.clone())
    }
    async fn update_leverage(&self, pair: &str, _m: &str, lev: u32) -> Result<Ack, PrivateError> {
        self.log(&format!("update_leverage {pair} {lev}"));
        Ok(ok())
    }
    async fn create_order(
        &self,
        _coid: &str,
        o: NewOrder,
    ) -> Result<FuturesOrderRecord, PrivateError> {
        self.log(&format!("create_order {} {}", o.pair, o.total_quantity));
        self.open_position(&o.pair, o.total_quantity);
        Ok(FuturesOrderRecord {
            id: Some("entry-1".into()),
            status: Some("open".into()),
            ..Default::default()
        })
    }
    async fn create_tpsl(&self, pos: &str, tp: f64, sl: f64) -> Result<TpslResult, PrivateError> {
        self.log(&format!("create_tpsl {pos} {tp} {sl}"));
        let pair = pos.trim_start_matches("pos-").to_string();
        let leg = |t: &str| serde_json::json!({"id": format!("{t}-1"), "pair": pair, "status": "untriggered", "order_type": t});
        let v = if self.tpsl_tp_fails {
            self.add_order(&pair, "untriggered");
            serde_json::json!({"stop_loss": leg("stop_market"), "take_profit": {"success": false, "error": "TP already exists"}})
        } else {
            self.add_order(&pair, "untriggered");
            self.add_order(&pair, "untriggered");
            serde_json::json!({"stop_loss": leg("stop_market"), "take_profit": leg("take_profit_market")})
        };
        Ok(TpslResult::from_value(&v))
    }
    async fn exit_position(&self, pos: &str) -> Result<Ack, PrivateError> {
        self.log(&format!("exit_position {pos}"));
        if !self.exit_does_nothing {
            let mut ex = self.ex.lock().unwrap();
            for p in ex
                .positions
                .iter_mut()
                .filter(|p| p.id.as_deref() == Some(pos))
            {
                p.active_pos = Some(0.0);
            }
        }
        Ok(ok())
    }
    async fn cancel_all_for_position(&self, pos: &str) -> Result<Ack, PrivateError> {
        self.log(&format!("cancel_all_for_position {pos}"));
        let pair = pos.trim_start_matches("pos-").to_string();
        self.ex
            .lock()
            .unwrap()
            .orders
            .retain(|o| o.pair.as_deref() != Some(pair.as_str()));
        Ok(ok())
    }
    async fn cancel_all_open_orders(&self, m: &[&str]) -> Result<Ack, PrivateError> {
        self.log(&format!("cancel_all_open_orders {m:?}"));
        self.ex.lock().unwrap().orders.clear();
        Ok(ok())
    }
}

fn instrument() -> FuturesInstrument {
    serde_json::from_value(serde_json::json!({
        "pair": "B-ETH_INR", "status": "active", "margin_currency_short_name": "INR",
        "settle_currency_short_name": "INR", "quote_currency_short_name": "INR",
        "underlying_currency_short_name": "ETH", "max_leverage_long": 20.0, "max_leverage_short": 20.0,
        "price_increment": 0.01, "quantity_increment": 0.001, "min_quantity": 0.001, "max_quantity": 100.0,
        "min_notional": 250.0, "max_notional": 1e9, "maker_fee": 0.02, "taker_fee": 0.05,
        "exit_only": false, "order_types": ["market_order", "limit_order", "take_profit_market", "stop_market"],
        "time_in_force_options": [], "dynamic_position_leverage_details": {}, "dynamic_safety_margin_details": {}
    }))
    .expect("instrument fixture matches FuturesInstrument")
}

fn clock() -> impl Fn() -> i64 {
    let t = Cell::new(1_795_440_000_000i64);
    move || {
        t.set(t.get() + 500);
        t.get()
    }
}

#[test]
fn plan_is_minimum_size_on_grid_and_respects_max_leverage() {
    let p = validate::plan(&instrument(), 100.0, 1).unwrap();
    // 250 * 1.02 / 100 = 2.55 -> already on the 0.001 grid.
    assert_eq!(p.quantity, d("2.55"));
    assert!(p.quantity * d("100") >= d("250"));
    let p = validate::plan(&instrument(), 333.0, 1).unwrap();
    assert_eq!(p.quantity, d("0.766")); // ceil(0.76576..)
    assert!(validate::plan(&instrument(), 100.0, 21).is_err());
    assert!(validate::plan(&instrument(), 100.0, 0).is_err());
    let mut exit_only = instrument();
    exit_only.exit_only = true;
    assert!(validate::plan(&exit_only, 100.0, 1).is_err());
    let (tp, sl) = validate::bracket(qe_coindcx::private::Side::Buy, d("100.005"), d("0.01"));
    assert!(tp > d("100.005") && sl < d("100.005"));
    assert_eq!((tp, sl), (d("101.01"), d("99.00")));
}

#[tokio::test]
async fn validation_happy_path_verifies_protection_and_closure() {
    let m = Mock::new();
    let mut c = Scripted::new(&[true, true]);
    let plan = validate::plan(&instrument(), 100.0, 1).unwrap();
    let out = validate::run(&m, &mut c, plan, Poll::IMMEDIATE, &clock(), false).await;
    assert!(out.protective_verified, "{:?}", out.steps);
    assert!(out.flat_at_end);
    assert!(out.closure_ms.unwrap() <= validate::MAX_CLOSURE_MS);
    assert!(out.protective_status().is_pass() && out.closure_status().is_pass());
    assert_eq!(
        c.asked,
        vec!["LEVERAGE B-ETH_INR 1x", "BUY 2.55 B-ETH_INR REAL"]
    );
    let calls = m.calls();
    let order: Vec<&str> = calls.iter().map(|c| c.split(' ').next().unwrap()).collect();
    assert_eq!(
        order,
        vec![
            "update_leverage",
            "create_order",
            "create_tpsl",
            "cancel_all_for_position",
            "exit_position",
            "cancel_all_open_orders"
        ]
    );
}

#[tokio::test]
async fn owner_declining_places_nothing() {
    let m = Mock::new();
    let plan = validate::plan(&instrument(), 100.0, 1).unwrap();
    let out = validate::run(
        &m,
        &mut Scripted::new(&[false]),
        plan.clone(),
        Poll::IMMEDIATE,
        &clock(),
        false,
    )
    .await;
    assert!(out.aborted_by_owner && !out.entry_sent);
    assert!(m.calls().is_empty());
    assert!(matches!(out.protective_status(), CheckStatus::NotRun(_)));
    // Confirms leverage, declines entry: leverage only.
    let m = Mock::new();
    let out = validate::run(
        &m,
        &mut Scripted::new(&[true, false]),
        plan,
        Poll::IMMEDIATE,
        &clock(),
        false,
    )
    .await;
    assert!(!out.entry_sent);
    assert_eq!(m.calls(), vec!["update_leverage B-ETH_INR 1"]);
}

#[tokio::test]
async fn validation_refuses_when_account_not_flat() {
    let m = Mock::new();
    m.add_order("B-BTC_INR", "open");
    let plan = validate::plan(&instrument(), 100.0, 1).unwrap();
    let out = validate::run(
        &m,
        &mut Scripted::new(&[true, true]),
        plan,
        Poll::IMMEDIATE,
        &clock(),
        false,
    )
    .await;
    assert!(!out.entry_sent);
    assert!(m.calls().is_empty(), "no mutation before a flat preflight");
}

#[tokio::test]
async fn partial_protection_fails_and_still_flattens() {
    let m = Mock {
        tpsl_tp_fails: true,
        ..Mock::new()
    };
    let plan = validate::plan(&instrument(), 100.0, 1).unwrap();
    let out = validate::run(
        &m,
        &mut Scripted::new(&[true, true]),
        plan,
        Poll::IMMEDIATE,
        &clock(),
        false,
    )
    .await;
    assert!(!out.protective_verified);
    assert!(
        matches!(out.protective_status(), CheckStatus::Fail(ref s) if s.contains("create_tpsl"))
    );
    assert!(out.flat_at_end, "emergency path must still flatten");
    assert!(out.emergency.is_some());
    assert!(m.calls().iter().any(|c| c.starts_with("exit_position")));
}

#[tokio::test]
async fn exit_that_does_not_close_is_reported_not_flat() {
    let m = Mock {
        exit_does_nothing: true,
        ..Mock::new()
    };
    let plan = validate::plan(&instrument(), 100.0, 1).unwrap();
    let out = validate::run(
        &m,
        &mut Scripted::new(&[true, true]),
        plan,
        Poll::IMMEDIATE,
        &clock(),
        false,
    )
    .await;
    assert!(!out.flat_at_end);
    assert!(matches!(out.closure_status(), CheckStatus::Fail(_)));
}

#[tokio::test]
async fn checkpoint_flattens_reads_actual_balance_and_prepares_withdrawal() {
    let m = Mock::new();
    m.open_position("B-SOL_INR", -3.0);
    m.add_order("B-SOL_INR", "untriggered");
    m.add_order("B-XRP_INR", "open");
    let flat = checkpoint::flatten_all(&m, &["INR"], Poll::IMMEDIATE).await;
    assert!(flat.flat && flat.errors.is_empty(), "{flat:?}");
    let bal = checkpoint::read_balances(&m, 1).await.unwrap();
    assert_eq!(bal.total("INR"), d("3050.5"));
    let rep = checkpoint::build_report(
        CheckpointKind::Stage1,
        1,
        flat,
        Ok(bal.clone()),
        "INR",
        Some(d("3050.5")),
        None,
    );
    let w = rep.withdrawal.clone().unwrap();
    assert!(w.possible && w.planned == d("2000"));
    assert!(rep.verdict.contains("reconciles"));
    // Missed target: never assumed.
    let poor = checkpoint::Balances {
        futures: [("INR".to_string(), d("812.4"))].into(),
        ..Default::default()
    };
    let w = checkpoint::prepare_withdrawal(&poor, "INR", d("2000"));
    assert!(!w.possible && w.instructions.iter().any(|l| l.contains("NOT reached")));
    // Desk ledger disagrees with the exchange.
    let rep = checkpoint::build_report(
        CheckpointKind::Stage2,
        1,
        rep.flatten.clone(),
        Ok(bal),
        "INR",
        Some(d("3100")),
        None,
    );
    assert!(rep.verdict.contains("DIFFERS") && rep.difference == Some(d("-49.5")));
    assert!(rep.withdrawal.is_none());
}

#[tokio::test]
async fn checkpoint_reports_not_flat_and_unknown_balance_honestly() {
    let m = Mock {
        exit_does_nothing: true,
        ..Mock::new()
    };
    m.open_position("B-SOL_INR", 1.0);
    let flat = checkpoint::flatten_all(&m, &["INR"], Poll::IMMEDIATE).await;
    assert!(!flat.flat && !flat.residual.is_empty());
    let rep = checkpoint::build_report(
        CheckpointKind::Stage1,
        1,
        flat.clone(),
        Err(PrivateError::Transport {
            message: "down".into(),
        }),
        "INR",
        None,
        None,
    );
    assert!(rep.verdict.starts_with("NOT FLAT"));
    let mut f2 = flat;
    f2.flat = true;
    let rep = checkpoint::build_report(
        CheckpointKind::Adhoc,
        1,
        f2,
        Err(PrivateError::Transport {
            message: "down".into(),
        }),
        "INR",
        None,
        None,
    );
    assert!(rep.verdict.contains("unknown") && rep.balances.is_none());
}

#[test]
fn withdrawal_reconciliation_uses_actual_balance() {
    let before = checkpoint::Balances {
        futures: [("INR".to_string(), d("3010"))].into(),
        spot: [("INR".to_string(), d("40"))].into(),
        ..Default::default()
    };
    let after = checkpoint::Balances {
        at_ms: 9,
        futures: [("INR".to_string(), d("1050"))].into(),
        ..Default::default()
    };
    let r = checkpoint::reconcile_withdrawal(&before, &after, "INR", d("2000"), d("0"), d("0.01"));
    assert!(r.confirmed && r.retained_capital == d("1050"));
    let r = checkpoint::reconcile_withdrawal(&before, &after, "INR", d("1500"), d("0"), d("0.01"));
    assert!(
        !r.confirmed && r.retained_capital == d("1050") && r.verdict.starts_with("NOT CONFIRMED")
    );
}

#[tokio::test]
async fn reconciliation_against_desk_state() {
    let m = Mock::new();
    m.open_position("B-ETH_INR", 0.5);
    let snap = readiness::snapshot(&m, &["INR"], 1).await.unwrap();
    let flat_desk = DeskState::default();
    let r = readiness::reconcile(&snap, &flat_desk);
    assert!(!r.ok && r.unexpected.len() == 1);
    let desk = DeskState {
        open: vec![DeskPosition {
            pair: "B-ETH_INR".into(),
            quantity: d("0.5"),
        }],
        written_at_ms: 1,
    };
    assert!(readiness::reconcile(&snap, &desk).ok);
    let wrong = DeskState {
        open: vec![
            DeskPosition {
                pair: "B-ETH_INR".into(),
                quantity: d("0.4"),
            },
            DeskPosition {
                pair: "B-BTC_INR".into(),
                quantity: d("1"),
            },
        ],
        written_at_ms: 1,
    };
    let r = readiness::reconcile(&snap, &wrong);
    assert!(!r.ok && r.quantity_mismatch.len() == 1 && r.missing.len() == 1);
    let (st, _) = readiness::account_check(&m, &desk, 1).await;
    assert!(st.is_pass());
}
