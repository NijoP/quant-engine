# Owner start at 00:00 IST, 24 Nov 2026 (gamble profile, live, at your own risk)

You chose to run gamble mode live toward the hourly schedule. The engineering verdict is still
**NO-GO** ([LIVE_GO_NO_GO.md](LIVE_GO_NO_GO.md)): there is no measured edge, and no authenticated
path has run against your account. Only you can do the steps below. No agent arms, reads keys or
places orders.

What this means in money: bets have negative expected value after costs. Bold play only
maximises the *chance* of a fixed target in a losing game, and that chance is tiny
(`/api/state.gamble.p_target`). Expect to end at your floor. Losses can exceed the stop
through gaps, slippage or liquidation.

## Steps

1. **Keys**: `.env` with `COINDCX_API_KEY` / `COINDCX_API_SECRET`, `chmod 600 .env` ([GO_LIVE.md](GO_LIVE.md) §1).
2. **Build**:
   ```sh
   cargo build --release -p qe-live-check --features live-orders
   cargo build --release -p qe-desk --features live
   alias qlc=./target/release/qe-live-check
   ```
3. **Readiness and the real protective-order test** (all must PASS; [LIVE_READINESS.md](LIVE_READINESS.md)):
   ```sh
   qlc readiness --read-account --pair <PAIR> --margin INR
   qlc governor
   qlc validate --pair <PAIR> --margin INR --leverage 1
   qlc readiness --read-account --pair <PAIR> --margin INR   # flat again
   ```
4. **Edit `configs/campaign-live.toml`** yourself:
   - set `enabled = true` and paste `owner_ack` exactly;
   - set `floor_inr`, `stake_inr` and `leverage` (the desk caps leverage at the exchange maximum);
   - set `execution_mode = "live"`.

   Keep `stake_policy = "fixed"`. At main `22fabe5` the desk **refuses to start** live with
   `target_optimal` (bold play), because it sizes from desk equity, not the real wallet. Bold play
   is available only if the campaign branch changes that and its CI is green.
5. **Arm**: `qlc arm --hours 23`, then type the owner phrase.
6. **Start at 00:00 IST**:
   ```sh
   QE_GAMBLE_CONFIG=configs/campaign-live.toml QE_EXECUTION_MODE=live \
   QE_LIVE_OPERATOR_CONFIRM=I-AM-THE-ACCOUNT-OWNER-AND-ACCEPT-LIVE-TRADING-RISK \
   ./target/release/qe-desk --profile gamble
   ```
   It must print `[live] ARMED`. If it prints `not armed, staying on paper`, it is not live.
7. **Kept regardless:**
   - one position at a time, SL and TP, 120 s maximum hold;
   - kill switch, data-integrity and minimum-notional checks (size is never raised to a minimum);
   - no size-up after a loss;
   - floor checked against the **real wallet** before every order;
   - a floor hit is a permanent stop (restart needs `--gamble-rearm`).
8. **Stop any time**: `qlc kill --reason "..."`, then `qlc checkpoint --kind adhoc`.
9. **22:00 IST**: `qlc checkpoint --kind stage2 --currency INR --expected <desk equity>`. Trust
   "flat" only if reconciliation confirms it. On `NOT VERIFIED FLAT`, close the position in the
   CoinDCX app.
