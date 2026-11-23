# Live go / no-go: 24 Nov 2026 campaign

**Decision: NO-GO for live trading.** Paper only. Decided 23 Nov 2026, 23:20 IST, against main
`22fabe5` plus the campaign branch. Evidence: [PRODUCTION_READINESS.md](PRODUCTION_READINESS.md).

The rule: live is GO only when **every** condition below is PASS. One unresolved condition means NO-GO.

| Condition | Status | Why it blocks |
|---|---|---|
| Forecast has calibration authority (P0-3) | FAIL | `desk.fp.v1` is `calibrating`, Brier skill −0.006, no promotable challenger |
| Positive net EV after costs out of sample (P0-4) | FAIL | Realised −9.5 to −13.6 bps per trade after about 13 bps of cost |
| Real-account credentials and signing (P0-5) | NOT TESTED | `qlc readiness --read-account` never run with real keys |
| Real exchange TP/SL on a minimum order (P0-6) | NOT TESTED | `qlc validate` only ran against mocks |
| 120 s live closure, verified flat (P0-7) | NOT TESTED | No live position ever opened |
| Live executor (P0-8) | NOT TESTED | Mock-tested only |
| Campaign floor, INR 1 / INR 5 caps, no size-up when behind (P0-9 to P0-11) | BLOCKED | Pending the campaign PR's green CI and review |
| 23:00 IST exit-only and verified-flat reporting (P0-12; 22:00 superseded) | BLOCKED / NOT TESTED | Desk side pending; live flatten never run for real |
| Gamble mode out of the live path (P0-14) | FAIL (code) | `gamble.toml` still accepts `execution_mode = "live"` at `22fabe5` |

Even if the engineering items were all PASS, P0-3 and P0-4 would keep this NO-GO: there is no
measured edge. Under the INR 1 per-trade cap with a ~INR 625 minimum notional, almost every
candidate is correctly rejected. The INR 50,000 schedule is a mathematical requirement, not an
expectation, and nothing may raise risk to chase it.

## Owner override (recorded 23 Nov 2026, ~23:20 IST)

The owner chose to trade **live from 00:00 IST** in gamble mode (`configs/campaign-live.toml`):
- bold play toward the hourly ~33 % demand, at the highest leverage the exchange allows, on any instrument, with a 120 s hold;
- the INR 1 / INR 5 caps are replaced by an owner-set floor.

That is the owner's decision, at the owner's own risk, with the owner's own money. It does **not**
change this engineering verdict. Every NOT TESTED item above stays unverified until the owner's own
runs pass, and gamble mode skips the calibration and positive-EV gates (P0-3, P0-4).

Facts that hold at main `22fabe5`:
- `target_optimal` is still refused live. Campaign bold play runs as `stake_policy = "fixed"` + `campaign_stake = "next_milestone"` (configs/campaign-live.toml, PR #24) and is allowed live under owner arming. Verdict unchanged: NO-GO.
- Leverage is an owner number capped at the exchange max.
- The floor is checked against the real wallet before each order.

Steps: [OWNER_START_0000.md](OWNER_START_0000.md). No agent arms live trading.

## What would change this

1. A model meets the fixed promotion criteria in [CALIBRATION.md](CALIBRATION.md) with positive
   net out-of-sample results.
2. The owner personally runs the [LIVE_READINESS.md](LIVE_READINESS.md) sequence (readiness,
   governor, supervised `validate`, readiness) and every item is PASS.
3. The campaign PR is merged with green CI covering the campaign risk tests.
4. The owner alone arms (`qlc arm`). No agent arms live trading, reads credentials or places orders.

Until then the desk runs the campaign tracker on **paper** equity, labelled PAPER.
