import { describe, expect, it } from "vitest";
import {
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

// The owner's published table, rounded to whole rupees. Regression reference only: the panel computes from the formula.
const TABLE = [
  100, 133, 176, 233, 310, 411, 545, 722, 958, 1271, 1686, 2236, 2966, 3934, 5218, 6922, 9181, 12178, 16153, 21425, 28419, 37696, 50000,
];

describe("campaign target curve 100 × 500^(h/22)", () => {
  it("matches all 23 hourly checkpoints of the published table (nearest rupee)", () => {
    expect(TABLE).toHaveLength(23);
    TABLE.forEach((v, h) => expect(Math.round(requiredEquity(h))).toBe(v));
  });
  it("starts at ₹100 and ends at ₹50,000", () => {
    expect(requiredEquity(0)).toBe(100);
    expect(requiredEquity(22)).toBeCloseTo(50000, 6);
  });
  it("compounds at ≈32.6418% per hour", () => {
    expect(hourlyMultiplier()).toBeCloseTo(1.326418, 6);
    expect((hourlyMultiplier() - 1) * 100).toBeCloseTo(32.6418, 4);
  });
  it("handles partial hours and clamps outside the window", () => {
    expect(requiredEquity(0.5)).toBeCloseTo(100 * Math.sqrt(hourlyMultiplier()), 9);
    expect(requiredEquity(-1)).toBe(100);
    expect(requiredEquity(30)).toBeCloseTo(50000, 6);
  });
});

describe("IST formatting", () => {
  it("shows the deadline as 22:00 IST on 24 Nov 2026 whatever the viewer's timezone", () => {
    const deadline = Date.parse("2026-11-24T16:30:00Z");
    expect(istDateTime(deadline)).toBe("24 Nov 2026, 22:00:00 IST");
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
  it("draws only verified samples inside the 22 h window, sorted, never invented", () => {
    const pts = actualPoints(start, [
      { t: "2026-11-23T20:30:00Z", equity_inr: "101" },
      { t: "2026-11-23T18:30:00Z", equity_inr: "100" },
      { t: "2026-11-23T17:00:00Z", equity_inr: "99" }, // before start
      { t: "2026-11-24T17:00:00Z", equity_inr: "99" }, // after deadline
      { t: "2026-11-23T19:00:00Z", equity_inr: "bad" },
    ]);
    expect(pts).toEqual([
      { h: 0, v: 100 },
      { h: 2, v: 101 },
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
    expect(xOf(g, 22)).toBe(g.width - g.padR);
  });
});

describe("dev fixture", () => {
  it("is labelled paper so it can never read as real capital", () => {
    expect(CAMPAIGN_FIXTURE.equity_source).toBe("paper");
    expect(CAMPAIGN_FIXTURE.milestones).toHaveLength(23);
  });
});
