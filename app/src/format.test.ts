import { describe, expect, it } from "vitest";
import { folderParts, isStale, keyGroups, lastBackupLine, lastBefore, nextBackupLine, relativeTime, sizeLabel, snapshotLabel, inlineTimeLabel, smallSize, dayTime, lastBackupStamp, versionCountLine, plainDate, baseName, restoreFolderName } from "./format";

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

describe("lastBackupLine", () => {
  it("shows relative time from unix seconds", () => {
    const secs = new Date(2026, 9, 24, 14, 50).getTime() / 1000;
    expect(lastBackupLine(secs, now)).toBe("Last backup: 10 minutes ago");
  });
  it("says none yet without a run", () => {
    expect(lastBackupLine(null, now)).toBe("Last backup: none yet");
    expect(lastBackupLine(undefined, now)).toBe("Last backup: none yet");
  });
});

describe("keyGroups", () => {
  it("makes 6 even groups", () => {
    expect(keyGroups("ABCDEFGHIJKLMNOPQRSTUVWX")).toEqual(["ABCD", "EFGH", "IJKL", "MNOP", "QRST", "UVWX"]);
  });
  it("ignores spaces and dashes and spreads the remainder", () => {
    expect(keyGroups("ABCD-EFGH IJ")).toEqual(["AB", "CD", "EF", "GH", "I", "J"]);
  });
  it("copes with empty and very short keys", () => {
    expect(keyGroups("")).toEqual([]);
    expect(keyGroups("AB")).toEqual(["A", "B"]);
  });
});

describe("nextBackupLine", () => {
  const secs = (d: number, h: number, m = 0) => Math.floor(new Date(2026, 9, d, h, m).getTime() / 1000);
  it("counts down to the next hourly run", () => {
    expect(nextBackupLine(secs(24, 14, 50), now)).toBe("Next: in about 50 minutes");
    expect(nextBackupLine(secs(24, 14, 0), now)).toBe("Next: very soon");
    expect(nextBackupLine(secs(24, 10, 0), now)).toBe("Next: very soon");
    expect(nextBackupLine(null, now)).toMatch(/Back up now/);
  });
});

describe("folderParts / isStale", () => {
  it("splits a Windows path", () => {
    expect(folderParts("C:\\Users\\Dad\\Documents")).toEqual({ name: "Documents", where: "C:\\Users\\Dad" });
    expect(folderParts("D:\\Photos\\")).toEqual({ name: "Photos", where: "D:\\" });
  });
  it("flags backups older than a day", () => {
    expect(isStale(Math.floor(new Date(2026, 9, 22).getTime() / 1000), now)).toBe(true);
    expect(isStale(Math.floor(new Date(2026, 9, 24, 14).getTime() / 1000), now)).toBe(false);
    expect(isStale(null, now)).toBe(false);
  });
});

describe("inlineTimeLabel", () => {
  const now = new Date(2026, 9, 24, 21, 0);
  it("lowercases today/yesterday for mid-sentence use", () => {
    expect(inlineTimeLabel(new Date(2026, 9, 24, 20, 41).toISOString(), now)).toBe("today at 8:41 PM");
    expect(inlineTimeLabel(new Date(2026, 9, 23, 15, 0).toISOString(), now)).toBe("yesterday at 3 PM");
  });
});

describe("v0.1.8 plain-word helpers", () => {
  const now = new Date(2026, 9, 25, 15, 0);
  it("smallSize", () => {
    expect(smallSize(532)).toBe("532 bytes");
    expect(smallSize(48 * 1024)).toBe("48 KB");
    expect(smallSize(3.2 * 1024 * 1024)).toBe("3.2 MB");
    expect(smallSize(1.5 * 1024 ** 3)).toBe("1.5 GB");
  });
  it("dayTime and lastBackupStamp", () => {
    expect(dayTime(new Date(2026, 9, 25, 14, 5), now)).toBe("today 14:05");
    expect(dayTime(new Date(2026, 9, 24, 9, 30), now)).toBe("yesterday 09:30");
    expect(dayTime(new Date(2026, 2, 3, 9, 30), now)).toBe("3 March 09:30");
    expect(lastBackupStamp(new Date(2026, 9, 25, 14, 5).getTime() / 1000, now)).toBe("Last backup: today 14:05");
    expect(lastBackupStamp(null, now)).toBe("Last backup: none yet");
  });
  it("versionCountLine, plainDate, baseName, restoreFolderName", () => {
    expect(versionCountLine(1)).toBe("1 saved version");
    expect(versionCountLine(12)).toBe("12 saved versions");
    expect(plainDate(new Date(2026, 9, 24, 9, 30).toISOString(), now)).toBe("yesterday");
    expect(plainDate("", now)).toBe("");
    expect(baseName("C:\\Users\\Dad\\Pictures")).toBe("Pictures");
    expect(baseName("/C/Users/Dad/a.txt")).toBe("a.txt");
    expect(restoreFolderName(now)).toBe("Lifeboat restore 2026-10-25");
  });
});
