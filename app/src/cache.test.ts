import { beforeEach, describe, expect, it, vi } from "vitest";
import { dedupe, fetchFresh, guard, invalidate, peek, put, swr } from "./cache";

const defer = <T>() => {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((a, b) => ((resolve = a), (reject = b)));
  return { promise, resolve, reject };
};

beforeEach(() => invalidate());

describe("cache", () => {
  it("stores and invalidates by prefix", () => {
    put("a:1", 1);
    put("a:2", 2);
    put("b", 3);
    invalidate("a:");
    expect(peek("a:1")).toBeUndefined();
    expect(peek("b")).toBe(3);
  });

  it("dedupes calls that overlap", async () => {
    const d = defer<number>();
    const fn = vi.fn(() => d.promise);
    const p1 = dedupe("k", fn);
    const p2 = dedupe("k", fn);
    d.resolve(7);
    expect(await p1).toBe(7);
    expect(await p2).toBe(7);
    expect(fn).toHaveBeenCalledTimes(1);
    await dedupe("k", async () => 8);
  });

  it("fetchFresh stores the result", async () => {
    await fetchFresh("x", async () => "v");
    expect(peek("x")).toBe("v");
  });
});

describe("swr", () => {
  it("shows cached data first, then fresh data only if it changed", async () => {
    put("s", { n: 1 });
    const seen: [unknown, boolean][] = [];
    await swr("s", async () => ({ n: 2 }), { onData: (v, c) => seen.push([v, c]) });
    expect(seen).toEqual([
      [{ n: 1 }, true],
      [{ n: 2 }, false],
    ]);
  });

  it("does not redraw when fresh data equals cached", async () => {
    put("s", { n: 1 });
    const on = vi.fn();
    await swr("s", async () => ({ n: 1 }), { onData: on });
    expect(on).toHaveBeenCalledTimes(1);
  });

  it("reports an error only when nothing was cached", async () => {
    const err = vi.fn();
    await swr("e", async () => Promise.reject(new Error("x")), { onData: vi.fn(), onError: err });
    expect(err).toHaveBeenCalledTimes(1);
    put("e", 1);
    await swr("e", async () => Promise.reject(new Error("x")), { onData: vi.fn(), onError: err });
    expect(err).toHaveBeenCalledTimes(1);
  });

  it("stays quiet once the caller has left", async () => {
    let alive = true;
    const on = vi.fn();
    const d = defer<number>();
    const p = swr("l", () => d.promise, { onData: on, alive: () => alive });
    alive = false;
    d.resolve(1);
    await p;
    expect(on).not.toHaveBeenCalled();
    expect(peek("l")).toBe(1);
  });
});

describe("guard", () => {
  it("drops the answer of a request that a newer one replaced", async () => {
    const g = guard();
    const slow = defer<string>();
    const applied: string[] = [];
    const first = g.run(() => slow.promise, (v) => applied.push(v));
    await g.run(async () => "new", (v) => applied.push(v));
    slow.resolve("old");
    await first;
    expect(applied).toEqual(["new"]);
  });

  it("drops errors of stale requests too", async () => {
    const g = guard();
    const slow = defer<string>();
    const fail = vi.fn();
    const first = g.run(() => slow.promise, vi.fn(), fail);
    g.next();
    slow.reject(new Error("late"));
    await first;
    expect(fail).not.toHaveBeenCalled();
  });

  it("isCurrent follows next()", () => {
    const g = guard();
    const t = g.next();
    expect(g.isCurrent(t)).toBe(true);
    g.next();
    expect(g.isCurrent(t)).toBe(false);
  });
});
