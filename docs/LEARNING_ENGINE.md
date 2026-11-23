# Learning engine (`crates/qe-learn`)

The learn-from-mistakes engine implements intent §6.1–6.6. It is a pure library.
It makes no network calls, reads no credentials and places no orders. It records,
measures, classifies and reports. It **cannot** raise leverage, size or risk, and
it **cannot** remove a stop. The risk governor remains the only authority over a trade.

Money is `rust_decimal::Decimal`: prices, quantities, fees, funding, P&L,
leverage, margin and risk budgets. Statistics are `f64`: returns,
probabilities, errors and bps estimates. Every output type serialises with serde
to JSON for the dashboard.

## Data flow

```
proposal ──► record_decision(DecisionSnapshot)   §6.1  sealed, SHA-256, append-only
                │ (approved and rejected proposals alike; BEFORE any order)
position closed + all horizons (<=120 s) elapsed
             ──► record_outcome(TradeOutcome)      §6.2  derived Decimal accounting
                    ├─ classify (calibration evidence from BEFORE this trade)  §6.3
                    ├─ online calibration update (tp_first, win, up_{h}s)      §6.5 A
                    └─ regime monitor: forecast residual z                      §6.5 B
market ticks ──► observe_market(instr, ts, {volatility, spread_bps, depth})    §6.5 B
desk         ◄── signal(instr): LearningSignal, multipliers in [0,1]            §6.6
dashboard    ◄── dashboard(as_of, Some(Mode::Paper|Live)), metrics(), lessons() §6.4/§6.5 D
models       ──► registry_mut().register / add_eval / promote / rollback        §6.5 C
```

## API for s3-desk

| call | returns | notes |
|---|---|---|
| `LearningEngine::open(cfg, path)` | engine | Replays the JSONL journal and verifies every hash and sequence number. It refuses to open on tampering (`LearnError::Tampered`). |
| `record_decision(snap)` | `DecisionReceipt {trade_id, seq, sha256}` | Validates the snapshot. Rejects duplicates and timestamps earlier than the last decision. |
| `record_outcome(o)` | `OutcomeRecord` | Rejects unknown or duplicate trades, out-of-order `recorded_ms`, look-ahead, and unequal entry and exit quantities (`ReconciliationRequired`). |
| `classify(id)` | `&MistakeReport` | Computed once, at record time. |
| `lesson(id)`, `lessons(filter, n)` | `LessonReport` | `lessons` lists the most recent first. |
| `metrics(as_of_ms, &Filter)` | `MetricsReport` | Uses only outcomes with `recorded_ms <= as_of_ms`. |
| `observe_market(instr, ts, MarketObs)` | `RegimeState` | |
| `signal(instr)` | `LearningSignal` | Shrink-only. Apply it with `apply_confidence(p)` and `apply_size(qty)`. |
| `calibrate(model, target, p)` | `Calibrated` | Returns raw `p` unless the bin is `Stable`. |
| `dashboard(as_of_ms, mode)` | `LearningDashboard` | Feeds the mandatory learning and mistakes section. |
| `registry()`, `registry_mut()`, `set_registry(r)` | `Registry` | Serde-serialisable, so you can persist it as JSON. |

Paper and live are separated: every record carries `mode`, and filters select
by mode. The dashboard label reads `PAPER`, `LIVE` or `PAPER+LIVE`, and lesson
text is prefixed `PAPER` or `LIVE`.

## §6.1 Decision snapshot (immutable)

The snapshot holds these fields:

- trade id, instrument and market (`spot` / `inr_futures` / `usdt_futures`), mode, timestamp and direction
- features, regime, multi-timeframe context and order-book features
- data quality: book age, trade age, book valid, book snapshot interval
- per-horizon forecasts from {1, 5, 10, 15, 30, 60, 90, 120} s: expected return, sd, P(up) and CI
- trade horizon, win probability, P(TP first), P(SL first) and P(neither), which is derived
- expected MFE and MAE, ensemble disagreement
- estimated spread, fee and slippage in bps (round trip)
- reference price, stop, target, qty, notional, margin, leverage and sizing note
- strategy and model version, the risk governor verdict (approved, codes, risk budget) and reasoning codes

The snapshot is validated before it is stored. Probabilities must lie in [0, 1]
with P(TP) + P(SL) ≤ 1. Every number must be finite and horizons must ascend
within the allowed set. An approved trade must have a stop and a target on the
correct sides of the reference price, plus qty > 0, leverage ≥ 1 and a risk
budget > 0. Rejected proposals are recorded too, and no outcome can ever attach
to them.

Once stored, a snapshot is never modified. The log hands out only shared
references, each sealed record carries a SHA-256 over its canonical JSON, and
the journal is opened in append mode.

## §6.2 Outcome (appended separately)

The caller supplies these raw inputs:

- entry and exit fills (time, price, qty, fee)
- the exit reference price, which is the SL/TP trigger or the mid at a time exit
- funding, exit reason, MFE and MAE
- the actual mid move at every horizon (`None` = not observed; it is never guessed)
- flags: stale data, external event, exchange error, execution error, entry and exit spread, regime after, realised volatility
- `recorded_ms`

The engine derives weighted average entry and exit, fees, gross and net P&L
(gross − fees − funding), entry notional, holding time (first entry fill to last
exit fill), a partial-fill flag, entry and exit slippage in bps (positive =
adverse), actual cost in bps and the net return.

Leakage guards:

- An entry fill cannot precede the decision.
- An exit fill cannot precede the last entry fill.
- An observed horizon move needs `decision_ts + h ≤ recorded_ms`.
- Outcomes must arrive in `recorded_ms` order.

## §6.3 Mistake classifier

The classifier runs on every losing trade and on every poorly executed trade. A
trade counts as poorly executed when any of these holds:

- actual cost > 1.5 × estimate and > estimate + 2 bps
- a partial fill
- an exchange or execution error
- stale data
- holding > 125 s

Forecast causes are assessed only for losers, because a win does not prove the
model right. A loss with no rule firing gets the verdict `uncertain`. The report
then says whether the realised move was inside the forecast CI, which means the
loss is consistent with noise and is not evidence of model error. When the
trade-horizon move was not observed, the verdict is also `uncertain`.

In the table, `sd` is the predicted return sd at the trade horizon and `a` is
the actual move at that horizon. *Known* means directly measured; *probable*
means inferred.

| cause | rule (defaults in `ClassifierRules`) | certainty |
|---|---|---|
| incorrect_direction | sign(a) ≠ direction and \|a\| > 0.25 sd | probable |
| incorrect_magnitude | right sign and \|a − predicted\| / sd > 2 | probable |
| incorrect_timing | stopped out but the trade-horizon move ended in the trade direction | probable |
| volatility_estimate | max(MFE, MAE) / sd outside [0.3, 3.0] | probable |
| probability_calibration | \|p_tp_first − empirical bin rate\| > 0.15 with bin n_eff ≥ 30 (evidence from *before* this trade) | probable |
| regime_misclassification | observed regime after ≠ regime at decision | probable |
| order_book_snapshot_limitation | slippage rule fired and book snapshots > 1000 ms apart | probable |
| stale_data | book or trade age > 3000 ms, book invalid, or feed went stale while open | known |
| spread_expansion | entry or exit spread > 2 × estimate and > estimate + 2 bps | known |
| slippage | slippage beyond the touch (entry + exit slip − est spread) > 2 × estimate and > estimate + 2 bps | known |
| partial_fill | entry filled qty < planned qty | known |
| stop_too_tight | stopped out and stop distance < 0.75 sd | probable |
| target_too_ambitious | no TP and (target distance > 2 sd, or MFE reached ≥ 60 % of the target) | probable |
| model_disagreement | ensemble disagreement > 1 sd, or ≥ 2 horizons ≥ 30 s forecasting against the direction | probable |
| event_outside_information | \|z\| > 5 (uncertain) or tagged external event (known) | uncertain / known |
| strategy_execution_error | execution error flag, held > 120 s + 5 s grace, TP exit with negative gross, or exit reason `error` | known |
| risk_sizing_error | realised loss > 1.10 × risk budget | known |
| exchange_api_error | exchange error flag or exit reason `exchange_reject` | known |

Execution and data causes are scored on winners as well as losers.

## §6.4 Metrics (chronological)

For each horizon the engine reports n, mean error, mean absolute error, mean
squared error, directional accuracy (non-zero moves only), CI coverage, and a
Brier score, log loss and ECE (10 bins) for P(up) against `actual > 0`. For each
trade it scores P(TP first) against exit = TP, P(SL first) against exit = SL,
and win probability against net P&L > 0. It also compares predicted and actual
MFE and MAE, and estimated and actual cost in bps. Reports are as of a time t
and include only outcomes recorded by t. You can filter them by mode, model
version and instrument.

## §6.5 Controlled learning

**A. Online calibration.** One tracker per key `model/target`, where the target
is `tp_first`, `win` or `up_{h}s`. Each tracker holds 10 EW bins (half-life 200
observations) and an exact rolling window of 500. `recalibrate(p)` returns the
raw `p` until the key has ≥ 100 observations *and* p's bin has n_eff ≥ 30.
After that it returns `(hits + 20·p)/(n_eff + 20)`. Updates that arrive out of
order are refused.

**B. Regime adaptation.** Per instrument, the monitor tracks volatility,
liquidity (depth), spread and forecast residual z. Each signal has a slow EW
(half-life 300) and a fast EW (half-life 20). After 60 observations, a shift is
`|fast − slow| / slow_sd > 2`. A shift drops confidence to 0.5, and confidence
then recovers linearly over 50 further observations. The multiplier always lies
in [0.5, 1], so the regime monitor can only lower confidence.

**C. Champion and challenger.** The first registered model is the champion;
later models are challengers. Each spec carries a version, parent, training and
validation windows (validation must not overlap training), feature definitions,
assumptions, limitations and a calibration report link. Promotion criteria are
fixed when the registry is created and have no setter.

To be promoted, a challenger needs both of these:

- an out-of-sample evaluation with n ≥ 300
- a paper-forward evaluation with n ≥ 50, whose window starts after the challenger was registered

Each of the two evaluations must also show:

- a Brier score ≤ 0.98 × the champion's on the same kind of evaluation
- ECE ≤ 0.05
- net expectancy ≥ 0 bps after costs

The registry records every registration, promotion (with its evidence) and
rollback. `rollback` restores the previous champion and is always allowed.

**D. Lesson report.** Each report covers what was predicted, what happened, the
error and its z-score, the probable causes with evidence, whether each cause is
known or uncertain, a new hypothesis to test, and the evidence that would
justify a model change. For a model cause, that evidence is the full promotion
gate. For an execution cause, it is a tested fix plus 50 paper trades without
recurrence, with no model change.

## §6.6 Safety rules (enforced in `safety.rs`)

- `LearningSignal` is the only thing learning sends to the desk. Its confidence
  and size multipliers are clamped to [0, 1] (NaN becomes 0). It has no
  leverage, stop or limit field. `apply_size` never returns more than the
  approved quantity, and `apply_confidence` pulls p toward 0.5.
- `vet_adjustment` refuses any learning-originated change that raises leverage,
  per-trade risk or the session-loss limit, opens more concurrent positions,
  removes or widens the stop, or drops the take-profit requirement. This holds
  whatever the motive or evidence. Changes motivated by a recent loss, a losing
  streak or a missed target may only tighten. Pure tightenings are always
  allowed.
- Retraining on one losing trade and using it live is impossible. Promotion
  needs ≥ 300 out-of-sample plus ≥ 50 paper-forward trades, and the lesson
  report states that one trade never justifies a change.

## Verification status

| item | status |
|---|---|
| snapshot validation, sealing, tamper detection, append-only order | verified by unit tests |
| outcome derivation (weighted averages, long and short P&L, funding, slippage, leakage, reconciliation) | verified by unit tests |
| classifier: all 18 causes fire in tests; noise loss → uncertain; unobserved → uncertain; winners get no forecast causes | verified by unit tests |
| per-horizon metrics, as-of chronology, paper/live filter | verified by unit tests |
| calibration min-sample guards, EW forgetting, chronology | verified by unit tests |
| regime shift → floor → recovery; multiplier ≤ 1 | verified by unit tests |
| registry gate, leakage windows, rollback | verified by unit tests |
| §6.6 shrink-only signal and vet rules | verified by unit tests |
| journal replay and tamper refusal | verified by unit tests |
| default thresholds are well chosen for CoinDCX microstructure | **unverified**: heuristic starting values that need review against paper data |
| wiring into qe-desk and the dashboard panel | not in this crate (s3-desk owns it) |
