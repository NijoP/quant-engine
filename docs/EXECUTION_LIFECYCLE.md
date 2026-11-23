# 120-second position lifecycle (crate `qe-risk`)

A deterministic state machine (`Position`). Time is always injected as `now_ms`, so
tests are reproducible. It emits `Action`s; the execution layer (paper now, a live
adapter later) performs them and feeds results back in.

```
Validated -start-> EntrySubmitted -on_fill(partial)-> Filling -on_fill(full)/on_entry_done_partial-> ProtectionPending
ProtectionPending -on_protection_confirmed-> Protected
ProtectionPending -on_protection_failed-> Exiting (+Incident)   never continue unprotected
open -SL/TP/edge<0/data fault/time/emergency-> Exiting -on_exit_fill(flat)-> Closed
EntrySubmitted|Exiting -ack timeout-> Reconciling -on_reconcile(exchange qty)-> protect / re-exit / Closed / Rejected
```

- The timer starts at the **first confirmed fill**, not at submission. `T_MAX_MS = 120000`.
- The time exit fires at `T_MAX - exit_lead_ms` (default 117 s), so closure completes by 120 s.
  If a position is ever observed at >= 120 s, a `deadline_breach` incident is emitted with the exit.
- Entry price is the weighted average over partial fills. Entering an exit while still `Filling`
  first cancels the entry remainder.
- After a timeout, the machine emits `Reconcile` and never a blind resend. Client ids are deterministic
  (`{trace}-entry`, `{trace}-exit-{n}`).
- Realised P&L = side * (exit value - avg_entry * qty) - entry fees - exit fees, converted to the account currency.
- Paper fills (`PaperExchange::market_fill`): buys fill at ask*(1+slip) and sells at bid*(1-slip),
  with the taker fee on notional. A crossed or empty book gives no fill.
- Controls: `kill_switch`, `pause_entries`, `flatten_actions`, `cancel_entry_actions` return `ControlResult`.

Not yet done: exchange-side SL/TP order types for CoinDCX (`create_tpsl`) and live
reconciliation are the lead's adapter's job and are **unverified**. Live mode stays disabled.
