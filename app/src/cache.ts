// Tiny in-memory stale-while-revalidate cache plus a "latest wins" guard.
// Panels show cached data at once, refresh in the background, and ignore answers for a screen the user left.

interface Entry<T> {
  value: T;
  at: number;
}

const store = new Map<string, Entry<unknown>>();
const inflight = new Map<string, Promise<unknown>>();

/** What is cached for `key`, if anything. */
export function peek<T>(key: string): T | undefined {
  return store.get(key)?.value as T | undefined;
}

/** Put a value in the cache by hand (for example after a write that returns fresh data). */
export function put<T>(key: string, value: T): void {
  store.set(key, { value, at: Date.now() });
}

/** Forget one key, or every key that starts with `prefix`. */
export function invalidate(prefix?: string): void {
  if (prefix === undefined) return store.clear();
  for (const k of [...store.keys()]) if (k.startsWith(prefix)) store.delete(k);
}

/** Run `fetcher` once per key at a time: callers that arrive while it runs share the same promise. */
export function dedupe<T>(key: string, fetcher: () => Promise<T>): Promise<T> {
  const running = inflight.get(key) as Promise<T> | undefined;
  if (running) return running;
  const p = fetcher().finally(() => inflight.delete(key));
  inflight.set(key, p);
  return p;
}

/** Fetch with the cache: always resolves with fresh data and stores it. */
export function fetchFresh<T>(key: string, fetcher: () => Promise<T>): Promise<T> {
  return dedupe(key, async () => {
    const v = await fetcher();
    put(key, v);
    return v;
  });
}

export interface SwrOptions<T> {
  /** Called with cached data at once (if any), then with fresh data when it differs. */
  onData: (value: T, fromCache: boolean) => void;
  /** Called when the fetch fails and nothing was cached to show. */
  onError?: (e: unknown) => void;
  /** Return false when the caller has moved on: no callback fires after that. */
  alive?: () => boolean;
}

/** Stale-while-revalidate: show cached data now, refresh in the background, redraw only if it changed. */
export function swr<T>(key: string, fetcher: () => Promise<T>, o: SwrOptions<T>): Promise<void> {
  const alive = o.alive ?? (() => true);
  const cached = peek<T>(key);
  const had = cached !== undefined;
  if (had && alive()) o.onData(cached as T, true);
  return fetchFresh(key, fetcher).then(
    (fresh) => {
      if (!alive()) return;
      if (!had || sig(fresh) !== sig(cached)) o.onData(fresh, false);
    },
    (e) => {
      if (alive() && !had) o.onError?.(e);
    },
  );
}

/** A cheap signature for "did this change?". */
export function sig(v: unknown): string {
  try {
    return JSON.stringify(v) ?? "";
  } catch {
    return String(Math.random());
  }
}

/** Latest-wins guard: each `next()` makes every earlier token stale, so slow answers for an old screen are dropped. */
export function guard() {
  let current = 0;
  return {
    next: () => ++current,
    isCurrent: (token: number) => token === current,
    /** Run `fn` and hand its result to `apply` only if no newer `next()` happened meanwhile. */
    async run<T>(fn: () => Promise<T>, apply: (v: T) => void, fail?: (e: unknown) => void): Promise<void> {
      const mine = ++current;
      try {
        const v = await fn();
        if (mine === current) apply(v);
      } catch (e) {
        if (mine === current) fail?.(e);
      }
    },
  };
}
