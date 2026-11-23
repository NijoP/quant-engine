# Going live: exact owner steps

Paper is the default and stays the default. Nothing below runs unless you, the
account owner, do it yourself. No developer or agent has run these steps with
real credentials. Treat every authenticated path as **unverified** until your
own runs pass.

The plan now has one capital target: **INR 50,000 by 17:00 IST on 24 Nov 2026**,
measured from the real wallet balance. The 22:30 Stage 1 target and the INR 2,000
withdrawal checkpoint are removed. The 17:00 final checkpoint stays. The target
never overrides the risk governor: 1% per trade, session loss limits, one
position, mandatory SL and TP, and a 120 s lifetime.

## 1. Keys

```sh
cat > .env <<'EOF'
COINDCX_API_KEY=...
COINDCX_API_SECRET=...
EOF
chmod 600 .env          # the loader refuses a group- or world-readable file
git status --short .env # must print nothing (.env is gitignored)
```

The tools never print, log or send keys to the dashboard or to agents. They
only report `configured` or `not configured`.

## 2. Build with the order path compiled in

```sh
cargo build --release -p qe-live-check --features live-orders
cargo build --release -p qe-desk --features live
alias qlc=./target/release/qe-live-check
```

`--features live` compiles the signed order path into the desk. That alone
opens nothing: every risk-opening call still needs an armed gate.

## 3. Readiness check (no orders)

```sh
qlc status
qlc readiness --read-account --pair <PAIR> --margin INR
```

This checks:
* whether credentials are configured
* the HMAC signing self-test
* instrument metadata: active, not exit-only, increments, min notional, and
  `market_order`, `take_profit_market` and `stop_market` listed
* wallets, positions and open orders, reconciled against the desk's state

## 4. Protective-order validation (real, minimum size)

```sh
qlc validate --pair <PAIR> --margin INR --leverage 1
qlc readiness --read-account --pair <PAIR> --margin INR   # confirm flat again
```

This places the smallest permitted order, attaches TP and SL, confirms the
exchange accepted both legs, then exits. It asks for typed confirmation first.

## 5. Arm

```sh
qlc arm --hours 3
```

Type `I-AM-THE-ACCOUNT-OWNER-AND-ACCEPT-LIVE-TRADING-RISK` when asked. The arm
record expires after the hours you give (24 at most).

## 6. Start the desk in live mode

```sh
QE_EXECUTION_MODE=live \
QE_LIVE_OPERATOR_CONFIRM=I-AM-THE-ACCOUNT-OWNER-AND-ACCEPT-LIVE-TRADING-RISK \
./target/release/qe-desk
```

At start-up the desk prints `[live] ARMED: ...` or `[live] not armed, staying
on paper: <reason>`. It installs the live executor
(`crates/qe-desk/src/live_exec.rs`) only when the gate is fully open. It re-reads
the arm record and the kill switch every second, so `qlc disarm`, `qlc kill` and
arm expiry take effect while it runs. Arming happens only at start-up: after
re-arming, restart the desk.

Before each order it reads the **real** futures wallet balance. It refuses the
order if the governor's risk at stop exceeds the per-trade fraction of that real
balance. It also drops any proposal more than 2 s old (for example, one queued
behind an open trade).

> Not yet verified: none of this has run against the real exchange. Desk
> equity, the capital controller and the dashboard still show the desk's own
> (paper) accounting. Live results appear in `live.live_trades` on
> `/api/status`. The wallet balance there is the real figure.

What the executor does with each proposal the governor approved:

1. It refuses unless the gate is armed, the kill switch is clear, the governor
   approved, and SL and TP are on the correct sides of the price.
2. It sets leverage to the lower of the governor's approved leverage and the
   exchange maximum for the instrument and tier. If the exchange refuses the
   leverage, it sends no order.
3. It sends a market entry under the idempotent client order id `qe-<trade_id>`.
4. If the outcome is unknown (timeout or 5xx), it reconciles against the
   exchange's order list and never resubmits blindly. If the order is absent,
   it stops. If the outcome stays unresolved, the state becomes
   `reconciliation_required`.
5. It confirms the fill from the exchange's position state. **The 120 000 ms
   timer starts at the confirmed fill.** On a partial fill it cancels the
   unfilled remainder.
6. It places exchange-side TP and SL. If either leg is not accepted, it exits
   at once and records an incident.
7. It monitors until the protective order fills, the kill switch is set, or
   120 s pass, then exits.
8. It cancels leftover orders for the position, verifies that the account is
   flat, and reads the **real** futures wallet balance. Paper equity is never
   shown as real.

## 7. Watch

```sh
qlc status                                   # gate, arm state, kill switch, check ages
curl -s 127.0.0.1:8787/api/status | jq .live # desk view (QE_DESK_ADDR overrides the address)
```

## 8. Kill, disarm, final checkpoint

```sh
qlc kill --reason "why"      # disarms now and blocks re-arming
qlc disarm                   # ends the arm record
qlc checkpoint --kind adhoc  # flatten everything (typed FLATTEN ALL)
qlc clear-kill               # typed CLEAR KILL SWITCH; you stay disarmed
```

A kill never blocks risk-reducing calls (cancel, exit, cancel-all), so an open
position can always be flattened. At 17:00 IST on 24 Nov run
`qlc checkpoint --kind stage2 --currency INR --expected <desk equity>` to stop
entries, flatten, cancel and reconcile. Never carry a position past it.
