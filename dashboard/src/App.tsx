import { useEffect, useRef, useState } from "react";
import {
  fetchState,
  type AuditEntry,
  type ClosedTrade,
  type DeskState,
  type Fill,
  type Opportunity,
  type Position,
  type ScannerRow,
} from "./api";
import { mockGamble } from "./fixtures/gamble";
import { GambleBanner, GamblePanel } from "./gamble";
import { AgentsPanel, ControlsPanel, DecisionsPanel, GlobalStatus, InstrumentPanel, ReadinessPanel, TargetPanel, UniverseTable } from "./panels";

const POLL_MS = 1000;
const DASH = "—";

type N = number | null | undefined;

function isNum(v: N): v is number {
  return typeof v === "number" && Number.isFinite(v);
}

/** Price with adaptive precision: more decimals for small prices. */
function px(v: N): string {
  if (!isNum(v)) return DASH;
  const a = Math.abs(v);
  const d = a >= 1000 ? 2 : a >= 10 ? 3 : a >= 1 ? 4 : a >= 0.01 ? 5 : a >= 0.0001 ? 7 : 9;
  return v.toFixed(d);
}
function num(v: N, d = 2): string {
  return isNum(v) ? v.toFixed(d) : DASH;
}
function money(v: N): string {
  if (!isNum(v)) return DASH;
  return v.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 });
}
function signedMoney(v: N): string {
  if (!isNum(v)) return DASH;
  return (v > 0 ? "+" : "") + money(v);
}
/** Fraction (0.0123) rendered as percent (1.230%). */
function pct(v: N, d = 3): string {
  return isNum(v) ? (v * 100).toFixed(d) + "%" : DASH;
}
function bps(v: N): string {
  return isNum(v) ? v.toFixed(1) : DASH;
}
function qty(v: N): string {
  if (!isNum(v)) return DASH;
  const a = Math.abs(v);
  return a >= 1000 ? v.toFixed(0) : a >= 1 ? v.toFixed(3) : v.toPrecision(4);
}
function time(ms: N): string {
  if (!isNum(ms)) return DASH;
  const d = new Date(ms);
  return d.toLocaleTimeString(undefined, { hour12: false }) + "." + String(d.getMilliseconds()).padStart(3, "0");
}
function ageS(ms: N, now: number): string {
  if (!isNum(ms)) return DASH;
  return ((now - ms) / 1000).toFixed(1) + "s";
}
function sign(v: N): string {
  if (!isNum(v) || v === 0) return "";
  return v > 0 ? "pos" : "neg";
}
function compactJson(v: unknown): string {
  try {
    return JSON.stringify(v);
  } catch {
    return String(v);
  }
}

export default function App() {
  const [state, setState] = useState<DeskState | null>(null);
  const [lastOkLocal, setLastOkLocal] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState<number>(Date.now());
  const inFlight = useRef(false);
  const [sel, setSel] = useState<{ pair: string; margin: string } | null>(null);
  const select = (pair: string, margin: string) => setSel({ pair, margin });

  useEffect(() => {
    let alive = true;
    const tick = async () => {
      if (inFlight.current) return;
      inFlight.current = true;
      const ctl = new AbortController();
      const t = setTimeout(() => ctl.abort(), 3000);
      try {
        const s = await fetchState(ctl.signal);
        if (!alive) return;
        setState(s);
        setLastOkLocal(Date.now());
        setError(null);
      } catch (e) {
        if (alive) setError(e instanceof Error ? e.message : String(e));
      } finally {
        clearTimeout(t);
        inFlight.current = false;
      }
    };
    tick();
    const poll = setInterval(tick, POLL_MS);
    const clock = setInterval(() => setNow(Date.now()), 200);
    return () => {
      alive = false;
      clearInterval(poll);
      clearInterval(clock);
    };
  }, []);

  // Server-clock "now": server_time_ms advanced by local elapsed since the last good fetch.
  const sinceFetchMs = lastOkLocal ? now - lastOkLocal : 0;
  const serverNow = state ? state.server_time_ms + sinceFetchMs : now;
  const offline = error !== null;
  // Dev-only `?mock=gamble` fixture wins so the panel renders without a desk.
  const gamble = mockGamble() ?? state?.gamble ?? null;

  return (
    <div className="desk">
      {offline && (
        <div className="offline">
          DESK OFFLINE — {lastOkLocal ? `last update ${((now - lastOkLocal) / 1000).toFixed(0)}s ago` : "no data received yet"} ({error})
        </div>
      )}
      <Header state={state} serverNow={serverNow} />
      <GambleBanner gamble={gamble} />
      {(state || gamble) && <GamblePanel gamble={gamble} serverNow={serverNow} />}
      {state ? (
        <>
          <GlobalStatus state={state} serverNow={serverNow} offline={offline} />
          <ReadinessPanel state={state} />
          <ControlsPanel controls={state.controls} offline={offline} />
          <AccountStrip state={state} />
          <div className="grid2">
            <PositionsPanel positions={state.positions} sinceFetchMs={sinceFetchMs} maxHold={state.account.policy.max_hold_s} />
            <UniversePanel state={state} serverNow={serverNow} />
          </div>
          <TargetPanel state={state} />
          <OpportunitiesPanel rows={state.opportunities} />
          <ScannerPanel rows={state.scanner} onSelect={select} />
          <InstrumentPanel sel={sel} />
          <DecisionsPanel />
          <AgentsPanel agents={state.agents} serverNow={serverNow} />
          <UniverseTable onSelect={select} />
          <div className="grid2">
            <FillsPanel rows={state.fills} />
            <ClosedPanel rows={state.closed} />
          </div>
          <AuditPanel rows={state.audit} />
        </>
      ) : (
        <div className="panel empty">Waiting for the desk at /api/state …</div>
      )}
    </div>
  );
}

function Header({ state, serverNow }: { state: DeskState | null; serverNow: number }) {
  const h = state?.health;
  const rtAge = h && isNum(h.last_rt_ms) ? (serverNow - h.last_rt_ms) / 1000 : null;
  const exp = state?.profile?.experiment ? state.profile : null;
  return (
    <>
    {exp && (
      <div className="experiment-banner" role="alert" title={exp.rules.join(" · ")}>
        {exp.banner ?? "PAPER EXPERIMENT - UNGATED, MAX LEVERAGE"}
        <span className="rules">{exp.rules.join(" · ")}</span>
      </div>
    )}
    {state && (
      <div className={"experiment-banner"} role="status" style={{ background: state.live_orders_enabled ? "#b91c1c" : "#1d4ed8" }}>
        {state.live_orders_enabled ? "LIVE — real orders enabled by the owner" : "PAPER — simulated orders only; paper P&L is not real equity"}
      </div>
    )}
    <header className="header">
      <h1>QUANT-ENGINE desk</h1>
      {state && <span className="badge mode">{state.mode.toUpperCase()}</span>}
      {state && (
        <span className={"badge " + (state.live_orders_enabled ? "danger" : "safe")}>
          LIVE ORDERS: {state.live_orders_enabled ? "ENABLED" : "DISABLED"}
        </span>
      )}
      {h && (
        <span className="health">
          <span className={rtAge !== null && rtAge < 5 ? "ok" : "warn"}>
            rt poll {rtAge === null ? DASH : rtAge.toFixed(1) + "s ago"}
          </span>
          <span>polls {h.rt_polls}</span>
          <span className={h.errors > 0 ? "warn" : ""}>errors {h.errors}</span>
          <span>uptime {((serverNow - h.started_ms) / 1000).toFixed(0)}s</span>
          {h.last_error && (
            <span className="err" title={h.last_error}>
              last error: {h.last_error}
            </span>
          )}
        </span>
      )}
    </header>
    </>
  );
}

function Stat({ label, value, cls }: { label: string; value: string; cls?: string }) {
  return (
    <div className="stat">
      <div className="label">{label}</div>
      <div className={"value " + (cls ?? "")}>{value}</div>
    </div>
  );
}

function AccountStrip({ state }: { state: DeskState }) {
  const a = state.account;
  const p = a.policy;
  return (
    <section className="panel strip">
      <Stat label="Equity" value={money(a.equity)} />
      <Stat label="Start equity" value={money(a.starting_equity)} />
      <Stat label="Realized" value={signedMoney(a.realized_pnl)} cls={sign(a.realized_pnl)} />
      <Stat label="Unrealized" value={signedMoney(a.unrealized_pnl)} cls={sign(a.unrealized_pnl)} />
      <Stat label="Fees" value={money(a.fees_paid)} />
      <Stat label="Day P&L" value={signedMoney(a.day_pnl)} cls={sign(a.day_pnl)} />
      <Stat label="Trades W/L" value={`${a.trades} · ${a.wins}/${a.losses}`} />
      <Stat
        label="Risk state"
        value={a.halted ? `HALTED${a.halt_reason ? ": " + a.halt_reason : ""}` : "ACTIVE"}
        cls={a.halted ? "neg" : "pos"}
      />
      <div className="policy">
        <span>tier <b>{p.tier}</b></span>
        <span>risk/trade <b>{pct(p.risk_per_trade_frac, 2)}</b></span>
        <span>max lev <b>{num(p.max_leverage, 1)}x</b></span>
        <span>max pos <b>{p.max_positions}</b></span>
        <span>daily loss lim <b>{pct(p.daily_loss_limit_frac, 2)}</b></span>
        <span>kelly <b>{num(p.kelly_fraction, 2)}</b></span>
        <span>max hold <b>{p.max_hold_s}s</b></span>
      </div>
    </section>
  );
}

function UniversePanel({ state, serverNow }: { state: DeskState; serverNow: number }) {
  const u = state.universe;
  return (
    <section className="panel">
      <h2>Universe</h2>
      <div className="kv">
        <span>Futures active</span><b>{u.futures_active}</b>
        <span>· USDT / INR</span><b>{u.futures_usdt ?? DASH} / {u.futures_inr ?? DASH}</b>
        <span>Futures tracked</span><b>{u.futures_tracked}</b>
        <span>Spot active</span><b>{u.spot_active}</b>
        <span>Spot with ticker</span><b>{u.spot_with_ticker}</b>
        <span>Refreshed</span><b>{isNum(u.refreshed_ms) ? `${time(u.refreshed_ms)} (${ageS(u.refreshed_ms, serverNow)} ago)` : DASH}</b>
        <span>Spot ticker</span><b>{ageS(state.health.last_spot_ticker_ms, serverNow)} ago</b>
      </div>
    </section>
  );
}

function PositionsPanel({ positions, sinceFetchMs, maxHold }: { positions: Position[]; sinceFetchMs: number; maxHold: number }) {
  return (
    <section className="panel">
      <h2>Position workstation — open paper positions ({positions.length}) · countdown is display-only, backend owns the 120s timer</h2>
      {positions.length === 0 ? (
        <div className="empty">No open positions</div>
      ) : (
        <table>
          <thead>
            <tr>
              <th>Pair</th><th>Side</th><th>Qty</th><th>Entry</th><th>Mark</th><th>TP</th><th>SL</th>
              <th>Lev</th><th>Notional</th><th>uPnL</th><th>Fees</th><th>State</th><th>Age</th><th>Time left</th>
            </tr>
          </thead>
          <tbody>
            {positions.map((p) => {
              const age = p.age_s + sinceFetchMs / 1000;
              const hold = maxHold > 0 ? maxHold : p.age_s + p.time_left_s;
              const left = Math.max(0, hold - age);
              const frac = hold > 0 ? Math.min(1, left / hold) : 0;
              return (
                <tr key={p.id}>
                  <td>{p.pair}</td>
                  <td className={p.side}>{p.side.toUpperCase()}</td>
                  <td className="r">{qty(p.qty)}</td>
                  <td className="r">{px(p.entry_price)}</td>
                  <td className="r">{px(p.mark_price)}</td>
                  <td className="r">{px(p.tp_price)}</td>
                  <td className="r">{px(p.sl_price)}</td>
                  <td className="r">{num(p.leverage, 1)}x</td>
                  <td className="r">{money(p.notional)}</td>
                  <td className={"r " + sign(p.unrealized_pnl)}>{signedMoney(p.unrealized_pnl)}</td>
                  <td className="r">{num(p.fees_paid, 4)}</td>
                  <td>{p.state ?? DASH}</td>
                  <td className="r">{age.toFixed(1)}s</td>
                  <td className="countdown">
                    <div className="bar">
                      <div className={"fill " + (left < 20 ? "urgent" : "")} style={{ width: `${frac * 100}%` }} />
                    </div>
                    <span className={left < 20 ? "neg" : ""}>{left.toFixed(1)}s</span>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      )}
    </section>
  );
}

function OpportunitiesPanel({ rows }: { rows: Opportunity[] }) {
  const sorted = [...rows].sort((a, b) => b.ts_ms - a.ts_ms);
  return (
    <section className="panel">
      <h2>Opportunities ({rows.length})</h2>
      {sorted.length === 0 ? (
        <div className="empty">No opportunities evaluated yet</div>
      ) : (
        <div className="scroll">
          <table>
            <thead>
              <tr>
                <th>Time</th><th>Pair</th><th>Side</th><th>P(TP first)</th><th>P(SL first)</th><th>P(up)</th><th>Calib</th><th>EV gross bps</th><th>Cost bps</th>
                <th>EV net bps</th><th>TP</th><th>SL</th><th>Horizon</th><th>Qty</th><th>Notional</th><th>Lev</th>
                <th>Decision</th><th>Reason</th>
              </tr>
            </thead>
            <tbody>
              {sorted.map((o, i) => (
                <tr key={`${o.pair}-${o.ts_ms}-${i}`} className={o.decision === "taken" ? "taken" : ""}>
                  <td>{time(o.ts_ms)}</td>
                  <td>{o.pair}</td>
                  <td className={o.side}>{o.side.toUpperCase()}</td>
                  <td className="r">{pct(o.p_win, 1)}</td>
                  <td className="r">{pct(o.p_stop, 1)}</td>
                  <td className="r">{pct(o.p_up_horizon, 1)}</td>
                  <td className={o.calibrated ? "pos" : "neg"}>{o.calibrated === undefined ? DASH : o.calibrated ? "yes" : "NO"}</td>
                  <td className="r">{bps(o.ev_gross_bps)}</td>
                  <td className="r">{bps(o.cost_bps)}</td>
                  <td className={"r " + sign(o.ev_net_bps)}>{bps(o.ev_net_bps)}</td>
                  <td className="r">{num(o.tp_pct, 3)}%</td>
                  <td className="r">{num(o.sl_pct, 3)}%</td>
                  <td className="r">{o.horizon_s}s</td>
                  <td className="r">{qty(o.qty)}</td>
                  <td className="r">{money(o.notional)}</td>
                  <td className="r">{num(o.leverage, 1)}x</td>
                  <td className={o.decision === "taken" ? "pos" : "muted"}>{o.decision}</td>
                  <td className="reason" title={o.reason}>{o.reason}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function ScannerPanel({ rows, onSelect }: { rows: ScannerRow[]; onSelect: (pair: string, margin: string) => void }) {
  const top = [...rows].sort((a, b) => b.score - a.score).slice(0, 60);
  return (
    <section className="panel">
      <h2>
        Scanner — top {top.length} of {rows.length} by score
      </h2>
      {top.length === 0 ? (
        <div className="empty">Scanner warming up</div>
      ) : (
        <div className="scroll tall">
          <table>
            <thead>
              <tr>
                <th>Pair</th><th>Last</th><th>r10s</th><th>r30s</th><th>r60s</th><th>r300s</th><th>σ 1s</th>
                <th>σ 1m candle</th><th>z r10s</th><th>z vol</th><th>Basis bps</th><th>OBI</th><th>Spread bps</th>
                <th>Funding</th><th>24h</th><th>Regime</th><th>Score</th><th>Anomaly</th>
              </tr>
            </thead>
            <tbody>
              {top.map((r) => (
                <tr key={`${r.pair}-${r.margin ?? ""}`} className={"click " + (r.anomaly ? "anomaly" : "")} onClick={() => onSelect(r.pair, r.margin ?? "USDT")}>
                  <td>{r.pair}{r.margin ? <span className="muted"> {r.margin}</span> : null}</td>
                  <td className="r">{px(r.last)}</td>
                  <td className={"r " + sign(r.ret_10s)}>{pct(r.ret_10s, 3)}</td>
                  <td className={"r " + sign(r.ret_30s)}>{pct(r.ret_30s, 3)}</td>
                  <td className={"r " + sign(r.ret_60s)}>{pct(r.ret_60s, 3)}</td>
                  <td className={"r " + sign(r.ret_300s)}>{pct(r.ret_300s, 3)}</td>
                  <td className="r">{pct(r.vol_1s, 4)}</td>
                  <td className="r">{pct(r.vol_1m_candle, 3)}</td>
                  <td className="r">{num(r.z_ret_10s, 2)}</td>
                  <td className="r">{num(r.z_volume, 2)}</td>
                  <td className="r">{bps(r.basis_bps)}</td>
                  <td className={"r " + sign(r.obi)}>{num(r.obi, 3)}</td>
                  <td className="r">{bps(r.spread_bps)}</td>
                  <td className="r">{pct(r.funding_rate, 4)}</td>
                  <td className={"r " + sign(r.change_24h_pct)}>{isNum(r.change_24h_pct) ? r.change_24h_pct.toFixed(2) + "%" : DASH}</td>
                  <td>{r.regime ?? DASH}</td>
                  <td className="r">{num(r.score, 3)}</td>
                  <td className="reason" title={r.anomaly_reasons.join(", ")}>
                    {r.anomaly ? "⚠ " : ""}
                    {r.anomaly_reasons.join(", ")}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function FillsPanel({ rows }: { rows: Fill[] }) {
  const sorted = [...rows].sort((a, b) => b.ts_ms - a.ts_ms);
  return (
    <section className="panel">
      <h2>Paper fills ({rows.length})</h2>
      {sorted.length === 0 ? (
        <div className="empty">No fills yet</div>
      ) : (
        <div className="scroll">
          <table>
            <thead>
              <tr>
                <th>Time</th><th>Pair</th><th>Side</th><th>Qty</th><th>Price</th><th>Ref mid</th><th>Slip bps</th>
                <th>Fee</th><th>Liq</th><th>Reason</th>
              </tr>
            </thead>
            <tbody>
              {sorted.map((f) => (
                <tr key={f.id}>
                  <td>{time(f.ts_ms)}</td>
                  <td>{f.pair}</td>
                  <td className={f.side === "buy" ? "long" : "short"}>{f.side.toUpperCase()}</td>
                  <td className="r">{qty(f.qty)}</td>
                  <td className="r">{px(f.price)}</td>
                  <td className="r">{px(f.ref_mid)}</td>
                  <td className="r">{bps(f.slippage_bps)}</td>
                  <td className="r">{num(f.fee, 4)}</td>
                  <td>{f.liquidity}</td>
                  <td className="reason" title={f.reason}>{f.reason}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function ClosedPanel({ rows }: { rows: ClosedTrade[] }) {
  const sorted = [...rows].sort((a, b) => b.closed_ms - a.closed_ms);
  return (
    <section className="panel">
      <h2>Closed trades ({rows.length})</h2>
      {sorted.length === 0 ? (
        <div className="empty">No closed trades yet</div>
      ) : (
        <div className="scroll">
          <table>
            <thead>
              <tr>
                <th>Closed</th><th>Pair</th><th>Side</th><th>Qty</th><th>Entry</th><th>Exit</th><th>Gross</th>
                <th>Fees</th><th>Net</th><th>Held</th><th>Exit</th>
              </tr>
            </thead>
            <tbody>
              {sorted.map((c) => (
                <tr key={c.id}>
                  <td>{time(c.closed_ms)}</td>
                  <td>{c.pair}</td>
                  <td className={c.side}>{c.side.toUpperCase()}</td>
                  <td className="r">{qty(c.qty)}</td>
                  <td className="r">{px(c.entry_price)}</td>
                  <td className="r">{px(c.exit_price)}</td>
                  <td className={"r " + sign(c.gross_pnl)}>{signedMoney(c.gross_pnl)}</td>
                  <td className="r">{num(c.fees, 4)}</td>
                  <td className={"r " + sign(c.net_pnl)}>{signedMoney(c.net_pnl)}</td>
                  <td className="r">{num(c.held_s, 1)}s</td>
                  <td>{c.exit_reason}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function AuditPanel({ rows }: { rows: AuditEntry[] }) {
  const sorted = [...rows].sort((a, b) => b.ts_ms - a.ts_ms);
  return (
    <section className="panel">
      <h2>Audit log ({rows.length})</h2>
      {sorted.length === 0 ? (
        <div className="empty">No audit entries</div>
      ) : (
        <div className="scroll">
          <table>
            <thead>
              <tr><th>Time</th><th>Kind</th><th>Message</th></tr>
            </thead>
            <tbody>
              {sorted.map((e, i) => (
                <tr key={`${e.ts_ms}-${i}`} title={e.data === undefined || e.data === null ? "" : compactJson(e.data)}>
                  <td>{time(e.ts_ms)}</td>
                  <td className={"kind kind-" + e.kind}>{e.kind}</td>
                  <td className="msg">{e.message}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}
