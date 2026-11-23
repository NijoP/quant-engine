// GAMBLE MODE banner and panel: display only, driven by `/api/state.gamble`.
// There are no controls here on purpose: the owner arms and configures gamble mode on the desk itself.
import type { Gamble } from "./api";

const DASH = "—";

function n(v: string | number | null | undefined): number | null {
  if (v === null || v === undefined) return null;
  const x = typeof v === "number" ? v : parseFloat(v);
  return Number.isFinite(x) ? x : null;
}
function inr(v: string | number | null | undefined, signed = false): string {
  const x = n(v);
  if (x === null) return DASH;
  const s = "₹" + Math.abs(x).toLocaleString("en-IN", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
  if (x < 0) return "−" + s;
  return signed && x > 0 ? "+" + s : s;
}
function pct(v: number | null | undefined, d = 2): string {
  return typeof v === "number" && Number.isFinite(v) ? (v * 100).toFixed(d) + "%" : DASH;
}
function countdown(ms: number): string {
  if (!Number.isFinite(ms)) return DASH;
  if (ms <= 0) return "deadline passed";
  const s = Math.floor(ms / 1000);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return `${h}h ${String(m).padStart(2, "0")}m ${String(s % 60).padStart(2, "0")}s`;
}
function policyLabel(g: Gamble): string {
  return g.policy === "bold_play" ? "bold play (stake sized so one win reaches target)" : `fixed stake ${inr(g.stake_inr)} per bet`;
}

export function GambleBanner({ gamble }: { gamble: Gamble | null | undefined }) {
  if (!gamble) return null;
  const mode = gamble.paper ? "PAPER" : "LIVE";
  const cls = "gamble-banner " + (gamble.stopped_reason ? "stopped" : gamble.enabled ? (gamble.paper ? "paper" : "live") : "off");
  return (
    <div className={cls} role="alert">
      GAMBLE MODE {gamble.enabled ? "ARMED" : "OFF"} · {mode} · PAPER / RESEARCH ONLY — not the campaign's live path
      {gamble.stopped_reason ? ` · STOPPED: ${gamble.stopped_reason}` : ""}
      <span className="rules">
        {policyLabel(gamble)} · leverage {gamble.leverage}x · stops permanently at floor {inr(gamble.floor_inr)} · forecast gate bypassed — bets are not
        backed by a measured edge
      </span>
    </div>
  );
}

export function GamblePanel({ gamble, serverNow }: { gamble: Gamble | null | undefined; serverNow: number }) {
  if (!gamble)
    return (
      <section className="panel">
        <h2>Gamble mode (paper / research only)</h2>
        <div className="empty">Gamble mode off</div>
      </section>
    );
  const g = gamble;
  const equity = n(g.equity_inr);
  const target = n(g.target_inr);
  const floor = n(g.floor_inr);
  const net = n(g.net_pnl_inr);
  const dist = n(g.distance_to_floor_inr);
  const deadlineMs = Date.parse(g.deadline);
  // Progress from floor (0%) to target (100%): the only two places gamble mode ends.
  const progress = equity !== null && target !== null && floor !== null && target > floor ? Math.min(1, Math.max(0, (equity - floor) / (target - floor))) : null;
  const decided = g.wins + g.losses;
  // 95% normal-approximation band on the realised win rate, so a small sample is not read as skill.
  const band = g.win_rate !== null && decided > 0 ? 1.96 * Math.sqrt((g.win_rate * (1 - g.win_rate)) / decided) : null;
  const multiple = equity !== null && target !== null && equity > 0 ? target / equity : null;
  const doublings = multiple !== null && multiple > 1 ? Math.log2(multiple) : null;
  return (
    <section className="panel gamble">
      <h2>
        Gamble mode (paper / research only) — {g.enabled ? "armed" : "off"} · <span className={"badge " + (g.paper ? "mode" : "danger")}>{g.paper ? "PAPER" : "LIVE"}</span>
        {g.stopped_reason && <span className="badge danger"> STOPPED: {g.stopped_reason}</span>}
      </h2>
      <div className="gamble-progress" title="floor → target">
        <span>floor {inr(g.floor_inr)}</span>
        <div className="bar">
          <div className="fill" style={{ width: `${(progress ?? 0) * 100}%` }} />
        </div>
        <span>target {inr(g.target_inr)}</span>
      </div>
      <div className="kv cols">
        <span>policy</span><b>{policyLabel(g)}</b>
        <span>stake / leverage</span><b>{inr(g.stake_inr)} · {g.leverage}x</b>
        <span>equity (actual)</span><b>{inr(g.equity_inr)}</b>
        <span>target</span><b>{inr(g.target_inr)}{multiple !== null ? ` (${multiple.toFixed(1)}× away${doublings !== null ? `, ≈${doublings.toFixed(1)} doublings` : ""})` : ""}</b>
        <span>deadline</span><b>{Number.isFinite(deadlineMs) ? new Date(deadlineMs).toLocaleString() : g.deadline}</b>
        <span>time left</span><b>{countdown(deadlineMs - serverNow)}</b>
        <span>floor</span><b>{inr(g.floor_inr)}</b>
        <span>distance to floor</span><b className={dist !== null && dist <= 0 ? "neg" : ""}>{inr(g.distance_to_floor_inr)}</b>
        <span>bets</span><b>{g.trades} · {g.wins} won / {g.losses} lost</b>
        <span>realised win rate</span>
        <b>{g.win_rate === null ? "no settled bets yet" : `${pct(g.win_rate, 1)}${band !== null ? ` ± ${pct(band, 1)} (95%, n=${decided})` : ""}`}</b>
        <span>net P&L after costs</span><b className={net === null || net === 0 ? "" : net > 0 ? "pos" : "neg"}>{inr(g.net_pnl_inr, true)}</b>
        <span>P(reach target)</span><b>{pct(g.p_target)}</b>
        <span>P(hit floor)</span><b className="neg">{pct(g.p_floor)}</b>
      </div>
      <p className="gamble-note">
        Honest odds: each bet pays about 13–16 bps of notional in round-trip costs, and no forecast edge has been measured, so the expected value of
        every bet is negative — roughly minus the costs. P(reach target) and P(hit floor) are Monte Carlo estimates from the realised win rate and the
        TP/SL payoff; they are not a promise. The most likely outcome is that the balance reaches the floor and gamble mode stops for good.
      </p>
    </section>
  );
}
