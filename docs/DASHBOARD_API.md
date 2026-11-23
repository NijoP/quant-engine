# Desk HTTP / WebSocket API (contract v1)

Served by the `qe-desk` binary on `http://127.0.0.1:8787` (override with `QE_DESK_ADDR`).
All responses are JSON, `snake_case`, times in **milliseconds since Unix epoch** (`*_ms`)
or seconds where the field name ends in `_s`. Any numeric field may be `null` when the
value is not yet computable (insufficient data); the UI must render `null` as "—".
The desk serves `dashboard/dist` at `/` when it exists.

Execution mode in this sprint is always `"paper"`. The desk holds no private client and never
reads credentials; no endpoint can enable live orders. `state.live` only *reports* the s2-live
gate (`qe_coindcx::live::LiveGate`) and its checklist.

## GET endpoints

| Path | Returns |
|---|---|
| `/api/state` | Aggregate snapshot used by the dashboard poll loop (shape below). |
| `/api/status` | `health`, `mode`, `live_orders_enabled` (always false), `live` (gate status + checklist), `readiness` flags, `calibration`, `controls`. |
| `/api/calibration` | `Calibration` (forward calibration of P(TP before SL within 120 s); gates strategy authority). |
| `/api/universe` | `{ "spot": [SpotMarket], "futures": [FuturesInstrument], "events": [UniverseEvent] }` |
| `/api/scanner` | `[ScannerRow]` ranked by `score` (all tracked futures). |
| `/api/opportunities` | `[Opportunity]` newest first. |
| `/api/instrument/{pair}?margin=USDT\|INR` | `{ "instrument": FuturesInstrument (live metadata), "capability": InstrumentCapability, "eligible": bool\|null, "eligibility": string, "features": ScannerRow, "forecast": Forecast\|null, "book": {ts, vs, bids[10], asks[10], sane}\|null, "mtf": {timeframes: {3d,2d,1d,1h,15m,5m,1m: {completed, ctx}}}\|null, "regime": [string], "decisions": [Decision] }` |
| `/api/instrument/{pair}/candles?resolution=1S\|1\|5\|15\|60\|1D\|2D\|3D` | `[Candle]` (proxied + 15 s cache; `complete` flag; `2D` aggregated from two completed `1D`; `1S` built locally from WS trades of subscribed candidates, with buy/sell volume). |
| `/api/forecasts` | `[Forecast]` for current candidates. |
| `/api/decisions` | `[Decision]` newest first, last 300 (every approve/reject with full numeric explanation). |
| `/api/audit?limit=N` | `[AuditEvent]` newest first. |
| `/api/positions` | `{ "open": [Position], "closed": [ClosedTrade], "fills": [Fill], "incidents": [Incident] }` |
| `/api/capital` | `Account` + `"target": Target` + `"equity_curve": [[ts_ms, equity]]` |
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
| `/api/control/ws_reconnect` | force the public WebSocket to reconnect (REST fallback covers the gap) |

Malformed bodies return `ok: false` with the expected shape in `message`.

## WebSocket

`GET /ws` upgrades to a WebSocket that pushes `{ "type": "state", "data": State }` once per
second (same shape as `/api/state`). Clients may fall back to polling `/api/state`.

## Shapes

```jsonc
State = {
  "server_time_ms": number, "mode": "paper", "live_orders_enabled": false,
  "health": { "started_ms": number, "last_universe_ms": number|null, "last_rt_ms": number|null,
              "last_spot_ticker_ms": number|null, "rt_polls": number, "errors": number,
              "last_error": string|null, "stale": boolean,
              "feed": "ws"|"rest-fallback", "ws_prices_fresh": boolean, "ws_live_books": number,
              "ws": WsHealth (qe_coindcx::ws), "instruments_cached": number, "store_dir": string },
  "universe": { "futures_active": number, "futures_usdt": number, "futures_inr": number,
                "futures_tracked": number, "spot_active": number, "spot_with_ticker": number,
                "refreshed_ms": number|null },
  "scanner": [ScannerRow],          // top 60
  "opportunities": [Opportunity],   // newest first, last 100
  "positions": [Position],
  "fills": [Fill], "closed": [ClosedTrade],
  "account": Account, "target": Target, "agents": [AgentStatus], "controls": ControlsState,
  "audit": [AuditEvent],            // newest first, last 200
  "calibration": Calibration, "readiness": { FLAG: boolean, ... }, "live": LiveStatus,
  "campaign": Campaign              // 24 Nov campaign, see docs/CAMPAIGN_24NOV.md
}
// Money is a decimal string. Milestone required_inr is rounded to the whole rupee; live amounts use 2 dp.
// The schedule is a mathematical requirement, not a forecast; no field here feeds sizing or entries.
Campaign = { "start": iso8601, "deadline": iso8601,          // 2026-11-24T00:00+05:30 / 23:00+05:30 (22:00 superseded)
  "first_entry": iso8601,                                     // 2026-11-24T01:00+05:30: no campaign entries before
  "duration_hours": 23, "entry_window_open": bool,            // window 01:00-23:00 IST; permits, never forces, a trade
  "hourly_multiplier": number,                                // (50000/101.45)^(1/23) = 1.3094071
  "start_equity_inr": string, "target_inr": "50000",
  "equity_inr": string|null,                                  // null when no verified figure exists
  "equity_source": "live_exchange"|"paper",                   // paper is never shown as real capital
  "equity_currency_note": string,                             // currency and any INR/USDT conversion source
  "required_now_inr": string, "delta_inr": string,            // delta = actual - required (negative = behind)
  "prev_milestone": Milestone, "next_milestone": Milestone,
  "milestones": [Milestone],                                  // hours 0..22, actual_inr null until reached
  "realised_pnl_inr": string, "unrealised_pnl_inr": string,   // always reported separately
  "remaining_inr": string, "remaining_secs": number, "drawdown_inr": string,
  "required_hourly_rate": number|null,                        // (50000/E)^(1/(22-h)) - 1, display only
  "checkpoints_done": number,
  "status": "ahead"|"on_track"|"behind"|"stopped"|"completed"|"expired",
  "floor_inr": "95", "loss_budget_inr": "5", "loss_used_inr": string,
  "per_trade_max_loss_inr": "1", "risk_budget_now_inr": string,
  "open_position": null | { "pair": string, "side": string, "held_secs": number, "unrealised_inr": string },
  "stop_reason": string|null, "exit_only": boolean,
  "equity_history": [ { "t": iso8601, "equity_inr": string } ] }  // verified campaign equity only, never synthesised
Milestone = { "hour": number, "at": iso8601, "required_inr": string, "actual_inr": string|null }
Decision = { "ts_ms", "trace_id", "pair", "side": "Long"|"Short", "decision": "taken"|"rejected",
  "reason": string, "entry", "tp_price", "sl_price", "ev_net_bps", "approved_order": ApprovedOrder|null,
  "instrument": InstrumentCapability|null,
  "explanation": { "model_id", "formulas": {..}, "inputs": {bid, ask, mid, obi_10, spread_bps, r_60,
     sigma_1s, book_age_ms, lambda_mom, gamma_obi, k_stop, k_target, taker_fee, paper_slippage, fx_usdtinr},
     "intermediates": {mu_per_s, sigma_120, tp_ret, sl_ret, cost},
     "sides": [{side, p_target, p_stop, p_neither, e_t_target, e_t_stop, e_ret_neither, ev_gross, ev_net}],
     "regime": [string], "data_ok": bool, "calibrated": bool } }
Calibration = { "model", "target", "n_resolved", "n_pending", "n_dropped_gaps", "brier",
  "brier_climatology", "log_loss", "ece_10bin", "mean_p", "hit_rate", "calibrated": bool,
  "verdict": string, "criteria": string, "method": string }
LiveStatus = { "gate": { compiled, execution_mode, operator_confirmed, credentials_configured,
  enabled, blocked_by, readiness: { items: { ITEM: { status: "pass"|"fail"|"not_run", detail } } } },
  "desk_live_path": string, "owner_command": string }
```

## Audit store

Append-only JSONL under `data/desk/` (gitignored; override `QE_DATA_DIR`): `decisions.jsonl`,
`governor.jsonl` (raw qe-risk audit records), `fills.jsonl`, `closed.jsonl`, `equity.jsonl`
(per trade + 10 s heartbeat), `incidents.jsonl`, `audit.jsonl`. Lines are never rewritten.

```jsonc
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
// Target: older stage-plan view; its deadline comes from configs/risk-limits.toml. Use Campaign for the 24 Nov campaign.
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
