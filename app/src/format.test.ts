import { describe, expect, it } from "vitest";
import { lastBefore, relativeTime, sizeLabel, snapshotLabel } from "./format";

// Local-time dates so the tests do not depend on the machine's time zone.
const now = new Date(2026, 9, 24, 15, 0, 0); // Saturday 24 Oct 2026, 3 PM
const at = (d: number, h: number, m = 0) => new Date(2026, 9, d, h, m).toISOString();

describe("relativeTime", () => {
  it("handles minutes, hours and days", () => {
    expect(relativeTime(at(24, 14, 59), now)).toBe("1 minute ago");
    expect(relativeTime(at(24, 14, 50), now)).toBe("10 minutes ago");
    expect(relativeTime(at(24, 15, 0), now)).toBe("just now");
    expect(relativeTime(at(24, 12, 0), now)).toBe("3 hours ago");
    expect(relativeTime(at(23, 20, 0), now)).toBe("yesterday");
    expect(relativeTime(at(19, 9, 0), now)).toBe("5 days ago");
  });
  it("copes with garbage", () => {
    expect(relativeTime("nope", now)).toBe("at an unknown time");
  });
});

describe("snapshotLabel", () => {
  it("names today, yesterday and weekdays", () => {
    expect(snapshotLabel(at(24, 15), now)).toBe("Today 3 PM");
    expect(snapshotLabel(at(23, 9, 30), now)).toBe("Yesterday 9:30 AM");
    expect(snapshotLabel(at(20, 15), now)).toBe("Tuesday 3 PM");
    expect(snapshotLabel(at(24, 0), now)).toBe("Today 12 AM");
  });
  it("uses a date when older than a week", () => {
    expect(snapshotLabel(at(10, 15), now)).toBe("10 October, 3 PM");
  });
});

describe("lastBefore", () => {
  const snaps = [
    { id: "a", time: at(22, 10) },
    { id: "b", time: at(23, 10) },
    { id: "c", time: at(24, 10) },
  ];
  it("picks the newest snapshot before the alert", () => {
    expect(lastBefore(snaps, at(23, 18))?.id).toBe("b");
  });
  it("returns nothing when none is older", () => {
    expect(lastBefore(snaps, at(21, 0))).toBeUndefined();
  });
});

describe("sizeLabel", () => {
  it("formats sizes", () => {
    expect(sizeLabel(500 * 1024 ** 3)).toBe("500 GB");
    expect(sizeLabel(2 * 1024 ** 4)).toBe("2.0 TB");
  });
});
