// Operational-health strip: connectivity, auth configured (yes/no only, never secrets), data freshness, last
// reconciliation, kill switch, watchdog. Each cell renders from /api/state when the desk reports the field,
// and says "not reported" otherwise; nothing is assumed healthy.
import type { DeskState } from "./api";

type Level = "ok" | "warn" | "bad" | "na";

function ago(ms: number | null | undefined, now: number): string | null {
  return typeof ms === "number" && Number.isFinite(ms) ? `${Math.max(0, (now - ms) / 1000).toFixed(0)}s ago` : null;
}

function Cell({ label, value, level }: { label: string; value: string; level: Level }) {
  const cls = level === "ok" ? "st st-ok" : level === "warn" ? "st st-warn" : level === "bad" ? "st st-bad" : "st";
  return (
    <div className={cls}>
      <div className="label">{label}</div>
      <div className="value" title={value}>{value}</div>
    </div>
  );
}

export function OpsHealthStrip({ state, serverNow, offline }: { state: DeskState | null; serverNow: number; offline: boolean }) {
  const h = state?.health;
  const rtAge = h && typeof h.last_rt_ms === "number" ? (serverNow - h.last_rt_ms) / 1000 : null;
  const creds = state?.live?.gate?.credentials_configured;
  const killed = state?.controls?.killed;
  const ops = state?.ops;
  const watchdogAgent = state?.agents?.find((a) => /watchdog/i.test(a.id) || /watchdog/i.test(a.name));
  const watchdog = ops?.watchdog ?? watchdogAgent?.status ?? null;
  const recon = ago(ops?.last_reconciliation_ms, serverNow);
  return (
    <section className="panel">
      <h2>Operational health</h2>
      <div className="status-grid">
        <Cell label="connectivity" value={offline ? "desk offline" : state ? "desk reachable" : "waiting"} level={offline ? "bad" : state ? "ok" : "na"} />
        <Cell
          label="auth configured"
          value={creds === undefined ? "not reported" : creds ? "yes" : "no"}
          level={creds === undefined ? "na" : "ok"}
        />
        <Cell
          label="data freshness"
          value={rtAge === null ? "not reported" : `rt ${rtAge.toFixed(1)}s ago${h?.stale ? " · stale" : ""}`}
          level={rtAge === null ? "na" : rtAge < 5 && !h?.stale ? "ok" : "warn"}
        />
        <Cell
          label="last reconciliation"
          value={recon ? `${recon}${ops?.last_reconciliation_result ? " · " + ops.last_reconciliation_result : ""}` : "not reported"}
          level={recon ? "ok" : "na"}
        />
        <Cell label="kill switch" value={killed === undefined ? "not reported" : killed ? "ENGAGED" : "not engaged"} level={killed === undefined ? "na" : killed ? "bad" : "ok"} />
        <Cell
          label="watchdog"
          value={watchdog ?? "not reported"}
          level={watchdog === null ? "na" : watchdog === "ok" ? "ok" : "warn"}
        />
      </div>
    </section>
  );
}
