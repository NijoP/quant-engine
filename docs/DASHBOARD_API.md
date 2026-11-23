# Desk HTTP / WebSocket API (contract v1)

Served by the `qe-desk` binary on `http://127.0.0.1:8787` (override with `QE_DESK_ADDR`).
All responses are JSON, `snake_case`, times in **milliseconds since Unix epoch** (`*_ms`)
or seconds where the field name ends in `_s`. Any numeric field may be `null` when the
value is not yet computable (insufficient data); the UI must render `null` as "—".
The desk serves `dashboard/dist` at `/` when it exists.

Execution mode in this sprint is always `"paper"`. Live order submission is compiled and
configured OFF; no endpoint can enable it.

## GET endpoints

| Path | Returns |
|---|---|
| `/api/state` | Aggregate snapshot used by the dashboard poll loop (shape below). |
| `/api/status` | `state.health` + `state.mode` + `state.live_orders_enabled` + readiness flags. |
| `/api/universe` | `{ "spot": [SpotMarket], "futures": [FuturesInstrument], "events": [UniverseEvent] }` |
| `/api/scanner` | `[ScannerRow]` ranked by `score` (all tracked futures). |
| `/api/opportunities` | `[Opportunity]` newest first. |
| `/api/instrument/{pair}?margin=USDT\|INR` | `{ "instrument": FuturesInstrument, "features": ScannerRow, "forecast": Forecast\|null, "book": Book\|null }` |
| `/api/instrument/{pair}/candles?resolution=1\|5\|15\|60\|1D&margin=USDT` | `[Candle]` (proxied + cached from CoinDCX candlesticks, completed candles only, `complete` flag). |
| `/api/forecasts` | `[Forecast]` for current candidates. |
| `/api/decisions` | `[Decision]` (every approve/reject with full numeric explanation). |
| `/api/audit?limit=N` | `[AuditEvent]` newest first. |
| `/api/positions` | `{ "open": [Position], "closed": [ClosedTrade], "fills": [Fill] }` |
| `/api/capital` | `Account` + `Target` |
| `/api/agents` | `[AgentStatus]` |
| `/api/risk` | `RiskParams` (current mode's hard ceilings and live governor state) |

## POST controls (all return `{ "ok": bool, "message": string, "state": ControlsState }`)

| Path | Effect |
|---|---|
| `/api/control/scanner` body `{ "running": bool }` | start/stop scanner loops |
| `/api/control/paper` body `{ "running": bool }` | start/stop paper trading (entries) |
| `/api/control/strategy` body `{ "enabled": bool }` | enable/disable strategy evaluation |
| `/api/control/pause_entries` body `{ "paused": bool }` | pause/resume new entries |
| `/api/control/cancel_entries` | cancel outstanding (paper) entry orders |
| `/api/control/flatten` | emergency flatten all paper positions at market |
| `/api/control/kill` | kill switch: flatten + disable entries until restart |

## WebSocket

`GET /ws` upgrades to a WebSocket that pushes `{ "type": "state", "data": State }` once per
second (same shape as `/api/state`). Clients may fall back to polling `/api/state`.

## Shapes

```jsonc
State = {
  "server_time_ms": number, "mode": "paper", "live_orders_enabled": false,
  "health": { "started_ms": number, "last_universe_ms": number|null, "last_rt_ms": number|null,
              "last_spot_ticker_ms": number|null, "rt_polls": number, "errors": number,
              "last_error": string|null, "stale": boolean },
  "universe": { "futures_active": number, "futures_usdt": number, "futures_inr": number,
                "futures_tracked": number, "spot_active": number, "spot_with_ticker": number,
                "refreshed_ms": number|null },
  "scanner": [ScannerRow],          // top 60
  "opportunities": [Opportunity],   // newest first, last 100
  "positions": [Position],
  "fills": [Fill], "closed": [ClosedTrade],
  "account": Account, "target": Target, "agents": [AgentStatus], "controls": ControlsState,
  "audit": [AuditEvent]             // newest first, last 200
}
ScannerRow = { "pair": string, "margin": "USDT"|"INR", "last": number, "mark": number|null,
  "funding_rate": number|null, "change_24h_pct": number|null, "volume_24h": number|null,
  "ret_10s": number|null, "ret_30s": number|null, "ret_60s": number|null, "ret_300s": number|null,
  "vol_1s": number|null, "vol_1m_candle": number|null, "z_ret_10s": number|null, "z_volume": number|null,
  "basis_bps": number|null, "obi": number|null, "spread_bps": number|null,
  "anomaly": boolean, "anomaly_reasons": [string], "regime": string|null, "score": number }
Opportunity = { "pair": string, "side": "long"|"short", "p_win": number, "p_up_horizon": number,
  "p_stop": number, "p_neither": number, "tp_pct": number, "sl_pct": number,
  "ev_gross_bps": number, "cost_bps": number, "ev_net_bps": number, "horizon_s": number,
  "qty": number, "notional": number, "leverage": number, "decision": "taken"|"rejected",
  "reason": string, "calibrated": boolean, "ts_ms": number }
Forecast = { "pair": string, "ts_ms": number, "mu_per_s": number, "sigma_per_s": number,
  "horizons": [ { "h_s": number, "exp_ret": number, "p_up": number, "p_down": number,
                  "sigma_h": number } ],
  "first_passage": { "p_target": number, "p_stop": number, "p_neither": number,
                     "e_t_target": number, "e_t_stop": number }, "model": string, "calibrated": boolean }
Position = { "id": string, "pair": string, "side": "long"|"short", "qty": number,
  "entry_price": number, "mark_price": number, "tp_price": number, "sl_price": number,
  "leverage": number, "notional": number, "opened_ms": number, "age_s": number,
  "time_left_s": number, "unrealized_pnl": number, "fees_paid": number, "state": string }
Fill = { "id": string, "position_id": string, "pair": string, "side": "buy"|"sell", "qty": number,
  "price": number, "ref_mid": number, "slippage_bps": number, "fee": number,
  "liquidity": "taker", "reason": "entry"|"tp"|"sl"|"time_stop"|"edge_lost"|"flatten", "ts_ms": number }
ClosedTrade = { "id": string, "pair": string, "side": "long"|"short", "entry_price": number,
  "exit_price": number, "qty": number, "gross_pnl": number, "fees": number, "net_pnl": number,
  "held_s": number, "exit_reason": string, "closed_ms": number }
Account = { "currency": "INR", "paper": true, "starting_equity": number, "equity": number,
  "realized_pnl": number, "unrealized_pnl": number, "fees_paid": number, "day_pnl": number,
  "used_margin": number, "available": number, "trades": number, "wins": number, "losses": number,
  "halted": boolean, "halt_reason": string|null,
  "policy": { "tier": "growth"|"protective", "risk_per_trade_frac": number, "max_leverage": number,
              "max_positions": number, "daily_loss_limit_frac": number, "kelly_fraction": number,
              "max_hold_s": 120 } }
Target = { "stage": 1|2, "start_equity": number, "target_equity": number, "deadline_ms": number,
  "hours_remaining": number, "growth_required": number,
  "scenarios": [ { "trades_per_hour": number, "required_return_per_trade": number } ],
  "actual_trades_per_hour": number|null, "avg_win": number|null, "avg_loss": number|null,
  "expectancy": number|null, "geometric_growth": number|null,
  "simulation": { "p_reach_target": number, "p_session_loss_first": number, "p_floor_breach": number,
                  "p_losing_streak": number, "p_unrecovered_drawdown": number } | null }
AgentStatus = { "id": string, "name": string, "status": "ok"|"degraded"|"stopped"|"error",
  "last_run_ms": number|null, "last_ok_ms": number|null, "duration_ms": number|null,
  "runs": number, "errors": number, "last_output": string|null, "last_error": string|null }
ControlsState = { "scanner_running": boolean, "paper_running": boolean, "strategy_enabled": boolean,
  "entries_paused": boolean, "killed": boolean }
AuditEvent = { "ts_ms": number, "kind": string, "message": string, "data": any }
Candle = { "time": number, "open": number, "high": number, "low": number, "close": number,
  "volume": number, "complete": boolean }
```
