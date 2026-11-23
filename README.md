# QUANT-ENGINE

CoinDCX-first quantitative trading intelligence desk: Rust core and a React + TypeScript dashboard.
**Execution mode: paper only.** The desk never reads API credentials and holds no private client;
live order submission is refused (see `qe_coindcx::live::LiveGate`, which only the account owner
can satisfy, and which still requires checks that have not been run).

Paper equity is a configured value (INR 100 by default) and is labelled PAPER everywhere. It is
not a CoinDCX balance.

## Run

```bash
cargo build && cargo test
(cd dashboard && npm ci && npm run build)
./target/debug/qe-desk          # http://127.0.0.1:8787  (dashboard at /, JSON API at /api/*)
```

`QE_DESK_ADDR` overrides the listen address; `QE_DATA_DIR` overrides the append-only audit
store (default `data/desk/`, gitignored). API reference: [docs/DASHBOARD_API.md](docs/DASHBOARD_API.md).

## Pipeline (what actually runs in `qe-desk`)

DATA → MATHEMATICS → PROBABILISTIC FORECAST → STRATEGY → RISK GOVERNOR → (paper) EXECUTION → FEEDBACK

| Stage | Implementation | Status |
|---|---|---|
| Universe | Spot `markets_details` + ticker, USDT and INR futures `active_instruments` every 60 s; listing/delisting and status/exit-only change events | live, verified |
| Instrument metadata | `data/instrument` per pair and margin (INR contracts first, 1 req/s), mapped into the qe-risk capability model: leverage per side, dynamic leverage tiers, min notional/quantity, increments, fees, order types. Records that fail to decode are rejected with the reason and retried after 10 min | live, verified |
| Scanner | All tracked futures ranked every second from the RT price endpoint (returns, σ, z-scores, basis vs spot, anomalies) | live, REST-polled |
| Order books | REST depth 20 for the top 6 candidates every 3 s, and every 0.5 s for open paper positions | live, REST-polled |
| Multi-timeframe | Exchange candles 1m, 5m, 15m, 1h, 1d and 3d per candidate (completed candles only); 2d aggregated from two completed 1d; per-timeframe trend z, σ, range ratio, volume z | live |
| Regimes | Explicit numeric rules: trend/range, high/low volatility, expansion/contraction, momentum acceleration/exhaustion, potential reversal, abnormal volume, liquidity withdrawal, unstable | live |
| Forecast | μ = 0.2·r₆₀/60 + 0.05·OBI₁₀·σ₁ₛ; horizons 1–120 s (E[r], P(up), σ_h, 90 % band); first-passage lattice P(TP first), P(SL first), P(neither), expected hit times; TP/SL = k·σ₁₂₀ (k_t 1.5, k_s 1.0) | live, **uncalibrated priors** |
| Costs / EV | EV = P(TP)·W − P(SL)·L + P(neither)·E[R\|neither] − C, with C = 2 × live taker fee + spread + 2 × paper slippage | live |
| Calibration | Every candidate's P(TP first) is resolved forward on the next 120 s of 1 s prices. It counts as calibrated only when n ≥ 200, ECE ≤ 0.05 and Brier beats climatology. Uncalibrated forecasts are denied strategy authority | live |
| Risk governor | `qe_risk::RiskGovernor` evaluates every candidate: kill/pause, session loss, high-water-mark floor, exit-only, data integrity, calibration, net edge, sizing from the stop-loss budget, highest exchange leverage stepped down for the liquidation buffer, min notional/quantity (rejected, never raised), margin | live |
| Paper execution | `qe_risk::Position` lifecycle with an independent 250 ms server-side timer: taker fills at the touch + slippage + live fee, simulated protection, exits on SL/TP/edge-negative/data integrity/time (117 s lead, 120 s max), reconcile on ack timeout, flatten, kill | unit-tested; live-wired |
| Capital / target | Growth → checkpoint (22:00 IST: pause + flatten) → user-confirmed withdrawal → protective. Trade-frequency scenarios and a seeded Monte-Carlo target simulation | live, paper ledger |
| Audit | JSONL: decisions (full numeric explanation), governor, fills, closed trades, equity, incidents, audit | live |

## Crates

| Crate | Contents |
|---|---|
| `qe-coindcx` | Public REST client (8 req/s pacing, HTTP 429 + circuit breaker), wire types, HMAC signing, and (s2-live) the signed private futures client behind `LiveGate` plus the owner's `live_readiness` command. The desk does not use the private client |
| `qe-math` | Deterministic statistics, microstructure, first-passage lattice, bracket EV, Brier/log-loss/ECE, sizing, target controller and simulation, timeframe aggregation, features |
| `qe-risk` | Risk governor, policies with hard ceilings, sizing, leverage selection, 120 s lifecycle state machine, paper fills, capital stages |
| `qe-desk` | The live binary: loops, pipeline (`desk.rs`), JSONL store, HTTP/WebSocket API |
| `qe-engine` | Reserved (empty) |

## Known limits (honest)

- Market data is REST-polled; the WebSocket stream (s2-ws) is not yet wired in.
- Forecast parameters are priors. Until the forward calibration gate passes, the governor rejects every entry ("forecast uncalibrated").
- At INR 100 paper equity, a 1 % risk budget (INR 1) is below the risk at minimum order size for most contracts, so the governor rejects them rather than raising the budget.
- Paper TP/SL is enforced by the server-side monitor; exchange-side protective orders are not used in paper.
- INR-margin conversion uses the spot USDTINR mid, which approximates the exchange's own conversion rate.
