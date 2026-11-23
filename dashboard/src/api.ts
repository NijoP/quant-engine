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
}

export interface Universe {
  futures_active: number;
  futures_tracked: number;
  spot_active: number;
  spot_with_ticker: number;
  refreshed_ms: number | null;
}

export interface ScannerRow {
  pair: string;
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

export interface DeskState {
  server_time_ms: number;
  mode: string;
  live_orders_enabled: boolean;
  health: Health;
  universe: Universe;
  scanner: ScannerRow[];
  opportunities: Opportunity[];
  positions: Position[];
  fills: Fill[];
  closed: ClosedTrade[];
  account: Account;
  audit: AuditEntry[];
}

export async function fetchState(signal?: AbortSignal): Promise<DeskState> {
  const res = await fetch("/api/state", { cache: "no-store", signal });
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return (await res.json()) as DeskState;
}
