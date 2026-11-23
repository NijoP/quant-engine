# Risk policy (crate `qe-risk`)

The Risk Governor (`RiskGovernor::evaluate_proposal`) is the only path to an approved
order. Nothing else, including the target controller, can override it or loosen limits.
`RiskPolicy::clamped()` clamps any configured value to the mode ceiling.

| Parameter | Growth | Protective |
|---|---|---|
| Risk per trade (fraction of current equity) | 1% | 0.5% |
| Session loss limit (of stage starting equity) | 5% | 2% |
| Max drawdown from high-water mark | 5% | 2% |
| Liquidation buffer (multiple of stop distance) | 1.5 | 2.0 |

## Rejection rules (all reasons are recorded)
Kill switch, paused entries, final target reached, session loss limit, HWM floor,
max open positions, inactive / exit-only instrument, market order unsupported,
unhealthy data, **uncalibrated forecast**, expected net edge <= minimum, missing FX,
stop/target on the wrong side, min quantity or min notional above the risk budget
(**the budget is never raised**), no leverage tier admitting the notional, liquidation
too close to the stop, margin above available.

## Sizing (independent of leverage)
- `R = equity * risk_fraction * loss_step_down^consecutive_losses`
- `N_max = R / ((stop_distance + cost_per_unit) * fx_to_account)`, then floor to the quantity increment and cap at max quantity
- `Margin = Notional / Leverage`

## Leverage
Start at the highest exchange-permitted leverage for the side (`max_leverage_long` or
`max_leverage_short`), within the dynamic tier that admits the notional. Step down one
unit at a time until the estimated isolated liquidation
`entry * (1 -/+ (1/L - mmr))` lies at least `liquidation_buffer * stop_distance` away.
Leverage never changes the quantity.

## No escalation
A loss halves the effective risk fraction and a win restores it. The ceiling is never exceeded.

## 24 Nov campaign risk profile (current)
Applies to the INR 100 → INR 50,000 campaign ([CAMPAIGN_24NOV.md](CAMPAIGN_24NOV.md),
example `configs/campaign-24nov.toml`), on top of the governor rules above. No limit is loosened
to make the schedule reachable.

| Rule | Value |
|---|---|
| Campaign floor (permanent stop) | INR 95 |
| Max planned loss per trade | INR 1, including fees, slippage and the stop-loss loss |
| Max cumulative campaign loss | INR 5 from INR 100 |
| Open positions / max hold | 1 / 120 s |
| Margin | isolated where supported |
| Size-up after a loss, martingale, averaging down | never |
| Raise size to meet an exchange minimum | never (rejected) |
| Stop beyond estimated liquidation | never (rejected) |
| Behind schedule | reporting only: no change to leverage, size, thresholds or entries |

The floor or loss-budget stop is permanent: no new entries, kill switch preserved, audited, owner
review required for any new campaign.

## Capital stages (historical)
> Historical. The stage plan below (checkpoint and withdrawal) is superseded by the 24 Nov
> campaign, which has no intermediate withdrawal.

`CapitalManager`: Stage 1 Growth -> at the checkpoint (22:00 IST) `AwaitingCheckpoint`
(stop entries, flatten) -> `confirm_withdrawal(amount, user_confirmed=true, confirmed_balance)`
-> Stage 2 Protective -> Complete at the final target. An unconfirmed withdrawal is
never recorded.
