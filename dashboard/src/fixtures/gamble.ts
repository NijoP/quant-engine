// Mocked `/api/state.gamble` for dev only. Open the dev server with `?mock=gamble` (or `?mock=gamble-stopped`)
// to render the gamble panel without a running desk. Production builds never use it.
import type { Gamble } from "../api";

export const GAMBLE_FIXTURE: Gamble = {
  enabled: true,
  policy: "fixed",
  stake_inr: "100",
  leverage: "20",
  floor_inr: "40",
  equity_inr: "1180.50",
  target_inr: "50000",
  deadline: "2026-11-24T11:30:00Z",
  trades: 23,
  wins: 9,
  losses: 13,
  win_rate: 9 / 22,
  net_pnl_inr: "-319.50",
  distance_to_floor_inr: "1140.50",
  p_target: 0.0014,
  p_floor: 0.962,
  stopped_reason: null,
  paper: true,
};

export const GAMBLE_STOPPED_FIXTURE: Gamble = {
  ...GAMBLE_FIXTURE,
  enabled: false,
  equity_inr: "38.20",
  trades: 41,
  wins: 14,
  losses: 27,
  win_rate: 14 / 41,
  net_pnl_inr: "-1461.80",
  distance_to_floor_inr: "-1.80",
  p_target: 0,
  p_floor: 1,
  stopped_reason: "balance floor reached",
};

/** The dev-only fixture selected by `?mock=…`, or undefined to use the desk's own value. */
export function mockGamble(): Gamble | undefined {
  if (!import.meta.env.DEV) return undefined;
  const m = new URLSearchParams(window.location.search).get("mock");
  if (m === "gamble") return GAMBLE_FIXTURE;
  if (m === "gamble-stopped") return GAMBLE_STOPPED_FIXTURE;
  return undefined;
}
