// Mocked `/api/state.campaign` for dev only: open the dev server with `?mock=campaign` (or `?mock=campaign-stopped`)
// to render the campaign panel without a running desk. Production builds never use it (import.meta.env.DEV guard),
// and every figure here is labelled PAPER: it is a layout fixture, not campaign data.
import type { Campaign, CampaignMilestone } from "../api";
import { requiredEquity } from "../campaign-math";

const START_MS = Date.parse("2026-11-23T18:30:00Z"); // 24 Nov 2026 00:00 IST
const H = 3_600_000;
const iso = (ms: number) => new Date(ms).toISOString();
const req = (h: number) => requiredEquity(h).toFixed(2);

// Fixture "now" is about 03:20 IST; paper equity samples a few minutes apart.
const HISTORY: [number, number][] = [
  [0, 100], [0.2, 100.4], [0.5, 99.6], [0.8, 101.2], [1, 101.9], [1.4, 100.8], [1.9, 102.5], [2, 102.1], [2.6, 103.4], [3, 102.9], [3.33, 103.6],
];

const milestones: CampaignMilestone[] = Array.from({ length: 23 }, (_, h) => {
  const s = HISTORY.find(([t]) => t === h);
  return { hour: h, at: iso(START_MS + h * H), required_inr: req(h), actual_inr: s ? s[1].toFixed(2) : null };
});

export const CAMPAIGN_FIXTURE: Campaign = {
  start: iso(START_MS),
  deadline: iso(START_MS + 22 * H),
  start_equity_inr: "100",
  target_inr: "50000",
  equity_inr: "103.60",
  equity_source: "paper",
  equity_currency_note: "INR paper wallet (simulated; no conversion)",
  required_now_inr: req(3.33),
  delta_inr: (103.6 - requiredEquity(3.33)).toFixed(2),
  prev_milestone: { hour: 3, at: iso(START_MS + 3 * H), required_inr: req(3) },
  next_milestone: { hour: 4, at: iso(START_MS + 4 * H), required_inr: req(4) },
  milestones,
  realised_pnl_inr: "3.85",
  unrealised_pnl_inr: "-0.25",
  remaining_inr: (50000 - 103.6).toFixed(2),
  remaining_secs: Math.round((22 - 3.33) * 3600),
  drawdown_inr: "0.60",
  required_hourly_rate: Math.pow(50000 / 103.6, 1 / (22 - 3.33)) - 1,
  checkpoints_done: 3,
  status: "behind",
  floor_inr: "95",
  loss_budget_inr: "5",
  loss_used_inr: "0.60",
  per_trade_max_loss_inr: "1",
  risk_budget_now_inr: "1",
  open_position: { pair: "B-SOL_USDT", side: "long", held_secs: 41, unrealised_inr: "-0.25" },
  stop_reason: null,
  exit_only: false,
  equity_history: HISTORY.map(([h, v]) => ({ t: iso(START_MS + h * H), equity_inr: v.toFixed(2) })),
};

export const CAMPAIGN_STOPPED_FIXTURE: Campaign = {
  ...CAMPAIGN_FIXTURE,
  equity_inr: "95.00",
  delta_inr: (95 - requiredEquity(3.33)).toFixed(2),
  remaining_inr: "49905.00",
  drawdown_inr: "8.60",
  loss_used_inr: "5.00",
  risk_budget_now_inr: "0",
  required_hourly_rate: Math.pow(50000 / 95, 1 / (22 - 3.33)) - 1,
  status: "stopped",
  stop_reason: "campaign loss budget ₹5 exhausted (equity reached the ₹95 floor)",
  exit_only: true,
  open_position: null,
  unrealised_pnl_inr: "0",
};

/** The dev-only fixture selected by `?mock=…`, or undefined to use the desk's own value. */
export function mockCampaign(): Campaign | undefined {
  if (!import.meta.env.DEV) return undefined;
  const m = new URLSearchParams(window.location.search).get("mock");
  if (m === "campaign") return CAMPAIGN_FIXTURE;
  if (m === "campaign-stopped") return CAMPAIGN_STOPPED_FIXTURE;
  return undefined;
}
