# QUANT-ENGINE

CoinDCX-first quantitative trading intelligence desk: Rust core and a React + TypeScript dashboard.
**Default execution mode: paper.** Live trading happens only through the owner's own arming; the canonical midnight procedure is [docs/OWNER_START_0000.md](docs/OWNER_START_0000.md) (CI artifact, no local build). Paper mode never reads API credentials or uses a private client, and
live order submission is refused (see `qe_coindcx::live::LiveGate`, which only the account owner
can satisfy, and which still requires checks that have not been run).

Paper equity is a configured value (INR 100 by default) and is labelled PAPER everywhere. It is
not a CoinDCX balance.

## 24 NOVEMBER CAMPAIGN (current)

One continuous campaign: **INR 101.45 (E0, last verified account read) → INR 50,000,
24 Nov 2026 00:00 → 23:00 IST** (23 h, first entry permitted 01:00 IST, no intermediate
withdrawals, no resets). The hourly schedule is `target_equity(h) = E0 · (50000/E0)^(h/23)`
(+30.9407 % per hour compounded). The earlier 22-hour schedule (INR 100 → 50,000 by 22:00 IST,
`100 · 500^(h/22)`) is superseded. It is a **mathematical requirement, not a trading forecast**,
and it never changes risk. Campaign risk profile: floor INR 95, at most INR 1 planned loss per
trade (fees, slippage and stop included), at most INR 5 cumulative campaign loss, one position,
120 s maximum hold, isolated margin, no martingale or size-up. No campaign entries before
01:00 IST; at 23:00 IST the desk turns exit-only. Live execution stays owner-controlled and disabled by default. Full maths, table,
rounding and deadline procedure: [docs/CAMPAIGN_24NOV.md](docs/CAMPAIGN_24NOV.md). Example
profile: `configs/campaign-24nov.toml`.

**Owner override:** the owner chose to run live gamble mode from 00:00 IST at their own risk
([docs/OWNER_START_0000.md](docs/OWNER_START_0000.md), `configs/campaign-live.toml`, shipped off and on paper).

**Engineering live decision: NO-GO** ([docs/LIVE_GO_NO_GO.md](docs/LIVE_GO_NO_GO.md)). The requirement-by-requirement evidence is in
[docs/PRODUCTION_READINESS.md](docs/PRODUCTION_READINESS.md).

Earlier targets (INR 18,000; INR 3,000 at 22:30 IST plus an INR 2,000 withdrawal; INR 50,000 by
17:00 IST) are **historical** and superseded.

## Run

```bash
# developers only; the owner's midnight path uses the CI artifact
cargo build && cargo test
(cd dashboard && npm ci && npm run build)
./target/debug/qe-desk          # http://127.0.0.1:8787  (dashboard at /, JSON API at /api/*)
```

`QE_DESK_ADDR` overrides the listen address; `QE_DATA_DIR` overrides the append-only audit
store (default `data/desk/`, gitignored). API reference: [docs/DASHBOARD_API.md](docs/DASHBOARD_API.md).

### Paper experiment profile (opt-in): PAPER EXPERIMENT - UNGATED, MAX LEVERAGE

```bash
./target/debug/qe-desk --profile experiment     # or QE_PROFILE=experiment
```

Loads `configs/experiment-max-leverage.toml`. Startup refuses (exit 2) unless every execution-mode
source is `paper` (the profile file, `risk-limits.toml`, `QE_EXECUTION_MODE`). The dashboard and
`/api/state` / `/api/status` (`profile`) show the banner. Rules: calibration gate and
session-loss / high-water-mark stops off; all paper equity committed as margin at the highest
exchange-permitted leverage for the side and notional tier (quantity rounded down, below min
notional rejected); one position at a time, compounded. Kept: raw EV > 0 after fees, spread and
slippage; model SL/TP; 120 s time stop; fees on every fill; audit trail; liquidation modelling (a
position whose touch reaches the estimated liquidation price closes as `liquidated`, margin lost).
Trading stops when equity is below the minimum tradable notional at max leverage. Audit store
defaults to `data/desk-experiment/`. The default profile is unchanged.

## Pipeline (what actually runs in `qe-desk`)

DATA → MATHEMATICS → PROBABILISTIC FORECAST → STRATEGY → RISK GOVERNOR → (paper) EXECUTION → FEEDBACK

| Stage | Implementation | Status |
|---|---|---|
| Universe | Spot `markets_details` + ticker, USDT and INR futures `active_instruments` every 60 s; listing/delisting and status/exit-only change events | live, verified |
| Instrument metadata | `data/instrument` per pair and margin (INR contracts first, 1 req/s), mapped into the qe-risk capability model: leverage per side, dynamic leverage tiers, min notional/quantity, increments, fees, order types. Records that fail to decode are rejected with the reason and retried after 10 min | live, verified |
| Market data | s2-ws public Socket.IO stream: `currentPrices@futures@rt` sampled at 1 Hz; sequence-checked Live order books (depth 20) and trades for the top 6 candidates and open positions. REST RT poll and REST depth 20 take over when the stream is stale | live |
| Scanner | All tracked futures ranked every second (returns, σ, z-scores, basis vs spot, anomalies); trade-flow imbalance from WS trades; 1 s candles from trades | live |
| Multi-timeframe | Exchange candles 1m, 5m, 15m, 1h, 1d and 3d per candidate (completed candles only); 2d aggregated from two completed 1d; per-timeframe trend z, σ, range ratio, volume z | live |
| Regimes | Explicit numeric rules: trend/range, high/low volatility, expansion/contraction, momentum acceleration/exhaustion, potential reversal, abnormal volume, liquidity withdrawal, unstable | live |
| Forecast | μ = 0.2·r₆₀/60 + 0.05·OBI₁₀·σ₁ₛ; horizons 1–120 s (E[r], P(up), σ_h, 90 % band); first-passage lattice P(TP first), P(SL first), P(neither), expected hit times; TP/SL = k·σ₁₂₀ (k_t 1.5, k_s 1.0) | live, **uncalibrated priors** |
| Costs / EV | EV = P(TP)·W − P(SL)·L + P(neither)·E[R\|neither] − C, with C = 2 × live taker fee + spread + 2 × paper slippage | live |
| Calibration | Forecast authority comes from the s2-calib walk-forward registry (`crates/qe-research/calibration.json`, model `desk.fp.v1`, targets `tp_first_{long,short}`; `QE_CALIBRATION_FILE` overrides; reloaded every 10 s). A missing file means no authority. A live forward monitor resolves each P(TP first) on the next 120 s of 1 s prices and revokes authority when, at n ≥ 200, ECE > 0.05 or Brier ≥ climatology. With authority, the registry's recalibrated probability drives EV | live; **no model calibrated** |
| Risk governor | `qe_risk::RiskGovernor` evaluates every candidate: kill/pause, session loss, high-water-mark floor, exit-only, data integrity, calibration, net edge, sizing from the stop-loss budget, highest exchange leverage stepped down for the liquidation buffer, min notional/quantity (rejected, never raised), margin | live |
| Paper execution | `qe_risk::Position` lifecycle with an independent 250 ms server-side timer: taker fills at the touch + slippage + live fee, simulated protection, exits on SL/TP/edge-negative/data integrity/time (117 s lead, 120 s max), reconcile on ack timeout, flatten, kill | unit-tested; live-wired |
| Capital / target | 24 Nov campaign schedule and controller (see [docs/CAMPAIGN_24NOV.md](docs/CAMPAIGN_24NOV.md)): reporting only in the default protective profile (INR 1/trade, INR 5 total); the owner-selected bold-play profile `configs/campaign-live.toml` sizes each bet toward the next hourly milestone above an owner floor; floor and loss-budget stop; no entries before 01:00 IST, exit-only at 23:00 IST. The older Growth → checkpoint → withdrawal → protective stage plan is historical. Trade-frequency scenarios and a seeded Monte-Carlo target simulation | paper ledger |
| Audit | JSONL: decisions (full numeric explanation), governor, fills, closed trades, equity, incidents, audit | live |

## Crates

| Crate | Contents |
|---|---|
| `qe-coindcx` | Public REST client (8 req/s per client and 12 req/s per process, adaptive slow-down after HTTP 429; circuit breaker that ignores decode/4xx responses and reports every open/close, which the desk logs as `rest_breaker_open`/`rest_breaker_close` incidents), wire types, HMAC signing, and (s2-live) the signed private futures client behind `LiveGate` plus the owner's `live_readiness` command. The desk does not use the private client |
| `qe-math` | Deterministic statistics, microstructure, first-passage lattice, bracket EV, Brier/log-loss/ECE, sizing, target controller and simulation, timeframe aggregation, features |
| `qe-risk` | Risk governor, policies with hard ceilings, sizing, leverage selection, 120 s lifecycle state machine, paper fills, capital stages |
| `qe-desk` | The live binary: loops, pipeline (`desk.rs`), JSONL store, HTTP/WebSocket API |
| `qe-research` | (s2-calib) recorder, historical loader, walk-forward evaluator and the calibration registry the desk gates on |
| `qe-engine` | Reserved (empty) |

## Known limits (honest)

- Forecast parameters are priors. The committed s2-calib registry calibrates no model (`desk.fp.v1`: ECE 0.080, Brier skill 0.005), so the governor rejects every entry ("forecast uncalibrated"). Release run results: [docs/RELEASE_REPORT_SPRINT2.md](docs/RELEASE_REPORT_SPRINT2.md).
- At INR 100 equity, the INR 1 per-trade loss cap is below the risk at minimum order size for most contracts: at a ~INR 625 minimum notional, INR 1 is 0.16 % of notional for costs and stop together, against ~0.10–0.16 % round-trip cost. The governor rejects these trades rather than raising the budget.
- Gamble mode skips the calibration and positive-EV gates, so it is a paper/research profile and is not part of the campaign's live path ([docs/GAMBLE_MODE.md](docs/GAMBLE_MODE.md)).
- Live trading is blocked: no authenticated path has run against the real account, and no model has calibration authority.
- Paper TP/SL is enforced by the server-side monitor; exchange-side protective orders are not used in paper.
- INR-margin conversion uses the spot USDTINR mid, which approximates the exchange's own conversion rate.
