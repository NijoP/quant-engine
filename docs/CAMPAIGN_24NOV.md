# 24 November 2026 campaign: INR 100 to INR 50,000 (22 h, mathematical schedule)

This is the single current capital campaign. It replaces every earlier target
(INR 18,000, INR 3,000 by 22:30 IST plus an INR 2,000 withdrawal, and
INR 50,000 by **17:00** IST). Those are kept in older docs as **historical** only.

| | |
|---|---|
| Starting capital | INR 100 |
| Start | 24 Nov 2026, 00:00 IST (`2026-11-23T18:30:00Z`, epoch ms `1795458600000`) |
| Deadline | 24 Nov 2026, 22:00 IST (`2026-11-24T16:30:00Z`, epoch ms `1795537800000`) |
| Final target | INR 50,000 (500x) |
| Duration | 22 h, one continuous campaign |
| Withdrawals | none in the middle, and no campaign reset |
| Max holding time | 120 s per position |
| Execution | **paper by default**. Live stays owner-controlled and disabled (see [GO_LIVE.md](GO_LIVE.md)) |

> **The hourly curve is a mathematical requirement, not a trading forecast.** It
> says what equity *would have to be* to stay on a constant compounded path to
> INR 50,000. It is not an expected return, a prediction or a reason to take
> more risk.

## Mathematics

```
multiplier          = 50000 / 100 = 500
hourly_multiplier   = 500 ^ (1/22)          = 1.326418146...
hourly growth       = hourly_multiplier - 1 = 32.6418 % per hour (compounded)
target_equity(h)    = 100 * 500 ^ (h / 22),   0 <= h <= 22
```

`h` is the number of elapsed hours since the start, as a real number (partial
hours allowed). Before the start `h` is clamped to 0, and after the deadline to 22.
For example, at 00:30 IST `h = 0.5` and the requirement is INR 115.17.

The code computes the formula. It does not look the values up in a list. The table
below is the regression reference that the tests check against.

### Rounding (defined once, used everywhere)

* Internal values stay unrounded (`f64` or decimal) in every calculation: deltas,
  required rates and status.
* **Displayed milestone values** are rounded to the nearest whole rupee, half away
  from zero (`f64::round`).
* "Increase from previous" is the difference of the two **rounded** milestones, so
  the column adds up exactly to INR 49,900.
* API money fields are decimal strings. Milestone `required_inr` values carry the
  rounded whole-rupee value. Live amounts (equity, P&L, delta) carry 2 dp.

### Hourly schedule (IST, 24 Nov 2026)

| h | Time | Required equity (INR) | Increase (INR) | Unrounded |
|---:|---|---:|---:|---:|
| 0 | 00:00 | 100 | - | 100.0000 |
| 1 | 01:00 | 133 | 33 | 132.6418 |
| 2 | 02:00 | 176 | 43 | 175.9385 |
| 3 | 03:00 | 233 | 57 | 233.3680 |
| 4 | 04:00 | 310 | 77 | 309.5436 |
| 5 | 05:00 | 411 | 101 | 410.5842 |
| 6 | 06:00 | 545 | 134 | 544.6064 |
| 7 | 07:00 | 722 | 177 | 722.3758 |
| 8 | 08:00 | 958 | 236 | 958.1724 |
| 9 | 09:00 | 1,271 | 313 | 1270.9372 |
| 10 | 10:00 | 1,686 | 415 | 1685.7942 |
| 11 | 11:00 | 2,236 | 550 | 2236.0680 |
| 12 | 12:00 | 2,966 | 730 | 2965.9611 |
| 13 | 13:00 | 3,934 | 968 | 3934.1047 |
| 14 | 14:00 | 5,218 | 1,284 | 5218.2678 |
| 15 | 15:00 | 6,922 | 1,704 | 6921.6052 |
| 16 | 16:00 | 9,181 | 2,259 | 9180.9427 |
| 17 | 17:00 | 12,178 | 2,997 | 12177.7690 |
| 18 | 18:00 | 16,153 | 3,975 | 16152.8137 |
| 19 | 19:00 | 21,425 | 5,272 | 21425.3853 |
| 20 | 20:00 | 28,419 | 6,994 | 28419.0198 |
| 21 | 21:00 | 37,696 | 9,277 | 37695.5036 |
| 22 | 22:00 | 50,000 | 12,304 | 50000.0000 |

### Catch-up rate (reporting only)

If actual equity `E` at elapsed hours `h` is below the curve, the controller
reports the compounded hourly rate that would still be needed:

```
required_hourly_rate = (50000 / E) ^ (1 / (22 - h)) - 1
```

It is `null` when `E <= 0` or when no time is left. **This number is only
displayed.** It feeds no sizing, leverage, signal threshold or entry decision.

## Campaign controller (what it tracks)

The campaign controller is a reporting and stopping component. The `campaign`
object in `/api/state` ([DASHBOARD_API.md](DASHBOARD_API.md)) exposes:

* start and deadline timestamps, starting equity, final target
* current **actual** equity, with `equity_source` = `live_exchange` or `paper`
  and a currency note
* previous and next hourly milestone, the requirement now, actual minus required
* realised P&L, and **unrealised P&L labelled separately**
* remaining amount and remaining time, drawdown from INR 100, required future rate
* completed checkpoints, and a status of `ahead`, `on_track`, `behind`,
  `stopped`, `completed` or `expired`

### Equity source and currency

* Live campaign reporting uses the **real exchange balance**. Paper equity is
  always labelled `paper` and is never shown as real capital.
* INR and USDT balances are never silently added together. A combined INR
  figure shows the conversion source (for example the spot USDTINR mid and its
  timestamp) in `equity_currency_note`. The desk's existing INR-margin P&L
  uses the spot USDTINR mid, which only approximates the exchange's own rate.

## Campaign risk profile

An example profile is in [`configs/campaign-24nov.toml`](../configs/campaign-24nov.toml)
(paper). These limits are fixed. They are not loosened to make the target reachable.

| Rule | Value |
|---|---|
| Initial capital | INR 100 |
| Absolute campaign floor | INR 95 |
| Max planned loss per trade | INR 1, **including** estimated fees, slippage and the stop-loss loss |
| Max cumulative campaign loss | INR 5 from the initial INR 100 |
| Open positions | 1 at a time |
| Max holding time | 120 s |
| Margin | isolated where supported |
| Martingale, averaging down, doubling after a loss | never |
| Size increase to recover a missed hourly target | never |
| Raising size to meet an exchange minimum order | never: the trade is rejected |
| Stop-loss beyond the estimated liquidation price | never: the trade is rejected |

The governor computes **actual loss exposure at the stop**:
`qty x (|entry - stop| + entry x round-trip cost rate)`. It does not use margin
size. A trade is rejected when the exchange minimum order, fees, slippage,
stop distance or leverage push that exposure above INR 1, or above the
remaining loss budget.

### Floor and loss-budget stop (permanent)

When equity reaches INR 95 or the INR 5 loss budget is used up:

1. no new positions are opened;
2. the campaign enters a **permanent** `stopped` state, and a restart keeps it;
3. the kill switch is preserved and never cleared automatically;
4. the event is written to the audit log;
5. a new campaign needs explicit owner review. Nothing restarts automatically.

### Honest arithmetic: most trades will be rejected, correctly

With INR 1 of allowed loss and an exchange minimum notional of roughly INR 625 on
INR futures (it varies per instrument and comes from live metadata):

```
max loss / min notional = 1 / 625 = 0.16 % of notional, for costs + stop distance together
round-trip cost (2 x taker + spread + 2 x slippage) ~ 0.10-0.16 %
=> stop distance available ~ 0.06 % or less, often nothing
```

A 120 s stop that tight sits inside normal noise for most contracts. So the
governor will reject most candidates, and it should. Starting at INR 100,
reaching INR 50,000 under these limits needs a sustained measured edge that the
forecast does not have today (see "Known state" below). The schedule records the
gap. It does not close it.

## Owner override: live gamble profile

On 23 Nov 2026 the owner chose a live gamble profile from 00:00 IST:
- bold play toward the schedule, at max leverage, with a 120 s hold;
- an owner-set floor in place of the INR 1 / INR 5 caps.

It is set up in `configs/campaign-live.toml` (`campaign_stake = "next_milestone"`: each bet is sized so one TP win reaches the next milestone, capped by the equity above the floor), shipped off and on paper, with steps in
[OWNER_START_0000.md](OWNER_START_0000.md). The engineering verdict stays **NO-GO**
([LIVE_GO_NO_GO.md](LIVE_GO_NO_GO.md)). The rules below describe the engineering-recommended
profile (`configs/campaign-24nov.toml`). Under the owner's gamble profile the schedule *does* inform
stake size, by the owner's choice.

## The hourly target never controls risk (recommended profile)

When the campaign is behind schedule, the controller **only reports** the gap
and the catch-up rate. It never:

* raises leverage or position size;
* lowers the signal-quality, calibration or net-EV threshold;
* forces a trade or tells the execution engine to enter;
* disables stop-losses or bypasses the risk governor.

A missed milestone is a reporting condition. It is not permission to take more
risk. The dashboard shows the gap neutrally, without countdown pressure styling
that pushes toward larger trades.

## Opportunity selection (unchanged gates)

Candidates (long and short, across supported CoinDCX spot, INR futures and USDT
futures) are ranked by measured risk-adjusted net EV after spread, taker fees,
slippage, liquidity and minimum notional. Each decision records its full numeric
explanation in `decisions.jsonl`. Stale, incomplete or unreliable data is
rejected. **For any live trade, the calibration-authority and positive-net-EV
gates stay mandatory.** Gamble mode is a paper/research profile and is not a
route around them (see [GAMBLE_MODE.md](GAMBLE_MODE.md)).

## Deadline procedure (22:00 IST, 24 Nov 2026)

1. Stop accepting new entries. The campaign goes to `exit_only = true`.
2. Cancel the remaining entry orders.
3. Where live execution is properly authorised (owner-armed, per
   [GO_LIVE.md](GO_LIVE.md)), run the existing validated risk-reducing
   flatten: `qe-live-check checkpoint --kind stage2 --currency INR --expected <desk equity>`.
4. Reconcile the actual positions and open protective orders with the exchange.
5. Report "flat" **only** if the exchange reconciliation confirms it. Otherwise
   report `NOT VERIFIED FLAT` and alert the owner to close the position in the
   CoinDCX app.
6. Record the final actual equity and the result (`completed` if at least
   INR 50,000, otherwise `expired`).

If the live close path is not verified, the desk still stops new entries and
alerts the owner. It never reports an execution that did not happen.

## Known state at HEAD (re-checked 23 Nov 2026, main `22fabe5`)

| Statement | Still true? | Evidence |
|---|---|---|
| The desk defaults to paper | yes | `configs/risk-limits.toml` `execution_mode = "paper"`. Live needs `--features live`, the owner arm record and `QE_EXECUTION_MODE=live` |
| Live execution not ready | yes | The live executor ([GO_LIVE.md](GO_LIVE.md)) exists but has only run against mocks. `LIVE_EXECUTION_VALIDATED`: NO |
| Forecast not calibrated, Brier worse than climatology | yes | `crates/qe-research/calibration.json`: `desk.fp.v1` status `calibrating`, Brier skill −0.006. Sprint 3 challengers: none promotable ([CALIBRATION.md](CALIBRATION.md)) |
| No authenticated path validated against the real account | yes | `qe-live-check readiness`/`validate` have never run with real credentials ([LIVE_READINESS.md](LIVE_READINESS.md)) |
| Gamble mode skips calibration and positive-EV checks | yes at `22fabe5`, so it is reclassified | [GAMBLE_MODE.md](GAMBLE_MODE.md) now marks it as a paper/research profile, kept out of the campaign's live path |

**Live trading for this campaign remains blocked** until the owner runs every
readiness step personally and a model earns calibration authority. No agent arms
live trading, reads credentials or places real orders.
