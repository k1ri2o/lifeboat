import { describe, expect, it } from "vitest";
import { friendlyError } from "./friendly";

const BANNED = /snapshot|repo|restic|canary|prune|retention|freeze|pipe|service|os error/i;

describe("friendlyError", () => {
  it("maps Windows os error 3 and keeps raw text as details", () => {
    const f = friendlyError({ kind: "other", message: "The system cannot find the path specified. (os error 3)" });
    expect(f.title).toMatch(/could not find that folder or drive/);
    expect(f.details).toContain("os error 3");
    expect(f.title + f.next).not.toMatch(BANNED);
  });
  it("maps a missing helper to a restart hint", () => {
    const f = friendlyError({ kind: "not_running", message: "Lifeboat's background service isn't running." });
    expect(f.unreachable).toBe(true);
    expect(f.next).toMatch(/Restart your computer/);
    expect(f.title + f.next).not.toMatch(BANNED);
  });
  it("maps pipe errors even when kind is other", () => {
    expect(friendlyError({ kind: "other", message: "pipe not reachable" }).unreachable).toBe(true);
  });
  it("maps administrator and password problems", () => {
    expect(friendlyError({ kind: "service", message: "needs administrator" }).title).toMatch(/administrator/);
    expect(friendlyError({ kind: "service", message: "That password is not right. Please try again." }).title).toMatch(
      /password is not right/,
    );
    expect(friendlyError({ kind: "service", message: "locked for 15 minutes" }).title).toMatch(/Too many/);
  });
  it("maps disk full", () => {
    expect(friendlyError("There is not enough space on the disk. (os error 112)").title).toMatch(/not enough room/);
  });
  it("passes through short plain sentences", () => {
    const f = friendlyError({ kind: "service", message: "Choose a folder first." });
    expect(f.title).toBe("Choose a folder first.");
    expect(f.details).toBe("");
  });
  it("hides unknown technical text behind details", () => {
    const f = friendlyError({ kind: "other", message: "panic at 0xDEADBEEF in module::thing" });
    expect(f.title).toBe("Something went wrong.");
    expect(f.details).toContain("0xDEADBEEF");
  });
});
