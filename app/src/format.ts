// Plain-words time helpers. No jargon, no seconds.

const DAYS = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS = [
  "January", "February", "March", "April", "May", "June",
  "July", "August", "September", "October", "November", "December",
];

function hour12(d: Date): string {
  const h = d.getHours();
  const m = d.getMinutes();
  const h12 = h % 12 === 0 ? 12 : h % 12;
  const ap = h < 12 ? "AM" : "PM";
  return m === 0 ? `${h12} ${ap}` : `${h12}:${String(m).padStart(2, "0")} ${ap}`;
}

function startOfDay(d: Date): number {
  return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
}

/** "just now", "10 minutes ago", "3 hours ago", "yesterday", "5 days ago". */
export function relativeTime(iso: string, now: Date = new Date()): string {
  const t = new Date(iso);
  const diff = now.getTime() - t.getTime();
  if (Number.isNaN(diff)) return "at an unknown time";
  const min = Math.floor(diff / 60000);
  if (min < 1) return "just now";
  if (min === 1) return "1 minute ago";
  if (min < 60) return `${min} minutes ago`;
  const hours = Math.floor(min / 60);
  const dayGap = Math.round((startOfDay(now) - startOfDay(t)) / 86400000);
  if (dayGap === 0) return hours === 1 ? "1 hour ago" : `${hours} hours ago`;
  if (dayGap === 1) return "yesterday";
  return `${dayGap} days ago`;
}

/** "Today 3 PM", "Yesterday 3 PM", "Tuesday 3 PM" (within a week), else "12 March, 3 PM". */
export function snapshotLabel(iso: string, now: Date = new Date()): string {
  const t = new Date(iso);
  if (Number.isNaN(t.getTime())) return "Unknown time";
  const dayGap = Math.round((startOfDay(now) - startOfDay(t)) / 86400000);
  const time = hour12(t);
  if (dayGap === 0) return `Today ${time}`;
  if (dayGap === 1) return `Yesterday ${time}`;
  if (dayGap > 1 && dayGap < 7) return `${DAYS[t.getDay()]} ${time}`;
  return `${t.getDate()} ${MONTHS[t.getMonth()]}, ${time}`;
}

export interface Snap {
  id: string;
  time: string;
}

/** The newest snapshot taken strictly before `since`, or undefined. */
export function lastBefore<T extends Snap>(snaps: T[], since: string): T | undefined {
  const limit = new Date(since).getTime();
  if (Number.isNaN(limit)) return undefined;
  return snaps
    .filter((s) => new Date(s.time).getTime() < limit)
    .sort((a, b) => new Date(b.time).getTime() - new Date(a.time).getTime())[0];
}

/** "1.5 GB" style sizes. */
export function sizeLabel(bytes: number): string {
  const gb = bytes / 1024 ** 3;
  if (gb >= 1000) return `${(gb / 1024).toFixed(1)} TB`;
  if (gb >= 10) return `${Math.round(gb)} GB`;
  return `${gb.toFixed(1)} GB`;
}

/** "Last backup: 10 minutes ago", or a plain note when there has been none. `secs` = unix seconds. */
export function lastBackupLine(secs: number | null | undefined, now: Date = new Date()): string {
  if (secs == null) return "Last backup: none yet";
  return `Last backup: ${relativeTime(new Date(secs * 1000).toISOString(), now)}`;
}

/**
 * Split a recovery key into 6 groups for easy copying by hand. Spaces and dashes in the key
 * are ignored. Groups are as even as possible, earlier groups longer.
 */
export function keyGroups(key: string, groups = 6): string[] {
  const clean = key.replace(/[\s-]+/g, "");
  if (!clean) return [];
  const size = Math.floor(clean.length / groups);
  const extra = clean.length % groups;
  const out: string[] = [];
  let pos = 0;
  for (let i = 0; i < groups; i++) {
    const len = size + (i < extra ? 1 : 0);
    if (len === 0) break;
    out.push(clean.slice(pos, pos + len));
    pos += len;
  }
  return out;
}
