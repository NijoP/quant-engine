import { Fragment, useEffect, useMemo, useState } from "react";
import {
  getJson,
  postControl,
  type AgentStatus,
  type Candle,
  type ControlsState,
  type DeskState,
  type InstrumentResp,
  type Target,
  type UniverseRecord,
  type UniverseResp,
} from "./api";

const DASH = "—";
type N = number | null | undefined;
const isNum = (v: N): v is number => typeof v === "number" && Number.isFinite(v);
const pct = (v: N, d = 2) => (isNum(v) ? (v * 100).toFixed(d) + "%" : DASH);
const num = (v: N, d = 2) => (isNum(v) ? v.toFixed(d) : DASH);
const money = (v: N) =>
  isNum(v) ? v.toLocaleString(undefined, { minimumFractionDigits: 2, maximumFractionDigits: 2 }) : DASH;
const ago = (ms: N, now: number) => (isNum(ms) ? ((now - ms) / 1000).toFixed(1) + "s" : DASH);
const cell = (v: unknown): string => {
  if (v === null || v === undefined || v === "") return DASH;
  if (typeof v === "number") return Number.isFinite(v) ? String(+v.toPrecision(8)) : DASH;
  if (typeof v === "object") return JSON.stringify(v);
  return String(v);
};

/** Poll a GET endpoint; returns data plus an honest error string (null when OK). */
export function usePoll<T>(path: string | null, ms: number): { data: T | null; error: string | null } {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    setData(null);
    setError(null);
    if (!path) return;
    let alive = true;
    const tick = async () => {
      const ctl = new AbortController();
      const t = setTimeout(() => ctl.abort(), 4000);
      try {
        const d = await getJson<T>(path, ctl.signal);
        if (alive) {
          setData(d);
          setError(null);
        }
      } catch (e) {
        if (alive) setError(e instanceof Error ? e.message : String(e));
      } finally {
        clearTimeout(t);
      }
    };
    tick();
    const id = setInterval(tick, ms);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, [path, ms]);
  return { data, error };
}

function ErrLine({ error }: { error: string | null }) {
  return error ? <div className="errline">endpoint error: {error}</div> : null;
}

/* ---------------- A. Global status ---------------- */
export function GlobalStatus({ state, serverNow, offline }: { state: DeskState; serverNow: number; offline: boolean }) {
  const h = state.health;
  const rtAge = isNum(h.last_rt_ms) ? (serverNow - h.last_rt_ms) / 1000 : null;
  const agents = state.agents ?? [];
  const bad = agents.filter((a) => a.status !== "ok").length;
  const c = state.controls;
  const items: [string, string, "ok" | "warn" | "bad"][] = [
    ["CoinDCX REST", offline ? "desk offline" : h.errors > 0 ? `${h.errors} errors` : "ok", offline ? "bad" : h.errors > 0 ? "warn" : "ok"],
    ["Futures feed", rtAge === null ? "no data" : `${rtAge.toFixed(1)}s ago${h.stale ? " STALE" : ""}`, rtAge === null || h.stale || rtAge > 5 ? "bad" : "ok"],
    ["Universe scan", isNum(state.universe.refreshed_ms) ? `${ago(state.universe.refreshed_ms, serverNow)} ago` : "pending", isNum(state.universe.refreshed_ms) ? "ok" : "warn"],
    ["Forecast / strategy", c ? (c.strategy_enabled ? "evaluating" : "disabled") : "not reported", c?.strategy_enabled ? "ok" : "warn"],
    ["Agents", agents.length ? `${agents.length - bad}/${agents.length} ok` : "not reported", agents.length && !bad ? "ok" : "warn"],
    ["Risk governor", state.account.halted ? `HALTED ${state.account.halt_reason ?? ""}` : c?.killed ? "KILLED" : "active", state.account.halted || c?.killed ? "bad" : "ok"],
    ["Execution", `${state.mode.toUpperCase()} · live ${state.live_orders_enabled ? "ON" : "OFF"}`, state.live_orders_enabled ? "bad" : "ok"],
    ["Local time", new Date(serverNow).toLocaleTimeString(undefined, { hour12: false }), "ok"],
  ];
  return (
    <section className="panel status-grid">
      {items.map(([k, v, s]) => (
        <div key={k} className={"st st-" + s}>
          <div className="label">{k}</div>
          <div className="value">{v}</div>
        </div>
      ))}
    </section>
  );
}

/* ---------------- B. All-instrument universe ---------------- */
type Coll = "futures" | "spot" | "events";
const PAGE = 50;

function pickKeys(rows: UniverseRecord[]): string[] {
  const pref = ["pair", "symbol", "market", "margin", "margin_currency_short_name", "status", "last", "last_price", "volume_24h",
    "volume", "spread_bps", "max_leverage_long", "max_leverage_short", "min_notional", "taker_fee", "maker_fee", "eligible", "rank", "score"];
  const all = new Set<string>();
  rows.slice(0, 50).forEach((r) => Object.keys(r).forEach((k) => all.add(k)));
  const ordered = pref.filter((k) => all.has(k));
  for (const k of all) if (!ordered.includes(k) && ordered.length < 14) ordered.push(k);
  return ordered;
}

export function UniverseTable({ onSelect }: { onSelect: (pair: string, margin: string) => void }) {
  const { data, error } = usePoll<UniverseResp>("/api/universe", 15000);
  const [coll, setColl] = useState<Coll>("futures");
  const [q, setQ] = useState("");
  const [margin, setMargin] = useState("all");
  const [sortKey, setSortKey] = useState<string | null>(null);
  const [desc, setDesc] = useState(true);
  const [page, setPage] = useState(0);
  const rows = data?.[coll] ?? [];
  const keys = useMemo(() => pickKeys(rows), [rows]);
  const filtered = useMemo(() => {
    const ql = q.trim().toLowerCase();
    let r = rows.filter((x) => !ql || JSON.stringify(x).toLowerCase().includes(ql));
    if (coll === "futures" && margin !== "all")
      r = r.filter((x) => String(x.margin ?? x.margin_currency_short_name ?? "").toUpperCase() === margin);
    if (sortKey) {
      r = [...r].sort((a, b) => {
        const av = a[sortKey], bv = b[sortKey];
        const c = typeof av === "number" && typeof bv === "number" ? av - bv : String(av ?? "").localeCompare(String(bv ?? ""));
        return desc ? -c : c;
      });
    }
    return r;
  }, [rows, q, margin, sortKey, desc, coll]);
  const pages = Math.max(1, Math.ceil(filtered.length / PAGE));
  const p = Math.min(page, pages - 1);
  return (
    <section className="panel">
      <h2>
        Universe — spot {data?.spot.length ?? DASH} · futures {data?.futures.length ?? DASH} · events {data?.events.length ?? DASH}
      </h2>
      <ErrLine error={error} />
      <div className="toolbar">
        {(["futures", "spot", "events"] as Coll[]).map((c) => (
          <button key={c} className={"tab " + (coll === c ? "on" : "")} onClick={() => { setColl(c); setPage(0); setSortKey(null); }}>
            {c}
          </button>
        ))}
        {coll === "futures" && (
          <select value={margin} onChange={(e) => { setMargin(e.target.value); setPage(0); }}>
            <option value="all">all margins</option>
            <option value="USDT">USDT</option>
            <option value="INR">INR</option>
          </select>
        )}
        <input placeholder="search…" value={q} onChange={(e) => { setQ(e.target.value); setPage(0); }} />
        <span className="muted">{filtered.length} rows · page {p + 1}/{pages}</span>
        <button onClick={() => setPage(Math.max(0, p - 1))} disabled={p === 0}>‹</button>
        <button onClick={() => setPage(Math.min(pages - 1, p + 1))} disabled={p >= pages - 1}>›</button>
      </div>
      {!data ? (
        <div className="empty">{error ? "Universe unavailable" : "Loading universe…"}</div>
      ) : (
        <div className="scroll tall">
          <table>
            <thead>
              <tr>
                {keys.map((k) => (
                  <th key={k} className="sortable" onClick={() => { if (sortKey === k) setDesc(!desc); else { setSortKey(k); setDesc(true); } }}>
                    {k}{sortKey === k ? (desc ? " ▼" : " ▲") : ""}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {filtered.slice(p * PAGE, p * PAGE + PAGE).map((r, i) => {
                const pair = String(r.pair ?? r.symbol ?? "");
                const m = String(r.margin ?? r.margin_currency_short_name ?? "USDT");
                return (
                  <tr key={i} className={coll === "futures" ? "click" : ""} onClick={() => coll === "futures" && pair && onSelect(pair, m)}>
                    {keys.map((k) => <td key={k}>{cell(r[k])}</td>)}
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

/* ---------------- D/E. Instrument intelligence + forecast ---------------- */
const RES = [
  ["1m", "1"], ["5m", "5"], ["15m", "15"], ["1h", "60"], ["1d", "1D"],
] as const;

function CandleChart({ candles }: { candles: Candle[] }) {
  const done = candles.filter((c) => c.complete).slice(-120);
  if (done.length < 2) return <div className="empty">Not enough completed candles</div>;
  const W = 720, H = 200;
  const lo = Math.min(...done.map((c) => c.low)), hi = Math.max(...done.map((c) => c.high));
  const y = (v: number) => H - 4 - ((v - lo) / (hi - lo || 1)) * (H - 8);
  const step = W / done.length;
  return (
    <svg viewBox={`0 0 ${W} ${H}`} className="chart" preserveAspectRatio="none">
      {done.map((c, i) => {
        const up = c.close >= c.open;
        const x = i * step + step / 2;
        return (
          <g key={c.time} className={up ? "cu" : "cd"}>
            <line x1={x} x2={x} y1={y(c.high)} y2={y(c.low)} />
            <rect x={x - step * 0.35} width={Math.max(1, step * 0.7)} y={y(Math.max(c.open, c.close))}
              height={Math.max(1, Math.abs(y(c.open) - y(c.close)))} />
          </g>
        );
      })}
      <text x={4} y={12} className="axis">{hi.toPrecision(6)}</text>
      <text x={4} y={H - 4} className="axis">{lo.toPrecision(6)}</text>
    </svg>
  );
}

export function InstrumentPanel({ sel }: { sel: { pair: string; margin: string } | null }) {
  const [res, setRes] = useState<string>("1");
  const base = sel ? `/api/instrument/${encodeURIComponent(sel.pair)}` : null;
  const inst = usePoll<InstrumentResp>(base && sel ? `${base}?margin=${sel.margin}` : null, 2000);
  const candles = usePoll<Candle[]>(base && sel ? `${base}/candles?resolution=${res}&margin=${sel.margin}` : null, 15000);
  if (!sel)
    return (
      <section className="panel">
        <h2>Instrument intelligence</h2>
        <div className="empty">Select an instrument in the scanner or universe table</div>
      </section>
    );
  const f = inst.data?.forecast;
  const ft = inst.data?.features;
  return (
    <section className="panel">
      <h2>Instrument intelligence — {sel.pair} ({sel.margin})</h2>
      <ErrLine error={inst.error ?? candles.error} />
      <div className="grid2">
        <div>
          <div className="toolbar">
            {RES.map(([l, r]) => (
              <button key={r} className={"tab " + (res === r ? "on" : "")} onClick={() => setRes(r)}>{l}</button>
            ))}
            <span className="muted">completed candles only · 3d / 2d / 1s views not exposed by desk API v1</span>
          </div>
          {candles.data ? <CandleChart candles={candles.data} /> : <div className="empty">{candles.error ? "Candles unavailable" : "Loading candles…"}</div>}
          {ft && (
            <div className="kv cols">
              <span>regime</span><b>{ft.regime ?? DASH}</b>
              <span>spread bps</span><b>{num(ft.spread_bps, 1)}</b>
              <span>OBI</span><b>{num(ft.obi, 3)}</b>
              <span>σ 1s</span><b>{pct(ft.vol_1s, 4)}</b>
              <span>r10s / r60s</span><b>{pct(ft.ret_10s, 3)} / {pct(ft.ret_60s, 3)}</b>
              <span>z vol</span><b>{num(ft.z_volume, 2)}</b>
            </div>
          )}
        </div>
        <div>
          <h2>
            Forecast — ESTIMATES, not facts{" "}
            {f && <span className={"badge " + (f.calibrated ? "safe" : "danger")}>{f.calibrated ? "calibrated" : "UNCALIBRATED"}</span>}
          </h2>
          {!f ? (
            <div className="empty">No forecast for this instrument</div>
          ) : (
            <>
              <div className="muted">model {f.model} · μ/s {f.mu_per_s.toExponential(3)} · σ/s {f.sigma_per_s.toExponential(3)}</div>
              <table>
                <thead><tr><th>h</th><th>E[ret]</th><th>P(up)</th><th>P(down)</th><th>σ_h</th><th>±1σ band</th></tr></thead>
                <tbody>
                  {f.horizons.map((h) => (
                    <tr key={h.h_s}>
                      <td>{h.h_s}s</td>
                      <td className="r">{pct(h.exp_ret, 4)}</td>
                      <td className="r">{pct(h.p_up, 1)}</td>
                      <td className="r">{pct(h.p_down, 1)}</td>
                      <td className="r">{pct(h.sigma_h, 4)}</td>
                      <td className="r">{pct(h.exp_ret - h.sigma_h, 3)} … {pct(h.exp_ret + h.sigma_h, 3)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
              <div className="kv cols">
                <span>P(TP first ≤120s)</span><b>{pct(f.first_passage.p_target, 1)}</b>
                <span>P(SL first ≤120s)</span><b>{pct(f.first_passage.p_stop, 1)}</b>
                <span>P(neither)</span><b>{pct(f.first_passage.p_neither, 1)}</b>
                <span>E[t to TP]</span><b>{num(f.first_passage.e_t_target, 1)}s</b>
                <span>E[t to SL]</span><b>{num(f.first_passage.e_t_stop, 1)}s</b>
              </div>
            </>
          )}
          <h2>Order book</h2>
          <pre className="book">{inst.data?.book ? JSON.stringify(inst.data.book, null, 1).slice(0, 1500) : "No book snapshot"}</pre>
        </div>
      </div>
    </section>
  );
}

/* ---------------- F. Mathematical brain ---------------- */
export function DecisionsPanel() {
  const { data, error } = usePoll<UniverseRecord[]>("/api/decisions", 3000);
  const [open, setOpen] = useState<number | null>(null);
  const rows = data ?? [];
  return (
    <section className="panel">
      <h2>Mathematical brain — decisions ({rows.length}) · click a row for the full numeric explanation</h2>
      <ErrLine error={error} />
      {rows.length === 0 ? (
        <div className="empty">{error ? "Decisions unavailable" : "No decisions recorded yet"}</div>
      ) : (
        <div className="scroll">
          <table>
            <thead><tr><th>#</th><th>pair</th><th>side</th><th>decision</th><th>reason</th></tr></thead>
            <tbody>
              {rows.slice(0, 200).map((d, i) => (
                <Fragment key={i}>
                  <tr className="click" onClick={() => setOpen(open === i ? null : i)}>
                    <td>{i}</td>
                    <td>{cell(d.pair)}</td>
                    <td>{cell(d.side)}</td>
                    <td className={d.decision === "taken" || d.approved === true ? "pos" : "muted"}>{cell(d.decision ?? d.approved)}</td>
                    <td className="reason">{cell(d.reason)}</td>
                  </tr>
                  {open === i && (
                    <tr><td colSpan={5}><pre className="book">{JSON.stringify(d, null, 2)}</pre></td></tr>
                  )}
                </Fragment>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

/* ---------------- G. Agents ---------------- */
export function AgentsPanel({ agents, serverNow }: { agents: AgentStatus[] | undefined; serverNow: number }) {
  return (
    <section className="panel">
      <h2>Agent control room ({agents?.length ?? 0})</h2>
      {!agents || agents.length === 0 ? (
        <div className="empty">Backend does not report agents yet</div>
      ) : (
        <table>
          <thead><tr><th>Agent</th><th>Status</th><th>Last run</th><th>Last ok</th><th>Dur ms</th><th>Runs</th><th>Err</th><th>Output / error</th></tr></thead>
          <tbody>
            {agents.map((a) => (
              <tr key={a.id}>
                <td>{a.name}</td>
                <td className={"ag-" + a.status}>{a.status}</td>
                <td>{ago(a.last_run_ms, serverNow)}</td>
                <td>{ago(a.last_ok_ms, serverNow)}</td>
                <td className="r">{num(a.duration_ms, 0)}</td>
                <td className="r">{a.runs}</td>
                <td className={"r " + (a.errors ? "neg" : "")}>{a.errors}</td>
                <td className="reason" title={a.last_error ?? a.last_output ?? ""}>{a.last_error ? "✖ " + a.last_error : a.last_output ?? DASH}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  );
}

/* ---------------- I. Capital & target ---------------- */
export function TargetPanel({ state }: { state: DeskState }) {
  const t: Target | undefined = state.target;
  const a = state.account;
  const [estR, setEstR] = useState(0.01);
  if (!t)
    return (
      <section className="panel">
        <h2>Capital & target</h2>
        <div className="empty">Backend does not report target yet</div>
      </section>
    );
  const n = Math.log(t.target_equity / Math.max(a.equity, 1e-9)) / Math.log(1 + estR);
  const tph = t.hours_remaining > 0 ? n / t.hours_remaining : null;
  const s = t.simulation;
  return (
    <section className="panel">
      <h2>
        Capital & target — stage {t.stage} · {a.policy.tier} mode {a.paper !== false && <span className="badge mode">PAPER EQUITY</span>}
      </h2>
      <div className="kv cols">
        <span>start equity</span><b>{money(t.start_equity)}</b>
        <span>actual equity</span><b>{money(a.equity)}</b>
        <span>available</span><b>{money(a.available)}</b>
        <span>used margin</span><b>{money(a.used_margin)}</b>
        <span>stage target</span><b>{money(t.target_equity)}</b>
        <span>remaining</span><b>{money(t.target_equity - a.equity)}</b>
        <span>growth required</span><b>{num(t.growth_required, 2)}×</b>
        <span>hours remaining</span><b>{num(t.hours_remaining, 2)}</b>
        <span>deadline</span><b>{new Date(t.deadline_ms).toLocaleString()}</b>
        <span>session loss limit</span><b>{pct(a.policy.daily_loss_limit_frac, 2)}</b>
        <span>actual trades/h</span><b>{num(t.actual_trades_per_hour, 1)}</b>
        <span>avg win / loss</span><b>{money(t.avg_win)} / {money(t.avg_loss)}</b>
        <span>expectancy</span><b>{money(t.expectancy)}</b>
        <span>geometric growth</span><b>{pct(t.geometric_growth, 3)}</b>
      </div>
      <table>
        <thead><tr><th>Trades/hour</th><th>Required return per trade</th></tr></thead>
        <tbody>
          {t.scenarios.map((sc) => (
            <tr key={sc.trades_per_hour}><td>{sc.trades_per_hour}</td><td className="r">{pct(sc.required_return_per_trade, 3)}</td></tr>
          ))}
        </tbody>
      </table>
      <div className="toolbar">
        <span>est. return/trade</span>
        <input type="number" step="0.001" min="0.0001" value={estR} onChange={(e) => setEstR(Number(e.target.value) || 0.001)} />
        <span>
          ⇒ n = ln(Ct/C)/ln(1+r) = <b>{isFinite(n) ? n.toFixed(0) : DASH}</b> trades · <b>{num(tph, 1)}</b>/h
        </span>
      </div>
      <div className="muted">A trade quota never forces entries; the risk governor decides.</div>
      {s ? (
        <div className="kv cols">
          <span>P(reach target)</span><b>{pct(s.p_reach_target, 1)}</b>
          <span>P(session loss first)</span><b>{pct(s.p_session_loss_first, 1)}</b>
          <span>P(floor breach)</span><b>{pct(s.p_floor_breach, 1)}</b>
          <span>P(losing streak)</span><b>{pct(s.p_losing_streak, 1)}</b>
          <span>P(unrecovered DD)</span><b>{pct(s.p_unrecovered_drawdown, 1)}</b>
        </div>
      ) : (
        <div className="empty">Stochastic target simulation not available yet</div>
      )}
    </section>
  );
}

/* ---------------- J. Risk & emergency controls ---------------- */
type Ctl = {
  label: string;
  path: string;
  body?: (c: ControlsState) => unknown;
  on?: (c: ControlsState) => boolean;
  danger?: boolean;
  confirm?: string;
};
const CONTROLS: Ctl[] = [
  { label: "Scanner", path: "/api/control/scanner", body: (c) => ({ running: !c.scanner_running }), on: (c) => c.scanner_running },
  { label: "Paper trading", path: "/api/control/paper", body: (c) => ({ running: !c.paper_running }), on: (c) => c.paper_running },
  { label: "Strategy eval", path: "/api/control/strategy", body: (c) => ({ enabled: !c.strategy_enabled }), on: (c) => c.strategy_enabled },
  { label: "Pause entries", path: "/api/control/pause_entries", body: (c) => ({ paused: !c.entries_paused }), on: (c) => c.entries_paused },
  { label: "Cancel entry orders", path: "/api/control/cancel_entries" },
  { label: "Emergency flatten", path: "/api/control/flatten", danger: true, confirm: "Flatten ALL paper positions at market?" },
  { label: "KILL SWITCH", path: "/api/control/kill", danger: true, confirm: "Kill: flatten and disable entries until restart?" },
];

export function ControlsPanel({ controls, offline }: { controls: ControlsState | undefined; offline: boolean }) {
  const [busy, setBusy] = useState<string | null>(null);
  const [log, setLog] = useState<{ ts: number; label: string; ok: boolean; msg: string }[]>([]);
  const run = async (c: Ctl) => {
    if (!controls) return;
    if (c.confirm && !window.confirm(c.confirm)) return;
    setBusy(c.path);
    try {
      const r = await postControl(c.path, c.body?.(controls));
      setLog((l) => [{ ts: Date.now(), label: c.label, ok: r.ok, msg: r.message }, ...l].slice(0, 20));
    } catch (e) {
      setLog((l) => [{ ts: Date.now(), label: c.label, ok: false, msg: e instanceof Error ? e.message : String(e) }, ...l].slice(0, 20));
    } finally {
      setBusy(null);
    }
  };
  const reason = offline
    ? "desk offline"
    : !controls
      ? "backend does not report controls state"
      : controls.killed
        ? "kill switch engaged — restart required"
        : null;
  return (
    <section className="panel">
      <h2>Risk & emergency controls</h2>
      <div className="toolbar">
        {CONTROLS.map((c) => {
          const isOn = controls && c.on ? c.on(controls) : null;
          return (
            <button
              key={c.path}
              className={"ctl " + (c.danger ? "danger " : "") + (isOn ? "on" : "")}
              disabled={!!reason || busy !== null}
              title={reason ?? c.path}
              onClick={() => run(c)}
            >
              {c.label}
              {isOn === null ? "" : isOn ? ": ON" : ": OFF"}
            </button>
          );
        })}
      </div>
      {reason && <div className="errline">controls disabled: {reason}</div>}
      {log.map((l, i) => (
        <div key={i} className={l.ok ? "pos" : "neg"}>
          {new Date(l.ts).toLocaleTimeString(undefined, { hour12: false })} {l.label}: {l.ok ? "OK" : "FAILED"} — {l.msg}
        </div>
      ))}
    </section>
  );
}
