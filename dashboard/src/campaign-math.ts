// Pure helpers for the 24 NOVEMBER CAMPAIGN panel. No React, no fetching: unit-tested in campaign-math.test.ts.
// The desk owns the campaign state; these helpers only format it and draw the reference curve.

/** 23-hour campaign: 00:00 → 23:00 IST on 24 Nov 2026 (the 22-hour schedule is superseded). */
export const CAMPAIGN_HOURS = 23;
/** E0: last verified real-account read before the campaign (immutable measurement baseline). */
export const CAMPAIGN_START_EQUITY = 101.45;
export const CAMPAIGN_TARGET = 50000;
/** Hour index at which campaign entries may first be evaluated (01:00 IST). */
export const FIRST_ENTRY_HOUR = 1;
export const IST_TZ = "Asia/Kolkata";

/**
 * Mathematical requirement, not a trading forecast: start × (target/start)^(h/hours).
 * With the campaign's ₹101.45 → ₹50,000 over 23 h this is 101.45 × (50000/101.45)^(h/23), ≈ +30.9407% per hour.
 * `h` is elapsed hours (fractional allowed), clamped to [0, hours].
 */
export function requiredEquity(h: number, start = CAMPAIGN_START_EQUITY, target = CAMPAIGN_TARGET, hours = CAMPAIGN_HOURS): number {
  const x = Math.min(hours, Math.max(0, h));
  return start * Math.pow(target / start, x / hours);
}

/** Hourly multiplier of the schedule: (target/start)^(1/hours). */
export function hourlyMultiplier(start = CAMPAIGN_START_EQUITY, target = CAMPAIGN_TARGET, hours = CAMPAIGN_HOURS): number {
  return Math.pow(target / start, 1 / hours);
}

/** Parse a decimal string (or number) to a finite number, else null. */
export function n(v: string | number | null | undefined): number | null {
  if (v === null || v === undefined || v === "") return null;
  const x = typeof v === "number" ? v : Number(v);
  return Number.isFinite(x) ? x : null;
}

const DASH = "—";

/** ₹ with Indian digit grouping, 2 decimals; true minus sign; optional leading + for gains. */
export function inr(v: string | number | null | undefined, signed = false): string {
  const x = n(v);
  if (x === null) return DASH;
  const s = "₹" + Math.abs(x).toLocaleString("en-IN", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
  if (x < 0) return "−" + s;
  return signed && x > 0 ? "+" + s : s;
}

/** Fraction (0.309407) as percent with `d` decimals (30.9407%). */
export function pct(v: number | null | undefined, d = 2): string {
  return typeof v === "number" && Number.isFinite(v) ? (v * 100).toFixed(d) + "%" : DASH;
}

/** Seconds as `Hh MMm SSs`; non-positive reads as "deadline passed". */
export function duration(secs: number | null | undefined): string {
  if (typeof secs !== "number" || !Number.isFinite(secs)) return DASH;
  if (secs <= 0) return "deadline passed";
  const s = Math.floor(secs);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return `${h}h ${String(m).padStart(2, "0")}m ${String(s % 60).padStart(2, "0")}s`;
}

/** Wall-clock in IST regardless of the viewer's timezone, e.g. "24 Nov 2026, 23:00:00 IST". */
export function istDateTime(ms: number): string {
  if (!Number.isFinite(ms)) return DASH;
  const s = new Intl.DateTimeFormat("en-GB", {
    timeZone: IST_TZ,
    day: "2-digit",
    month: "short",
    year: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  }).format(new Date(ms));
  return s + " IST";
}

/** HH:MM in IST. */
export function istHourMinute(ms: number): string {
  if (!Number.isFinite(ms)) return DASH;
  return new Intl.DateTimeFormat("en-GB", { timeZone: IST_TZ, hour: "2-digit", minute: "2-digit", hour12: false }).format(new Date(ms));
}

/** Human label for a campaign status. Neutral wording on purpose: being behind is a reporting fact, not a call to act. */
export function statusLabel(s: string): string {
  switch (s) {
    case "ahead":
      return "ahead of schedule";
    case "on_track":
      return "on track";
    case "behind":
      return "behind schedule";
    case "stopped":
      return "stopped";
    case "completed":
      return "completed";
    case "expired":
      return "expired";
    default:
      return s;
  }
}

export interface ChartGeom {
  width: number;
  height: number;
  padL: number;
  padR: number;
  padT: number;
  padB: number;
  minY: number;
  maxY: number;
}

/**
 * Log-scale geometry: the target curve is exponential, so on a log axis it is a straight line and the ₹95 floor
 * stays visible next to ₹50,000. `values` widens the range to include actual equity outside [floor, target].
 */
export function chartGeom(floor: number, target: number, values: number[], width = 900, height = 260): ChartGeom {
  const finite = values.filter((v) => Number.isFinite(v) && v > 0);
  const lo = Math.min(floor, ...finite);
  const hi = Math.max(target, ...finite);
  return { width, height, padL: 64, padR: 12, padT: 12, padB: 26, minY: lo * 0.9, maxY: hi * 1.1 };
}

export function xOf(g: ChartGeom, hours: number): number {
  const c = Math.min(CAMPAIGN_HOURS, Math.max(0, hours));
  return g.padL + (c / CAMPAIGN_HOURS) * (g.width - g.padL - g.padR);
}

export function yOf(g: ChartGeom, v: number): number {
  const lv = Math.log(Math.max(v, g.minY));
  const l0 = Math.log(g.minY);
  const l1 = Math.log(g.maxY);
  return g.padT + (1 - (lv - l0) / (l1 - l0)) * (g.height - g.padT - g.padB);
}

/** SVG path of the mathematical target curve, sampled every `step` hours. */
export function targetPath(g: ChartGeom, start: number, target: number, step = 0.25): string {
  const pts: string[] = [];
  for (let h = 0; h <= CAMPAIGN_HOURS + 1e-9; h += step) {
    pts.push(`${xOf(g, h).toFixed(1)},${yOf(g, requiredEquity(h, start, target)).toFixed(1)}`);
  }
  return "M" + pts.join("L");
}

/**
 * Verified equity points inside the campaign window, sorted by time. Only samples from the desk's
 * `equity_history` are used: nothing is interpolated past the last real sample or invented before the first.
 */
export function actualPoints(startMs: number, history: { t: string; equity_inr: string }[]): { h: number; v: number }[] {
  return history
    .map((p) => ({ h: (Date.parse(p.t) - startMs) / 3_600_000, v: n(p.equity_inr) }))
    .filter((p): p is { h: number; v: number } => Number.isFinite(p.h) && p.h >= 0 && p.h <= CAMPAIGN_HOURS && p.v !== null && p.v > 0)
    .sort((a, b) => a.h - b.h);
}

/** SVG path through `actualPoints`; empty string when there is no verified sample. */
export function actualPath(g: ChartGeom, pts: { h: number; v: number }[]): string {
  if (pts.length === 0) return "";
  return "M" + pts.map((p) => `${xOf(g, p.h).toFixed(1)},${yOf(g, p.v).toFixed(1)}`).join("L");
}

/** Round-number ticks (1-2-5 per decade) on the log axis between minY and maxY. */
export function logTicks(g: ChartGeom): number[] {
  const out: number[] = [];
  for (let e = Math.floor(Math.log10(g.minY)); e <= Math.ceil(Math.log10(g.maxY)); e++) {
    for (const m of [1, 2, 5]) {
      const v = m * Math.pow(10, e);
      if (v >= g.minY && v <= g.maxY) out.push(v);
    }
  }
  return out;
}
