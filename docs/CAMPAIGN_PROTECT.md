# Launch-time protective campaign (24 Nov 2026, deadline 23:30 IST)

Profile: `qe-desk --profile campaign-protect` reads `configs/campaign-protect.toml`
(`QE_CAMPAIGN_CONFIG` overrides the path). Paper is the default.

## What it does

- **Campaign clock.** T0 is the instant the desk gets its first fresh verified
  INR futures-wallet read (at most 30 s old). E0 is that balance. If the read fails
  or is stale, the campaign does not activate and no entries are allowed. Paper
  runs activate on the paper ledger and use a separate data directory, so a paper
  E0 never becomes a live baseline. A live desk refuses a persisted
  paper-activated state.
- **Target curve.** `Target(t) = E0 * (50000/E0) ^ ((t - T0) / (D - T0))`, where
  D = 2026-11-24 23:30 IST (epoch ms 1795543200000). There is one checkpoint per
  whole hour from T0, plus a final checkpoint at 23:30. The curve is only a
  progress measure: it never changes sizing, leverage, gates or entries.
- **Limits, derived from E0 at activation.** Planned loss per trade is at most
  min(INR 1, 1% of E0), counting stop loss, fees and slippage. Cumulative loss is
  at most min(INR 5, 5% of E0). The floor is at least 95% of E0. Hitting the
  cumulative limit or the floor stops the campaign permanently.
- **Positions and orders.** One position at a time. Each position is closed
  120 s after the confirmed fill, measured with a monotonic clock. Margin is
  isolated, leverage is capped at 5x (and never above the instrument maximum),
  and entries are maker orders. An order below the exchange minimum notional is
  rejected, never resized up.
- **Strategy gates.** The normal gates apply unchanged: calibration authority and
  positive net EV after actual costs. Expect most or all opportunities to be
  rejected.
- **23:30 IST cutoff.** At 23:30 the campaign switches to exit-only, and that
  state is persisted across restarts. The live task refuses new jobs. Any open
  trade finishes through its own TP/SL or its 120 s exit. Flat is reported only
  when a read-only exchange snapshot, started after the executor went idle, shows
  0 positions and 0 open orders. If that snapshot is missing or the account is
  not flat, an incident is raised and the owner must reconcile.
- **Restart recovery.** T0, E0, the checkpoint table, exit-only state and any
  permanent stop are restored from `campaign_state.json`. A persisted profile
  that was loosened or does not match is refused.
- **Series statistics.** `/api/state.campaign.series` reports the trade count,
  wins and losses, win rate, average win and average loss, realised EV per trade
  and maximum drawdown.

## Live post-fill verification (every live trade)

After a fill, the desk reads the exchange position and checks:

1. The quantity is greater than 0 and no more than the requested quantity.
2. A fill price is reported.
3. The leverage equals the leverage that was set.
4. `margin_type == isolated`.

It then places TP and SL with `create_tpsl`. Both legs must also be **listed**
on the exchange as `untriggered` orders for the pair; an accepted API response
is not enough.

If any of these steps fails, the desk does the following:

- emergency exit;
- verify flat;
- record an incident (`incident-<trade>.json` in the live state dir);
- campaign **permanent stop**;
- set the kill switch.

The same permanent stop applies if a position stays open longer than 120 s +
15 s, or if flat cannot be verified. It also applies when, with no live trade in
flight, the exchange shows any position or open order (reconciliation failure).

## First genuine trade protection (no test order)

```
qe-live-check arm --hours 22 --first-trade-verifies-protection
```

Arming still requires every other readiness item to have passed recently.
`ProtectiveOrdersVerified` and `Closure120sTested` may be NotRun but must not be
Fail. The owner types the usual confirmation phrase, and the prompt states that
the first trade is financially exposed from the moment it fills.

The first filled live trade records both items with source `first live trade`:

- If its verification succeeds, `ProtectiveOrdersVerified` is recorded as PASS.
- If the trade closes within 120 s + 15 s with flat verified, `Closure120sTested`
  is recorded as PASS.
- A failure is recorded as FAIL, which closes the live gate. Later trades never
  overwrite these records.

## Owner commands (live)

1. In `configs/campaign-protect.toml`, set `execution_mode = "live"`.
2. Run the readiness checks (`qe-live-check readiness ...`), then arm:
   `qe-live-check arm --hours 22 --first-trade-verifies-protection`.
3. Start the desk:
   `QE_EXECUTION_MODE=live QE_LIVE_OPERATOR_CONFIRM='<phrase>' qe-desk --profile campaign-protect`
   (use the `qe-desk-linux-x86_64` CI artifact).
4. Kill at any time with `qe-live-check kill`.
