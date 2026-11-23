# Owner start, 24 Nov 2026: 23-hour campaign (campaign bold play, live, at your own risk)

> **23-HOUR SCHEDULE (campaign command update, 24 Nov 2026).** Clock 00:00 IST,
> **first campaign entry permitted 01:00 IST**, deadline **23:00 IST** (23 h),
> baseline E0 = INR 101.45, target INR 50,000, schedule
> `101.45 * (50000/101.45)^(h/23)` (about 30.9407 % per hour, a mathematical
> requirement, not a forecast). The 22-hour schedule (22:00 IST, `100 * 500^(h/22)`)
> is **superseded**: do **not** run a binary built before this change. Download the
> fresh `qe-desk-linux-x86_64` artifact from the CI run of the updated campaign
> branch. If `data/desk-campaign/campaign_state.json` was written by the old
> 22-hour binary, move it aside first (the new desk refuses it and stays stopped).
> No risk setting changed (floor, leverage, sizing, SL/TP, isolated margin, one
> position, 120 s, kill switch, permanent stop).

This file is for branch `campaign/24nov-50k-hourly-target` (PR #24).

You chose to run the campaign bold-play profile live toward the hourly schedule
`101.45 * (50000/101.45)^(h/23)` (00:00 → 23:00 IST; entries from 01:00 IST). The engineering verdict is still **NO-GO** ([LIVE_GO_NO_GO.md](LIVE_GO_NO_GO.md)):
- there is no measured edge;
- the forecast is uncalibrated;
- no authenticated path has ever run against your account.

Only you can do the steps below. No agent reads keys, arms or places orders.

What this means in money:
- Each bet is sized so one take-profit win reaches the **next hourly milestone**, limited to the equity above your floor.
- Bets have negative expected value after costs. Expect to end at your floor.
- Losses can exceed the stop through gaps, slippage or liquidation.

Nothing is built on your machine: CI builds both binaries.

## Steps

1. **Get the binaries (no build).** From a checkout of this branch (it holds `configs/`):
   ```sh
   git fetch origin && git checkout campaign/24nov-50k-hourly-target && git pull
   gh run list --branch campaign/24nov-50k-hourly-target --workflow CI --limit 1   # note the run id
   gh run download <run-id> -R NijoP/quant-engine -n qe-desk-linux-x86_64   # unpacks target/release/{qe-desk,qe-live-check} + dashboard/dist
   chmod +x target/release/qe-desk target/release/qe-live-check
   ls -lh target/release/qe-desk target/release/qe-live-check     # both must exist
   alias qlc=./target/release/qe-live-check
   ```
   - The artifact `qe-desk-linux-x86_64` holds `qe-desk` built with `--features live` and `qe-live-check` built with `--features live-orders`. It is kept for 3 days.
   - Run both from the repository root: the desk reads `configs/`, `data/` and `dashboard/dist` from the current directory, or from `QE_REPO_ROOT` if you set it.
2. **Keys.** Create `.env` in the repository root with `COINDCX_API_KEY=...` and `COINDCX_API_SECRET=...`, then run `chmod 600 .env`. Confirm git ignores it with `git check-ignore -v .env`, which must print `.gitignore:...:.env`. Never commit it.
3. **Read-only check (READ-ONLY: places nothing).** Run `qlc status` first (READ-ONLY: local arming and kill state), then:
   ```sh
   ./target/release/qe-desk --readonly-check
   ```
   - It prints futures wallets, spot balances, open positions, open orders and the **INR campaign equity** (INR futures wallet balance + locked). Secrets are redacted.
   - Compare the figures with the CoinDCX app. **Stop if they differ or the check fails.**
   - While live, the desk repeats this read every 10 s. If a read fails, or the balance is more than 30 s old, the campaign refuses new entries.
   - USDT wallets are never converted or added into the INR equity.
4. **Readiness.** All must PASS ([LIVE_READINESS.md](LIVE_READINESS.md)):
   ```sh
   qlc readiness --read-account --pair <PAIR> --margin INR
   qlc governor
   ```
   Both are READ-ONLY.

   **Choose `<PAIR>`** from the public list of active INR-margined futures (no key needed):
   ```sh
   curl -s 'https://api.coindcx.com/exchange/v1/derivatives/futures/data/active_instruments?margin_currency_short_name[]=INR' | tr ',' '\n' | head -40
   ```
   Pick a liquid pair from that output, e.g. `B-BTC_USDT` or `B-ETH_USDT`, only if it is listed. Its INR minimum notional is on the dashboard's instrument panel, and the desk rejects instruments whose minimum does not fit.

   **OPTIONAL - REAL MONEY ORDER:** `qlc validate --pair <PAIR> --margin INR --leverage 1`.
   - It places a **real, small order with protective SL/TP orders** on your account, then closes it.
   - It costs fees and can lose money. It is the only end-to-end proof of the order path.
   - Right after the fill it reads the position from the exchange and requires `margin_type` = `isolated`. Otherwise it places no TP/SL, flattens, verifies flat and stops. The margin type it saw is printed and audited.
   - Its 120 s result is **fill-to-flat within 120 s during validation (immediate close)**. It does **not** prove the desk's automatic 120 s holding-time enforcement, which is unit-tested only and not proven live.
   - Afterwards, run a **fresh reconciliation**: `qlc readiness --read-account --pair <PAIR> --margin INR` and `./target/release/qe-desk --readonly-check`. Both must show no position and no open order.
5. **Edit `configs/campaign-live.toml`** yourself:
   - `enabled = true`; paste `owner_ack` exactly as written in the file.
   - `floor_inr = "<your floor>"`: **required, no default**. Equity at or below it is a permanent stop.
   - `leverage = <whole number>`: the desk caps it at the instrument maximum per side and tier.
   - `stake_inr = "<n>"`: required by the validator, but replaced per bet by `campaign_stake = "next_milestone"`.
   - Keep `campaign_stake = "next_milestone"`, `stake_policy = "fixed"` and `entry = "maker"` (post-only maker entry).
   - Set `execution_mode = "live"`.
6. **Arm (changes live state, places no order):** `qlc arm --hours 23`, then type the owner phrase. Check with `qlc status`.
7. **Start (00:00–01:00 IST is preparation; the desk refuses campaign entries before 01:00 IST):**
   ```sh
   QE_GAMBLE_CONFIG=configs/campaign-live.toml QE_EXECUTION_MODE=live \
   QE_LIVE_OPERATOR_CONFIRM=I-AM-THE-ACCOUNT-OWNER-AND-ACCEPT-LIVE-TRADING-RISK \
   ./target/release/qe-desk --profile gamble
   ```
   - It must print `[live] ARMED`. If it prints `not armed, staying on paper`, it is **not** live.
   - The dashboard is at http://127.0.0.1:8787, in the **24 NOVEMBER CAMPAIGN** section. Its equity source must read `live_exchange` with a number, not `null`.
8. **Kept regardless:**
   - one position at a time; exchange SL and TP on every entry (flatten if protection is not confirmed); SL inside liquidation;
   - 120 **second** maximum hold (desk 120 s holding-time enforcement: unit-tested only, not proven live; `validate` does not prove it);
   - kill switch, data-integrity and minimum-notional checks (size is never raised to a minimum);
   - the floor is checked against the real wallet before every order;
   - a floor hit is a permanent stop. A restart needs `--gamble-rearm`, and `data/desk-campaign/campaign_state.json` must be archived to start a new campaign.
9. **Emergency stop, any time:**
   ```sh
   qlc status
   qlc kill --reason "OWNER EMERGENCY STOP"
   qlc checkpoint --kind adhoc
   ```
   If the checkpoint is not verified flat, close the position in the CoinDCX app.
10. **23:00 IST** (the old 22:00 deadline is superseded):
    - The desk switches to exit-only by itself and refuses new entries. On live it records `verified_flat=false` and raises an incident, because it never claims flat on its own.
    - Run `qlc checkpoint --kind stage2 --currency INR --expected <desk equity>`.
    - Trust "flat" only if reconciliation confirms it. On `NOT VERIFIED FLAT`, close the position in the CoinDCX app.
