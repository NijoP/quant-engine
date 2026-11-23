import { describe, expect, it } from "vitest";
import {
  CAMPAIGN_HOURS,
  actualPath,
  actualPoints,
  chartGeom,
  duration,
  hourlyMultiplier,
  inr,
  istDateTime,
  istHourMinute,
  requiredEquity,
  statusLabel,
  xOf,
  yOf,
} from "./campaign-math";
import { CAMPAIGN_FIXTURE } from "./fixtures/campaign";

// The owner's published 23-hour table (00:00 … 23:00 IST), whole rupees. Regression reference only: the panel computes
// from the formula.
const TABLE = [
  101, 133, 174, 228, 298, 391, 511, 670, 877, 1148, 1503, 1968, 2577, 3375, 4419, 5786, 7576, 9920, 12990, 17009, 22271, 29162,
  38185, 50000,
];

describe("campaign target curve 101.45 × (50000/101.45)^(h/23)", () => {
  it("matches all 24 hourly checkpoints of the published table (nearest rupee)", () => {
    expect(CAMPAIGN_HOURS).toBe(23);
    expect(TABLE).toHaveLength(24);
    TABLE.forEach((v, h) => expect(Math.round(requiredEquity(h))).toBe(v));
  });
  it("starts at ₹101.45 and ends at exactly ₹50,000 at hour 23", () => {
    expect(requiredEquity(0)).toBe(101.45);
    expect(requiredEquity(23)).toBeCloseTo(50000, 6);
    expect(requiredEquity(22)).toBeCloseTo(38185.2212, 3);
  });
  it("compounds at ≈30.9407% per hour", () => {
    expect(hourlyMultiplier()).toBeCloseTo(1.3094071, 7);
    expect((hourlyMultiplier() - 1) * 100).toBeCloseTo(30.9407, 4);
  });
  it("handles partial hours and clamps outside the window", () => {
    expect(requiredEquity(0.5)).toBeCloseTo(101.45 * Math.sqrt(hourlyMultiplier()), 9);
    expect(requiredEquity(0.5)).toBeCloseTo(116.0886, 4);
    expect(requiredEquity(-1)).toBe(101.45);
    expect(requiredEquity(30)).toBeCloseTo(50000, 6);
  });
});

describe("IST formatting", () => {
  it("shows the deadline as 23:00 IST on 24 Nov 2026 whatever the viewer's timezone", () => {
    const deadline = 1795541400000;
    expect(Date.parse("2026-11-24T17:30:00Z")).toBe(deadline);
    expect(istDateTime(deadline)).toBe("24 Nov 2026, 23:00:00 IST");
    expect(istHourMinute(1795458600000 + 3_600_000)).toBe("01:00");
    expect(istHourMinute(Date.parse("2026-11-23T18:30:00Z"))).toBe("00:00");
  });
  it("counts down and reads expired at zero", () => {
    expect(duration(3 * 3600 + 5 * 60 + 7)).toBe("3h 05m 07s");
    expect(duration(0)).toBe("deadline passed");
    expect(duration(null)).toBe("—");
  });
});

describe("money and status labels", () => {
  it("formats ₹ with a true minus and optional plus", () => {
    expect(inr("-1.5")).toBe("−₹1.50");
    expect(inr("2", true)).toBe("+₹2.00");
    expect(inr(null)).toBe("—");
    expect(inr("not a number")).toBe("—");
  });
  it("uses neutral wording for every status", () => {
    expect(statusLabel("behind")).toBe("behind schedule");
    expect(statusLabel("on_track")).toBe("on track");
  });
});

describe("chart", () => {
  const start = Date.parse("2026-11-23T18:30:00Z");
  it("draws only verified samples inside the 23 h window, sorted, never invented", () => {
    const pts = actualPoints(start, [
      { t: "2026-11-23T20:30:00Z", equity_inr: "101" },
      { t: "2026-11-23T18:30:00Z", equity_inr: "100" },
      { t: "2026-11-23T17:00:00Z", equity_inr: "99" }, // before start
      { t: "2026-11-24T17:00:00Z", equity_inr: "98" }, // 22:30 IST: inside the 23 h window
      { t: "2026-11-24T17:31:00Z", equity_inr: "99" }, // after the 23:00 deadline
      { t: "2026-11-23T19:00:00Z", equity_inr: "bad" },
    ]);
    expect(pts).toEqual([
      { h: 0, v: 100 },
      { h: 2, v: 101 },
      { h: 22.5, v: 98 },
    ]);
    const g = chartGeom(95, 50000, pts.map((p) => p.v));
    expect(actualPath(g, [])).toBe("");
    expect(actualPath(g, pts).startsWith("M")).toBe(true);
  });
  it("keeps the ₹95 floor and ₹50,000 target on the axis, target above floor", () => {
    const g = chartGeom(95, 50000, []);
    expect(g.minY).toBeLessThan(95);
    expect(g.maxY).toBeGreaterThan(50000);
    expect(yOf(g, 50000)).toBeLessThan(yOf(g, 95));
    expect(xOf(g, 0)).toBe(g.padL);
    expect(xOf(g, 23)).toBe(g.width - g.padR);
  });
});

describe("dev fixture", () => {
  it("is labelled paper so it can never read as real capital", () => {
    expect(CAMPAIGN_FIXTURE.equity_source).toBe("paper");
    expect(CAMPAIGN_FIXTURE.milestones).toHaveLength(24);
    expect(CAMPAIGN_FIXTURE.start_equity_inr).toBe("101.45");
    expect(Date.parse(CAMPAIGN_FIXTURE.deadline)).toBe(1795541400000);
    expect(CAMPAIGN_FIXTURE.milestones[23].required_inr).toBe("50000.00");
  });
});
