// 24 NOVEMBER CAMPAIGN section: display only, driven solely by `/api/state.campaign`.
// The hourly schedule is a progress benchmark, never a trading instruction: there are no controls here, and being
// behind schedule is shown in the same neutral style as being ahead (no red, no urgency cues, no "catch up" prompts).
import type { Campaign } from "./api";
import {
  actualPath,
  actualPoints,
  chartGeom,
  duration,
  hourlyMultiplier,
  inr,
  istDateTime,
  istHourMinute,
  logTicks,
  n,
  pct,
  statusLabel,
  targetPath,
  xOf,
  yOf,
} from "./campaign-math";

const DASH = "—";

function EquitySourceBadge({ c }: { c: Campaign }) {
  if (c.equity_source === "live_exchange")
    return <span className="badge live-equity">LIVE EXCHANGE BALANCE</span>;
  return <span className="badge mode">PAPER EQUITY — simulated, not real capital</span>;
}

export function CampaignPanel({ campaign, serverNow }: { campaign: Campaign | null | undefined; serverNow: number }) {
  if (!campaign)
    return (
      <section className="panel campaign">
        <h2>24 November campaign</h2>
        <div className="empty">The desk does not report a campaign yet (no `campaign` in /api/state).</div>
      </section>
    );
  const c = campaign;
  const startMs = Date.parse(c.start);
  const deadlineMs = Date.parse(c.deadline);
  const equity = n(c.equity_inr);
  const required = n(c.required_now_inr);
  const delta = n(c.delta_inr);
  const pos = c.open_position;
  const terminal = c.status === "stopped" || c.status === "completed" || c.status === "expired";
  const mult = hourlyMultiplier(n(c.start_equity_inr) ?? 100, n(c.target_inr) ?? 50000);
  return (
    <section className="panel campaign">
      <h2>
        24 November campaign · {inr(c.start_equity_inr)} → {inr(c.target_inr)} by {istDateTime(deadlineMs)} · <EquitySourceBadge c={c} />{" "}
        <span className={"badge campaign-status" + (terminal ? " terminal" : "")}>{statusLabel(c.status).toUpperCase()}</span>
        {c.exit_only && <span className="badge exit-only">EXIT-ONLY — no new entries</span>}
      </h2>
      {c.stop_reason && (
        <div className="campaign-stop" role="alert">
          Campaign stopped permanently: {c.stop_reason}. No new positions; a new campaign needs explicit owner review.
        </div>
      )}
      <div className="kv cols">
        <span>starting capital</span><b>{inr(c.start_equity_inr)}</b>
        <span>actual equity</span>
        <b>
          {equity === null ? "unverified — not shown" : inr(equity)} <span className="muted">({c.equity_source === "live_exchange" ? "live exchange" : "paper"})</span>
        </b>
        <span>balance currency</span><b className="muted">{c.equity_currency_note || DASH}</b>
        <span>final target</span><b>{inr(c.target_inr)}</b>
        <span>deadline</span><b>{istDateTime(deadlineMs)}</b>
        <span>current time</span><b>{istDateTime(serverNow)}</b>
        <span>hourly milestone</span>
        <b>
          h{c.prev_milestone.hour} {inr(c.prev_milestone.required_inr)} → h{c.next_milestone.hour} {inr(c.next_milestone.required_inr)} at{" "}
          {istHourMinute(Date.parse(c.next_milestone.at))} · {c.checkpoints_done}/22 checkpoints passed
        </b>
        <span>required now</span><b>{inr(c.required_now_inr)}</b>
        <span>actual vs required</span>
        <b>
          {equity === null ? DASH : inr(equity)} vs {inr(required)}
          {equity !== null && required !== null && required > 0 ? ` (${((equity / required) * 100).toFixed(1)}% of schedule)` : ""}
        </b>
        <span>{delta !== null && delta < 0 ? "behind schedule by" : "ahead of schedule by"}</span>
        <b>{delta === null ? DASH : inr(Math.abs(delta))}</b>
        <span>remaining to target</span><b>{inr(c.remaining_inr)}</b>
        <span>remaining time</span><b>{duration(c.remaining_secs)}</b>
        <span>required future rate</span>
        <b>{c.required_hourly_rate === null ? DASH : `${pct(c.required_hourly_rate, 2)} per hour, compounded`}</b>
        <span>schedule rate</span><b>{pct(mult - 1, 4)} per hour (×{mult.toFixed(6)})</b>
        <span>drawdown</span><b>{inr(c.drawdown_inr)}</b>
        <span>campaign floor</span><b>{inr(c.floor_inr)}</b>
        <span>loss budget used</span><b>{inr(c.loss_used_inr)} of {inr(c.loss_budget_inr)}</b>
        <span>risk budget now</span><b>{inr(c.risk_budget_now_inr)} (max planned loss per trade {inr(c.per_trade_max_loss_inr)})</b>
        <span>open position</span>
        <b>
          {pos
            ? `${pos.pair} ${pos.side.toUpperCase()} · held ${pos.held_secs.toFixed(0)}s of 120s max · unrealised ${inr(pos.unrealised_inr, true)}`
            : "none"}
        </b>
        <span>realised P&L</span><b>{inr(c.realised_pnl_inr, true)}</b>
        <span>unrealised P&L</span><b>{inr(c.unrealised_pnl_inr, true)} <span className="muted">(open, not banked)</span></b>
      </div>
      <CampaignChart c={c} startMs={startMs} serverNow={serverNow} />
      <p className="campaign-note">
        The hourly curve is a mathematical requirement, not a trading forecast: 100 × 500^(h/22). A missed checkpoint is a reporting
        condition only. It never raises leverage, position size or trade frequency, never lowers the signal threshold and never bypasses the
        risk governor ({inr(c.per_trade_max_loss_inr)} per trade, {inr(c.loss_budget_inr)} campaign loss, {inr(c.floor_inr)} floor).
      </p>
    </section>
  );
}

function CampaignChart({ c, startMs, serverNow }: { c: Campaign; startMs: number; serverNow: number }) {
  const start = n(c.start_equity_inr) ?? 100;
  const target = n(c.target_inr) ?? 50000;
  const floor = n(c.floor_inr) ?? 95;
  const pts = actualPoints(startMs, c.equity_history);
  const g = chartGeom(floor, target, pts.map((p) => p.v));
  const nowH = (serverNow - startMs) / 3_600_000;
  const actual = actualPath(g, pts);
  const last = pts.length > 0 ? pts[pts.length - 1] : null;
  return (
    <figure className="campaign-chart">
      <svg viewBox={`0 0 ${g.width} ${g.height}`} role="img" aria-label="22-hour campaign progress: mathematical target curve and verified equity">
        {logTicks(g).map((v) => (
          <g key={v}>
            <line className="grid" x1={g.padL} x2={g.width - g.padR} y1={yOf(g, v)} y2={yOf(g, v)} />
            <text className="axis" x={g.padL - 6} y={yOf(g, v) + 3} textAnchor="end">₹{v.toLocaleString("en-IN")}</text>
          </g>
        ))}
        {Array.from({ length: 23 }, (_, h) => h)
          .filter((h) => h % 2 === 0)
          .map((h) => (
            <text key={h} className="axis" x={xOf(g, h)} y={g.height - 8} textAnchor="middle">
              {String(h).padStart(2, "0")}:00
            </text>
          ))}
        <line className="floor" x1={g.padL} x2={g.width - g.padR} y1={yOf(g, floor)} y2={yOf(g, floor)} />
        <text className="axis floor-label" x={g.width - g.padR - 4} y={yOf(g, floor) - 4} textAnchor="end">floor {inr(floor)}</text>
        <line className="final" x1={g.padL} x2={g.width - g.padR} y1={yOf(g, target)} y2={yOf(g, target)} />
        <text className="axis" x={g.padL + 4} y={yOf(g, target) - 4}>final target {inr(target)}</text>
        <path className="target-curve" d={targetPath(g, start, target)} />
        {c.milestones.map((m) => {
          const req = n(m.required_inr);
          const act = n(m.actual_inr);
          return (
            <g key={m.hour}>
              {req !== null && (
                <circle className="checkpoint" cx={xOf(g, m.hour)} cy={yOf(g, req)} r={2.5}>
                  <title>
                    {`h${m.hour} ${istHourMinute(Date.parse(m.at))} required ${inr(req)}${act !== null ? ` · verified ${inr(act)}` : ""}`}
                  </title>
                </circle>
              )}
              {act !== null && act > 0 && <circle className="checkpoint-actual" cx={xOf(g, m.hour)} cy={yOf(g, act)} r={3.5} />}
            </g>
          );
        })}
        {actual && <path className="actual-curve" d={actual} />}
        {last && <circle className="checkpoint-actual" cx={xOf(g, last.h)} cy={yOf(g, last.v)} r={3} />}
        {nowH >= 0 && nowH <= 22 && <line className="now" x1={xOf(g, nowH)} x2={xOf(g, nowH)} y1={g.padT} y2={g.height - g.padB} />}
      </svg>
      <figcaption>
        <span className="lg target">— mathematical requirement, not a trading forecast</span>
        <span className="lg actual">— verified campaign equity ({c.equity_source === "live_exchange" ? "live exchange" : "paper"})</span>
        <span className="lg floor">— {inr(floor)} campaign floor</span>
        <span className="muted">log scale · times IST · {pts.length === 0 ? "no verified equity samples yet" : `${pts.length} verified samples`}</span>
      </figcaption>
    </figure>
  );
}
