// Types for GET /api/state served by the desk binary.
export type Side = "long" | "short";

export interface Health {
  started_ms: number;
  last_universe_ms: number | null;
  last_rt_ms: number | null;
  last_spot_ticker_ms: number | null;
  rt_polls: number;
  errors: number;
  last_error: string | null;
  stale?: boolean;
}

export interface Universe {
  futures_active: number;
  futures_usdt?: number;
  futures_inr?: number;
  futures_tracked: number;
  spot_active: number;
  spot_with_ticker: number;
  refreshed_ms: number | null;
}

export interface ScannerRow {
  pair: string;
  margin?: "USDT" | "INR";
  regime?: string | null;
  last: number;
  mark: number | null;
  funding_rate: number | null;
  change_24h_pct: number | null;
  volume_24h: number | null;
  ret_10s: number | null;
  ret_30s: number | null;
  ret_60s: number | null;
  ret_300s: number | null;
  vol_1s: number | null;
  vol_1m_candle: number | null;
  z_ret_10s: number | null;
  z_volume: number | null;
  basis_bps: number | null;
  obi: number | null;
  spread_bps: number | null;
  anomaly: boolean;
  anomaly_reasons: string[];
  score: number;
}

export interface Opportunity {
  pair: string;
  side: Side;
  p_win: number;
  p_up_horizon: number;
  p_stop?: number;
  p_neither?: number;
  calibrated?: boolean;
  tp_pct: number;
  sl_pct: number;
  ev_gross_bps: number;
  cost_bps: number;
  ev_net_bps: number;
  horizon_s: number;
  qty: number;
  notional: number;
  leverage: number;
  decision: "taken" | "rejected";
  reason: string;
  ts_ms: number;
}

export interface Position {
  id: string;
  pair: string;
  side: Side;
  qty: number;
  entry_price: number;
  mark_price: number;
  tp_price: number;
  sl_price: number;
  leverage: number;
  notional: number;
  opened_ms: number;
  age_s: number;
  time_left_s: number;
  unrealized_pnl: number;
  fees_paid: number;
  state?: string;
}

export interface Fill {
  id: string;
  position_id: string;
  pair: string;
  side: "buy" | "sell";
  qty: number;
  price: number;
  ref_mid: number;
  slippage_bps: number;
  fee: number;
  liquidity: string;
  reason: string;
  ts_ms: number;
}

export interface ClosedTrade {
  id: string;
  pair: string;
  side: Side;
  entry_price: number;
  exit_price: number;
  qty: number;
  gross_pnl: number;
  fees: number;
  net_pnl: number;
  held_s: number;
  exit_reason: string;
  closed_ms: number;
}

export interface Policy {
  tier: string;
  risk_per_trade_frac: number;
  max_leverage: number;
  max_positions: number;
  daily_loss_limit_frac: number;
  kelly_fraction: number;
  max_hold_s: number;
}

export interface Account {
  currency?: string;
  paper?: boolean;
  used_margin?: number;
  available?: number;
  starting_equity: number;
  equity: number;
  realized_pnl: number;
  unrealized_pnl: number;
  fees_paid: number;
  day_pnl: number;
  trades: number;
  wins: number;
  losses: number;
  halted: boolean;
  halt_reason: string | null;
  policy: Policy;
}

export interface AuditEntry {
  ts_ms: number;
  kind: string;
  message: string;
  data: unknown;
}

export interface DeskProfile {
  name: string;
  experiment: boolean;
  banner: string | null;
  rules: string[];
}

export interface DeskState {
  server_time_ms: number;
  mode: string;
  profile?: DeskProfile;
  live_orders_enabled: boolean;
  health: Health;
  universe: Universe;
  scanner: ScannerRow[];
  opportunities: Opportunity[];
  positions: Position[];
  fills: Fill[];
  closed: ClosedTrade[];
  account: Account;
  target?: Target;
  agents?: AgentStatus[];
  controls?: ControlsState;
  audit: AuditEntry[];
  readiness?: Record<string, boolean | string>;
  calibration?: Calibration;
  live?: LiveStatus;
  /** Gamble mode, armed by the owner; null when the desk has it off. Money fields are decimal strings. */
  gamble?: Gamble | null;
  /** 24 November ₹100 → ₹50,000 campaign tracker; absent or null when the desk does not run one. Money fields are decimal strings. */
  campaign?: Campaign | null;
  /** Optional operational-health extras; the dashboard renders each field only if the desk reports it. */
  ops?: OpsHealth | null;
}

export interface OpsHealth {
  /** ms epoch of the last exchange reconciliation of positions and protective orders. */
  last_reconciliation_ms?: number | null;
  last_reconciliation_result?: string | null;
  /** Watchdog status text, e.g. "ok" or a fault description. */
  watchdog?: string | null;
  watchdog_last_ms?: number | null;
}

export interface CampaignMilestone {
  hour: number;
  /** ISO timestamp of the checkpoint. */
  at: string;
  /** Mathematical requirement 101.45 × (50000/101.45)^(hour/23), as the desk rounds it. */
  required_inr: string;
  /** Verified equity recorded at the checkpoint; null if not reached yet or not recorded. */
  actual_inr: string | null;
}

export type CampaignStatus = "ahead" | "on_track" | "behind" | "stopped" | "completed" | "expired";

export interface Campaign {
  start: string;
  /** First instant campaign entries may be evaluated (01:00 IST); older desks omit it. */
  first_entry?: string;
  deadline: string;
  /** Campaign length in hours (23). */
  duration_hours?: number;
  /** True between 01:00 and 23:00 IST (permits evaluation; never forces a trade). */
  entry_window_open?: boolean;
  /** Constant hourly multiplier of the schedule, as the desk computes it. */
  hourly_multiplier?: number;
  start_equity_inr: string;
  target_inr: string;
  /** Verified equity; null when the desk cannot verify it. */
  equity_inr: string | null;
  /** `live_exchange` is the real exchange balance; `paper` is simulated and is never real capital. */
  equity_source: "live_exchange" | "paper";
  /** Which balance currency the figure is in, and the conversion source if INR was combined from USDT. */
  equity_currency_note: string;
  required_now_inr: string;
  delta_inr: string;
  prev_milestone: { hour: number; at: string; required_inr: string };
  next_milestone: { hour: number; at: string; required_inr: string };
  milestones: CampaignMilestone[];
  realised_pnl_inr: string;
  unrealised_pnl_inr: string;
  remaining_inr: string;
  remaining_secs: number;
  drawdown_inr: string;
  /** Compounded hourly rate still needed from the current equity (0.33 = 33%/h); null when undefined. */
  required_hourly_rate: number | null;
  checkpoints_done: number;
  status: CampaignStatus;
  floor_inr: string;
  loss_budget_inr: string;
  loss_used_inr: string;
  per_trade_max_loss_inr: string;
  risk_budget_now_inr: string;
  open_position: null | { pair: string; side: string; held_secs: number; unrealised_inr: string };
  stop_reason: string | null;
  exit_only: boolean;
  equity_history: { t: string; equity_inr: string }[];
}

export interface Gamble {
  enabled: boolean;
  policy: "fixed" | "bold_play";
  stake_inr: string;
  leverage: string;
  floor_inr: string;
  equity_inr: string;
  target_inr: string;
  deadline: string;
  trades: number;
  wins: number;
  losses: number;
  win_rate: number | null;
  net_pnl_inr: string;
  distance_to_floor_inr: string;
  p_target: number | null;
  p_floor: number | null;
  stopped_reason: string | null;
  paper: boolean;
}

export interface Calibration {
  model: string;
  target: string;
  n_resolved: number;
  n_pending: number;
  n_dropped_gaps: number;
  brier: number | null;
  brier_climatology: number | null;
  log_loss: number | null;
  ece_10bin: number | null;
  mean_p: number | null;
  hit_rate: number | null;
  calibrated: boolean;
  verdict: string;
  criteria: string;
  method: string;
}

export interface LiveStatus {
  gate: {
    compiled: boolean;
    execution_mode: string;
    operator_confirmed: boolean;
    credentials_configured: boolean;
    enabled: boolean;
    blocked_by: unknown;
    readiness: { items: Record<string, { status: string; detail: string }> };
  };
  desk_live_path: string;
  owner_command: string;
}

export interface Target {
  stage: number;
  start_equity: number;
  target_equity: number;
  deadline_ms: number;
  hours_remaining: number;
  growth_required: number;
  scenarios: { trades_per_hour: number; required_return_per_trade: number }[];
  actual_trades_per_hour: number | null;
  avg_win: number | null;
  avg_loss: number | null;
  expectancy: number | null;
  geometric_growth: number | null;
  simulation: {
    p_reach_target: number;
    p_session_loss_first: number;
    p_floor_breach: number;
    p_losing_streak: number;
    p_unrecovered_drawdown: number;
  } | null;
}

export interface AgentStatus {
  id: string;
  name: string;
  status: "ok" | "degraded" | "stopped" | "error";
  last_run_ms: number | null;
  last_ok_ms: number | null;
  duration_ms: number | null;
  runs: number;
  errors: number;
  last_output: string | null;
  last_error: string | null;
}

export interface ControlsState {
  scanner_running: boolean;
  paper_running: boolean;
  strategy_enabled: boolean;
  entries_paused: boolean;
  killed: boolean;
}

export interface Forecast {
  pair: string;
  ts_ms: number;
  mu_per_s: number;
  sigma_per_s: number;
  horizons: { h_s: number; exp_ret: number; p_up: number; p_down: number; sigma_h: number }[];
  first_passage: { p_target: number; p_stop: number; p_neither: number; e_t_target: number; e_t_stop: number };
  model: string;
  calibrated: boolean;
}

export interface Candle {
  time: number;
  open: number;
  high: number;
  low: number;
  close: number;
  volume: number;
  complete: boolean;
}

/** Universe rows are passed through as loosely-typed records: the contract does not pin their fields. */
export type UniverseRecord = Record<string, unknown>;
export interface UniverseResp {
  spot: UniverseRecord[];
  futures: UniverseRecord[];
  events: UniverseRecord[];
}

export interface InstrumentResp {
  instrument: UniverseRecord;
  features: ScannerRow | null;
  forecast: Forecast | null;
  book: unknown;
}

export interface ControlResp {
  ok: boolean;
  message: string;
  state: ControlsState;
}

export async function getJson<T>(path: string, signal?: AbortSignal): Promise<T> {
  const res = await fetch(path, { cache: "no-store", signal });
  if (!res.ok) throw new Error(`${path}: HTTP ${res.status}`);
  return (await res.json()) as T;
}

export async function postControl(path: string, body?: unknown): Promise<ControlResp> {
  const res = await fetch(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await res.text();
  let parsed: unknown = null;
  try {
    parsed = JSON.parse(text);
  } catch {
    /* non-JSON body */
  }
  if (!res.ok) {
    const msg = parsed && typeof parsed === "object" && "message" in parsed ? String((parsed as { message: unknown }).message) : text;
    throw new Error(`HTTP ${res.status}${msg ? ": " + msg : ""}`);
  }
  return parsed as ControlResp;
}

export async function fetchState(signal?: AbortSignal): Promise<DeskState> {
  const res = await fetch("/api/state", { cache: "no-store", signal });
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return (await res.json()) as DeskState;
}
