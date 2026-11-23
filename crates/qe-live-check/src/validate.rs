//! Protective-order validation (2), run BY THE OWNER, with the smallest
//! permitted order on a liquid instrument:
//!
//! 1. pre-flight: account flat (no positions, no live orders);
//! 2. typed confirmation, then `update_leverage` (never above the instrument max);
//! 3. typed confirmation of the WHOLE sequence, then a market entry of the
//!    minimum quantity, immediately followed (no further prompt, so the
//!    position is never left waiting on a keyboard) by:
//! 4. wait for the fill, read the exchange's position for the pair and require
//!    `margin_type == "isolated"` (anything else or missing: emergency flatten,
//!    verify flat, stop; no TP/SL is placed), then `create_tpsl` (take_profit_market + stop_market),
//!    verify both legs accepted AND listed as untriggered orders;
//! 5. `cancel_all_open_orders_for_position`, verify the legs are gone;
//! 6. `positions/exit`, verify flat, measure fill -> flat (must be <= 120 s);
//!    this is "fill-to-flat within 120 s during validation (immediate close)"
//!    only: it does NOT prove the desk's automatic 120 s holding-time
//!    enforcement, which is unit-tested only and not proven live;
//! 7. `cancel_all_open_orders` and a final flat check.
//!
//! Any failure after the entry was sent runs the emergency flatten
//! (cancel-all + exit + verify) and reports loudly. Every step is printed and
//! audited.
use crate::api::AccountApi;
use crate::checkpoint::{flatten_all, FlattenReport};
use crate::confirm::Confirm;
use crate::money::{ceil_to, dec, floor_to};
use crate::readiness::snapshot;
use crate::Poll;
use qe_coindcx::live::{CheckItem, CheckStatus};
use qe_coindcx::private::types::{NewOrder, Side};
use qe_coindcx::private::FuturesPosition;
use qe_coindcx::types::FuturesInstrument;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Serialize;

pub const MAX_CLOSURE_MS: i64 = 120_000;
/// The only margin type the validation accepts on the opened position.
pub const REQUIRED_MARGIN_TYPE: &str = "isolated";
/// What the `Closure120sTested` check proves (the serialized key is kept for
/// `checks.json` / `armed.json` compatibility).
pub const CLOSURE_CHECK_LABEL: &str =
    "fill-to-flat within 120 s during validation (immediate close)";
/// Stated wherever the closure check is shown: the validation closes at once,
/// so it never exercises the desk's own 120 s holding-time exit.
pub const DESK_HOLD_ENFORCEMENT_NOTE: &str =
    "desk 120 s holding-time enforcement: unit-tested only, not proven live";
/// TP/SL distance from the fill for the test (1 %): far enough not to trigger
/// during a test lasting seconds, close enough to be accepted.
pub const TEST_BRACKET_PCT: Decimal = Decimal::from_parts(1, 0, 0, false, 2);

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub pair: String,
    pub margin: String,
    pub side: Side,
    pub leverage: u32,
    pub quantity: Decimal,
    pub reference_price: Decimal,
    pub est_notional: Decimal,
    pub price_increment: Decimal,
}

/// Smallest order satisfying min_quantity and min_notional (+2 % headroom for
/// price movement), on the quantity grid. Leverage defaults to 1x and is
/// refused above the instrument's long max.
pub fn plan(i: &FuturesInstrument, reference_price: f64, leverage: u32) -> Result<Plan, String> {
    let px = dec(reference_price)
        .filter(|p| *p > Decimal::ZERO)
        .ok_or("no valid reference price")?;
    let step = dec(i.quantity_increment)
        .filter(|s| *s > Decimal::ZERO)
        .ok_or("no quantity increment")?;
    let pxi = dec(i.price_increment)
        .filter(|s| *s > Decimal::ZERO)
        .ok_or("no price increment")?;
    let min_qty = dec(i.min_quantity).unwrap_or_default();
    let min_notional = dec(i.min_notional).unwrap_or_default();
    if leverage < 1 || f64::from(leverage) > i.max_leverage_long {
        return Err(format!(
            "leverage {leverage}x outside 1..={} (instrument max long)",
            i.max_leverage_long
        ));
    }
    if !(i.status == "active" && !i.exit_only) {
        return Err(format!(
            "{} is not tradeable (status {}, exit_only {})",
            i.pair, i.status, i.exit_only
        ));
    }
    let headroom = Decimal::new(102, 2);
    let by_notional = ceil_to(min_notional * headroom / px, step);
    let qty = ceil_to(min_qty.max(by_notional), step).normalize();
    if let Some(maxq) = i.max_market_order_quantity.and_then(dec) {
        if maxq > Decimal::ZERO && qty > maxq {
            return Err(format!(
                "minimum qty {qty} exceeds max market order qty {maxq}"
            ));
        }
    }
    Ok(Plan {
        pair: i.pair.clone(),
        margin: i.margin_currency_short_name.clone(),
        side: Side::Buy,
        leverage,
        quantity: qty,
        reference_price: px,
        est_notional: (qty * px).round_dp(4),
        price_increment: pxi,
    })
}

/// TP above / SL below the fill for a long (reverse for a short), on the price grid.
pub fn bracket(side: Side, fill: Decimal, pxi: Decimal) -> (Decimal, Decimal) {
    let up = fill * (Decimal::ONE + TEST_BRACKET_PCT);
    let down = fill * (Decimal::ONE - TEST_BRACKET_PCT);
    match side {
        Side::Buy => (ceil_to(up, pxi), floor_to(down, pxi)),
        Side::Sell => (floor_to(down, pxi), ceil_to(up, pxi)),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    pub at_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub plan: Plan,
    pub steps: Vec<Step>,
    pub entry_sent: bool,
    pub protective_verified: bool,
    pub closure_ms: Option<i64>,
    pub flat_at_end: bool,
    pub emergency: Option<FlattenReport>,
    pub aborted_by_owner: bool,
    /// `margin_type` the exchange reported for the opened position (`None`
    /// when no position was observed or the field was missing).
    pub margin_type: Option<String>,
    /// True only when that value is `isolated`.
    pub margin_type_verified: bool,
}

impl Outcome {
    pub fn protective_status(&self) -> CheckStatus {
        if self.protective_verified {
            CheckStatus::Pass(format!(
                "{} create_tpsl both legs accepted and listed; cancel verified",
                self.plan.pair
            ))
        } else if !self.entry_sent {
            CheckStatus::NotRun("validation stopped before the entry order".into())
        } else {
            CheckStatus::Fail(self.failure_summary())
        }
    }
    pub fn closure_status(&self) -> CheckStatus {
        match (self.flat_at_end, self.closure_ms) {
            (true, Some(ms)) if ms <= MAX_CLOSURE_MS => CheckStatus::Pass(format!(
                "{CLOSURE_CHECK_LABEL}: {} verified flat {ms} ms after fill (limit {MAX_CLOSURE_MS} ms). Does NOT prove the desk's automatic 120 s holding-time enforcement ({DESK_HOLD_ENFORCEMENT_NOTE})",
                self.plan.pair
            )),
            (true, Some(ms)) => CheckStatus::Fail(format!(
                "{CLOSURE_CHECK_LABEL}: flat only after {ms} ms (> {MAX_CLOSURE_MS} ms)"
            )),
            _ if !self.entry_sent => {
                CheckStatus::NotRun("validation stopped before the entry order".into())
            }
            _ => CheckStatus::Fail(self.failure_summary()),
        }
    }
    fn failure_summary(&self) -> String {
        self.steps
            .iter()
            .filter(|s| !s.ok)
            .map(|s| format!("{}: {}", s.name, s.detail))
            .collect::<Vec<_>>()
            .join("; ")
    }
    pub fn checks(&self) -> [(CheckItem, CheckStatus); 2] {
        [
            (
                CheckItem::ProtectiveOrdersVerified,
                self.protective_status(),
            ),
            (CheckItem::Closure120sTested, self.closure_status()),
        ]
    }
}

struct Run<'a> {
    steps: Vec<Step>,
    now: &'a dyn Fn() -> i64,
    echo: bool,
}

impl Run<'_> {
    fn step(&mut self, name: &str, ok: bool, detail: impl Into<String>) -> bool {
        let s = Step {
            name: name.into(),
            ok,
            detail: detail.into(),
            at_ms: (self.now)(),
        };
        if self.echo {
            println!(
                "[{}] {:<22} {}",
                if ok { " OK " } else { "FAIL" },
                s.name,
                s.detail
            );
        }
        self.steps.push(s);
        ok
    }
}

/// True only for the exact margin type `isolated` (ASCII case ignored).
pub fn is_isolated(margin_type: Option<&str>) -> bool {
    margin_type.is_some_and(|m| m.trim().eq_ignore_ascii_case(REQUIRED_MARGIN_TYPE))
}

fn find_open<'p>(ps: &'p [FuturesPosition], pair: &str) -> Option<&'p FuturesPosition> {
    ps.iter()
        .find(|p| p.pair.as_deref() == Some(pair) && p.is_open())
}

/// Run the validation. `now` is the wall clock (injected for tests);
/// `echo` prints each step as it happens.
pub async fn run<A: AccountApi, C: Confirm>(
    api: &A,
    confirm: &mut C,
    plan: Plan,
    poll: Poll,
    now: &dyn Fn() -> i64,
    echo: bool,
) -> Outcome {
    let mut r = Run {
        steps: vec![],
        now,
        echo,
    };
    let margins = [plan.margin.as_str()];
    let out =
        |r: Run, entry_sent, protective_verified, closure_ms, flat_at_end, emergency, aborted| {
            Outcome {
                plan: plan.clone(),
                steps: r.steps,
                entry_sent,
                protective_verified,
                closure_ms,
                flat_at_end,
                emergency,
                aborted_by_owner: aborted,
                margin_type: None,
                margin_type_verified: false,
            }
        };

    // 1. pre-flight
    match snapshot(api, &margins, now()).await {
        Ok(s) if s.is_flat() => {
            r.step(
                "preflight",
                true,
                format!("{} margin account flat", plan.margin),
            );
        }
        Ok(s) => {
            r.step("preflight", false, format!(
                "account not flat: {} exposed positions, {} live orders; flatten first (qe-live-check checkpoint)",
                s.exposed().len(),
                s.live_orders.len()
            ));
            return out(r, false, false, None, false, None, false);
        }
        Err(e) => {
            r.step("preflight", false, format!("account read failed: {e}"));
            return out(r, false, false, None, false, None, false);
        }
    }

    // 2. leverage
    let phrase = format!("LEVERAGE {} {}x", plan.pair, plan.leverage);
    if !confirm.confirm(
        &format!(
            "Step 2/7 will call update_leverage({}, {}, {}x) on your REAL account.",
            plan.pair, plan.margin, plan.leverage
        ),
        &phrase,
    ) {
        r.step("leverage", false, "owner did not confirm");
        return out(r, false, false, None, true, None, true);
    }
    match api
        .update_leverage(&plan.pair, &plan.margin, plan.leverage)
        .await
    {
        Ok(a) if a.is_success() => {
            r.step("leverage", true, format!("{}x accepted", plan.leverage));
        }
        Ok(a) => {
            r.step(
                "leverage",
                false,
                format!("not acknowledged: {:?}", a.message),
            );
            return out(r, false, false, None, true, None, false);
        }
        Err(e) => {
            r.step("leverage", false, e.to_string());
            return out(r, false, false, None, true, None, false);
        }
    }

    // 3. entry (confirms the whole remaining sequence)
    let side = plan.side.as_str().to_uppercase();
    let phrase = format!("{side} {} {} REAL", plan.quantity, plan.pair);
    if !confirm.confirm(
        &format!(
            "Step 3/7 places a REAL market {side} of {} {} (~{} {} notional at {}x), then without pausing attaches TP/SL, cancels them, exits and verifies flat. Fees are charged.",
            plan.quantity, plan.pair, plan.est_notional, plan.margin, plan.leverage
        ),
        &phrase,
    ) {
        r.step("entry", false, "owner did not confirm");
        return out(r, false, false, None, true, None, true);
    }
    let qty = plan.quantity.to_f64().unwrap_or(0.0);
    let coid = format!("qe-val-{}", now());
    let entry = api
        .create_order(
            &coid,
            NewOrder::market(plan.side, &plan.pair, qty, plan.leverage, &plan.margin),
        )
        .await;
    let entry_detail = match &entry {
        Ok(o) => format!("order {:?} status {:?}", o.id, o.status),
        Err(e) => e.to_string(),
    };
    r.step("entry", entry.is_ok(), entry_detail);
    // Even a rejected/unknown entry may have filled: always verify and flatten.
    let mut fill: Option<(FuturesPosition, i64)> = None;
    for attempt in 0..poll.attempts.max(1) {
        if attempt > 0 {
            tokio::time::sleep(poll.interval).await;
        }
        if let Ok(ps) = api.positions(&margins).await {
            if let Some(p) = find_open(&ps, &plan.pair) {
                fill = Some((p.clone(), now()));
                break;
            }
        }
        if entry.is_err() && attempt >= 1 {
            break;
        }
    }
    let Some((pos, t_fill)) = fill else {
        r.step("fill", false, "no open position observed");
        let em = flatten_all(api, &margins, poll).await;
        let flat = em.flat;
        r.step(
            "emergency_flatten",
            flat,
            format!("flat={flat} errors={:?}", em.errors),
        );
        return out(r, true, false, None, flat, Some(em), false);
    };
    let pos_id = pos.id.clone().unwrap_or_default();
    let fill_px = pos.avg_price.and_then(dec).unwrap_or(plan.reference_price);
    r.step(
        "fill",
        true,
        format!(
            "position {pos_id} active_pos {:?} avg {fill_px}",
            pos.active_pos
        ),
    );

    // 4a. margin type, from the exchange's own position response for the pair
    // (the positions read that observed the fill). Anything but isolated:
    // no TP/SL, emergency flatten, verify flat, stop.
    let margin_type = pos.margin_type.clone();
    let isolated = is_isolated(margin_type.as_deref());
    let margin_seen = (margin_type.clone(), isolated);
    r.step(
        "margin_type",
        isolated,
        match (&margin_type, isolated) {
            (Some(m), true) => format!("exchange reports margin_type {m:?} for {pos_id} (verified isolated)"),
            (Some(m), false) => format!(
                "exchange reports margin_type {m:?} for {pos_id}, expected {REQUIRED_MARGIN_TYPE:?}: stopping, emergency flatten"
            ),
            (None, _) => format!(
                "exchange position {pos_id} has no margin_type, expected {REQUIRED_MARGIN_TYPE:?}: stopping, emergency flatten"
            ),
        },
    );
    if !isolated {
        let em = flatten_all(api, &margins, poll).await;
        let flat = em.flat;
        r.step(
            "emergency_flatten",
            flat,
            if flat {
                format!("verified flat; errors={:?}", em.errors)
            } else {
                format!(
                    "NOT FLAT: {:?} errors {:?} - CHECK THE COINDCX APP NOW",
                    em.residual, em.errors
                )
            },
        );
        let mut o = out(r, true, false, None, flat, Some(em), false);
        (o.margin_type, o.margin_type_verified) = margin_seen;
        return o;
    }

    // 4b. protection
    let (tp, sl) = bracket(plan.side, fill_px, plan.price_increment);
    let mut protective = false;
    match api
        .create_tpsl(
            &pos_id,
            tp.to_f64().unwrap_or(0.0),
            sl.to_f64().unwrap_or(0.0),
        )
        .await
    {
        Ok(t) if t.fully_protected() => {
            r.step(
                "create_tpsl",
                true,
                format!("TP {tp} (take_profit_market) + SL {sl} (stop_market) accepted"),
            );
            let listed = match api.live_orders(&margins).await {
                Ok(os) => os
                    .iter()
                    .filter(|o| o.pair.as_deref() == Some(plan.pair.as_str()))
                    .filter(|o| {
                        o.status
                            .as_deref()
                            .is_some_and(|s| s.eq_ignore_ascii_case("untriggered"))
                    })
                    .count(),
                Err(_) => 0,
            };
            protective = r.step(
                "tpsl_listed",
                listed >= 2,
                format!("{listed} untriggered orders listed for {}", plan.pair),
            );
        }
        Ok(t) => {
            r.step(
                "create_tpsl",
                false,
                format!(
                    "not fully protected: TP {:?} SL {:?}",
                    t.take_profit, t.stop_loss
                ),
            );
        }
        Err(e) => {
            r.step("create_tpsl", false, e.to_string());
        }
    }

    // 5. cancel protection (only when it was placed; emergency path otherwise)
    if protective {
        let cancelled = api.cancel_all_for_position(&pos_id).await;
        r.step(
            "cancel_tpsl",
            cancelled.is_ok(),
            match &cancelled {
                Ok(_) => "cancel_all_open_orders_for_position sent".to_string(),
                Err(e) => e.to_string(),
            },
        );
        let mut gone = false;
        for attempt in 0..poll.attempts.max(1) {
            if attempt > 0 {
                tokio::time::sleep(poll.interval).await;
            }
            if let Ok(os) = api.live_orders(&margins).await {
                if !os
                    .iter()
                    .any(|o| o.pair.as_deref() == Some(plan.pair.as_str()))
                {
                    gone = true;
                    break;
                }
            }
        }
        protective = r.step(
            "cancel_verified",
            gone,
            if gone {
                "no live orders left for the pair"
            } else {
                "orders still listed"
            },
        ) && cancelled.is_ok();
    }

    // 6. exit + 7. verify flat (the same flatten the checkpoint uses)
    let ex = api.exit_position(&pos_id).await;
    r.step(
        "exit",
        ex.is_ok(),
        match &ex {
            Ok(a) => format!("positions/exit group {:?}", a.group_id()),
            Err(e) => e.to_string(),
        },
    );
    let fin = flatten_all(api, &margins, poll).await;
    let t_flat = now();
    let closure = fin.flat.then_some(t_flat - t_fill);
    r.step(
        "verify_flat",
        fin.flat,
        match closure {
            Some(ms) => format!("flat {ms} ms after fill"),
            None => format!(
                "NOT FLAT: {:?} errors {:?} - CHECK THE COINDCX APP NOW",
                fin.residual, fin.errors
            ),
        },
    );
    let flat = fin.flat;
    let emergency = (!protective || ex.is_err()).then_some(fin);
    let mut o = out(r, true, protective, closure, flat, emergency, false);
    (o.margin_type, o.margin_type_verified) = margin_seen;
    o
}
