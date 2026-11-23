# Live readiness: what the account owner runs, in order

Live trading is **off** unless you, the account owner, run the steps below
yourself, on your own machine, with your own credentials. Nothing in the desk,
the target controller, an agent or a schedule can arm live trading.

**Status of this tooling:** the developers built `qe-live-check` from
https://docs.coindcx.com/ (re-read 2026-11-23). They never ran it against the
real API and never handled credentials. It is tested against an in-memory
exchange and a local mock HTTP server that serves the documented example
responses. Until your own runs pass, treat every authenticated path as
**unverified**.

## Build once

```sh
cargo build --release -p qe-live-check --features live-orders
alias qlc=./target/release/qe-live-check
```

Building with `live-orders` compiles the order-sending code in. That alone
opens nothing. Every risk-opening call still needs the gate described below.
State goes to `data/live/`, which is gitignored. Set `QE_LIVE_STATE_DIR` to put
it somewhere else. Credentials come from `COINDCX_API_KEY` / `COINDCX_API_SECRET`
or a `chmod 600` file named by `QE_SECRETS_FILE` (default `./.env`). The tools
only ever print `configured` or `not configured`.

## Before arming (finish before the campaign starts at 00:00 IST, 24 Nov)

Current campaign: [CAMPAIGN_24NOV.md](CAMPAIGN_24NOV.md) (INR 100 → INR 50,000, deadline 22:00 IST 24 Nov 2026).

| # | Command | What it does | Places orders? |
| --- | --- | --- | --- |
| 1 | `qlc status` | Shows the gate, the arm state, the kill switch and every check with its age. No network. | no |
| 2 | `qlc readiness --read-account --pair <PAIR> --margin INR` | Checks credentials (configured or not) and runs the signing self-test. Checks public instrument metadata: active, not exit-only, increments, min notional, and that `market_order`, `take_profit_market` and `stop_market` are listed. Reads futures wallets, positions and live orders, then reconciles them against `data/live/desk_state.json` (absent means the desk believes it is flat). | no |
| 3 | `qlc governor` | Runs `cargo test -p qe-risk` and records RiskGovernorAcceptance. | no |
| 4 | `qlc validate --pair <PAIR> --margin INR --leverage 1` | Runs a **real** supervised test with the smallest permitted order (see below). | **yes, minimum size, after typed confirmation** |
| 5 | `qlc readiness --read-account --pair <PAIR> --margin INR` | Re-checks that the account is flat. Account reconciliation must be under 15 min old when you arm. | no |
| 6 | `qlc arm --hours 3` | Arms live trading. Needs every check passed recently and you typing `I-AM-THE-ACCOUNT-OWNER-AND-ACCEPT-LIVE-TRADING-RISK` on a terminal. Audited. | no |
| 7 | Start the desk with `QE_EXECUTION_MODE=live` and `QE_LIVE_OPERATOR_CONFIRM=<same phrase>` | The desk's gate opens only while steps 1-6 hold and the arm record has not expired. | desk, within governor limits |

### How long a passing check counts as recent at arming time

| Check | Max age |
| --- | --- |
| Credentials, signing self-test, account reconciled | 15 min |
| Instrument requirements | 6 h |
| Protective orders verified, 120 s closure tested, risk-governor acceptance | 24 h |

Arming snapshots the checks. While the arm record is valid (at most 24 h,
default 3 h), the gate does not re-expire on account-check age. Expiry, `disarm`
or `kill` closes it.

### What `validate` does (step 4)

1. **Pre-flight.** The account must have no position and no open or untriggered
   order in that margin currency. Otherwise nothing is sent.
2. **Typed confirmation `LEVERAGE <PAIR> <N>x`**, then `update_leverage`. The
   tool refuses anything above the instrument's max long leverage.
3. **Typed confirmation `BUY <QTY> <PAIR> REAL`** for the whole remaining
   sequence, then a market entry of the minimum quantity. The minimum is
   max(min_quantity, min_notional x 1.02 / price), rounded up to the
   quantity increment. After that there are no further prompts, so an open
   position never waits on a keyboard:
4. it waits for the fill, sends `create_tpsl` (`take_profit_market` and
   `stop_market` at +/-1 % of the fill, on the price grid), and requires both
   legs to be accepted **and** listed as untriggered orders;
5. it sends `cancel_all_open_orders_for_position` and checks that the legs are gone;
6. it sends `positions/exit`, then `cancel_all_open_orders`, and checks that
   the account is flat. It measures the time from fill to flat, which must be
   120 s or less.

Every step is printed, audited to `data/live/audit.jsonl` and written to
`data/live/reports/validation-<ms>.json`. If anything fails after the entry
was sent, the tool runs an emergency flatten (cancel all, exit all, verify). If
it still cannot confirm flat, it prints `NOT VERIFIED FLAT`: **open the CoinDCX
app and close the position yourself.**

Fees are charged on the test trade. With INR 100 of capital, the minimum
notional of some INR pairs may need more margin than you have at 1x. The
exchange then rejects the entry, and nothing is assumed. Choose a liquid pair
whose minimum order fits your balance. Raise `--leverage` only within the
instrument maximum, and only if you accept that risk.

## Kill switch (any time)

```sh
qlc kill --reason "why"                      # disarms now and blocks re-arming
qlc checkpoint --kind adhoc                  # flatten (typed `FLATTEN ALL`)
qlc clear-kill                               # typed `CLEAR KILL SWITCH`; you stay DISARMED
```

A kill never blocks risk-reducing calls (cancel, exit, cancel-all), so a
disarmed desk can still close what it holds. The desk can call
`qe_live_check::arming::kill` itself, for example on a governor halt.

## (Historical) 22:30 IST Stage 1 checkpoint and INR 2,000 withdrawal

> **Historical, removed by the owner on 23 Nov 2026.** The later "INR 50,000 by
> 17:00 IST" deadline is historical too. The current campaign has no
> intermediate withdrawal and one final checkpoint at **22:00 IST on 24 Nov 2026**
> ([CAMPAIGN_24NOV.md](CAMPAIGN_24NOV.md)). The `stage1` tool below is kept for
> reference only.

```sh
qlc checkpoint --kind stage1 --currency INR --expected <desk realised equity>
```

1. Disarms first, so the desk makes no new entries.
2. After you type `FLATTEN ALL`: cancels all open orders (USDT and INR), cancels
   protection on each position, exits every position, and polls until flat.
3. Reads the **actual** futures wallet and spot balances, keeping currencies
   separate and converting nothing.
4. Writes `data/live/reports/checkpoint-stage1-<ms>.json` with the verdict:
   flat or not, the actual balance, and the difference from the desk ledger.
   It also writes the withdrawal preparation. If the actual INR balance is
   INR 2,000 or more, it tells you how to withdraw **in the CoinDCX app
   yourself**. If not, it says the target was **not reached** and that Stage 2
   starts from the actual balance with unchanged risk limits.
5. After **you** withdraw manually:

```sh
qlc reconcile-withdrawal --before data/live/reports/checkpoint-stage1-<ms>.json --amount 2000 [--fee <fee>]
```

This only reads balances. It reports whether before minus withdrawn minus fee
equals the actual balance (tolerance 0.01, change it with `--tolerance`). It
also gives the retained capital, always the **actual** balance, which Stage 2
must start from.

6. For Stage 2, re-run steps 2, 5 and 6 above (`readiness`, then `arm`).

## 22:00 IST 24 Nov: campaign final checkpoint (was 17:00, now historical)

```sh
qlc checkpoint --kind stage2 --currency INR --expected <desk realised equity>
```

This is the same flatten, verify, balance and report sequence. It also sets the
**kill switch**, so no position is carried past the checkpoint. If it prints
`NOT VERIFIED FLAT`, the account is not flat: close it in the CoinDCX app.

## Hooks for the desk (`qe_live_check`)

* `arming::gate_for_desk(&StateDir::from_env(), creds_configured, now_ms)` builds
  the `LiveGate` for each risk-opening decision. Re-evaluate it on every order
  so expiry and kill take effect immediately.
* `arming::arm_status`: secret-free JSON for the dashboard.
* `arming::kill`: the desk-side kill switch.
* `checkpoint::flatten_all(&private_client, &["USDT","INR"], Poll::DEFAULT)` and
  `checkpoint::read_balances`.
* The desk writes `data/live/desk_state.json` (`{"open":[{"pair":..,"quantity":".."}],"written_at_ms":..}`)
  so readiness can reconcile the exchange against what the desk believes.

## What is NOT verified

The timestamp unit (ms), the GET-with-body wallet call, the `create_tpsl`
response shape and the `update_leverage` field names all follow the docs'
examples where the docs contradict themselves (see `docs/SECURITY.md`). The
first `readiness --read-account` and `validate` runs on your account are the
verification.
