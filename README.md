# QUANT-ENGINE

CoinDCX-first quantitative trading intelligence desk: Rust core and a React + TypeScript dashboard.
**Execution mode: paper only. Live order submission is compiled and configured off.**

## Run

```bash
cargo build && cargo test
(cd dashboard && npm ci && npm run build)
./target/debug/qe-desk          # http://127.0.0.1:8787  (dashboard at /, JSON at /api/state)
```

No API key is needed or read. `.env.example` lists variable names only, and `.env` is gitignored.

## Crates

| Crate | Contents |
|---|---|
| `qe-coindcx` | Public REST client (8 req/s pacing, HTTP 429 and 5-failure circuit breaker), wire types checked against live responses, HMAC-SHA256 signing (RFC 4231 test), futures order, leverage and TP/SL request bodies, and `LiveGate`, which refuses submission in every configuration |
| `qe-math` | Deterministic statistics, microstructure (spread, OBI, microprice, book walk), first-passage lattice P(TP first / SL first / neither), bracket EV, Brier, log-loss and ECE, sizing and leverage tiers, target controller, seeded stochastic target simulation, timeframe aggregation |
| `qe-risk` | Risk governor, growth and protective modes, sizing, leverage selection, 120 s lifecycle state machine, paper fills (unit-tested; **not yet wired into the desk**) |
| `qe-desk` | Live binary: universe discovery, futures RT scanner over all instruments, spot ticker basis, order books for the top 8 candidates, first-passage EV evaluation, and `/api/state` |
| `qe-engine` | Placeholder; the orchestration currently lives in `qe-desk` |

## CoinDCX endpoints

| Endpoint | Status |
|---|---|
| `GET api.coindcx.com/exchange/v1/markets_details` | verified live (981 active spot) |
| `GET api.coindcx.com/exchange/ticker` | verified live |
| `GET .../derivatives/futures/data/active_instruments?margin_currency_short_name[]=USDT\|INR` | verified live (505 each) |
| `GET .../derivatives/futures/data/instrument?pair=&margin_currency_short_name=` | verified live; client implemented but not yet called by the desk |
| `GET public.coindcx.com/market_data/v3/current_prices/futures/rt` | verified live (some entries have no `ls`; skipped) |
| `GET public.coindcx.com/market_data/v3/orderbook/{pair}-futures/{10\|20\|50}` | verified live |
| `GET public.coindcx.com/market_data/candlesticks?...&pcode=f` | verified live; client implemented but not used by the desk |
| `GET .../derivatives/futures/data/trades?pair=` | verified live; client implemented but not used |
| `wss://stream.coindcx.com` (socket.io 2.x) | **not implemented**; REST polling instead |
| Authenticated create order / update_leverage / create_tpsl | request bodies built from the docs; **never called**. Timestamp unit (ms per examples vs s per tables) is unverified |
