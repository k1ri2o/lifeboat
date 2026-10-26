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

/** Plain sizes for single files and folders: "532 bytes", "48 KB", "3.2 MB", "1.5 GB". */
export function smallSize(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "";
  if (bytes < 1024) return `${Math.round(bytes)} bytes`;
  const kb = bytes / 1024;
  if (kb < 1024) return `${kb >= 10 ? Math.round(kb) : kb.toFixed(1)} KB`;
  const mb = kb / 1024;
  if (mb < 1024) return `${mb >= 10 ? Math.round(mb) : mb.toFixed(1)} MB`;
  return sizeLabel(bytes);
}

const pad2 = (n: number) => String(n).padStart(2, "0");

/** "today 14:05", "yesterday 09:30", "Tuesday 14:05" or "3 March 14:05". */
export function dayTime(d: Date, now: Date = new Date()): string {
  const hm = `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
  const startOf = (x: Date) => new Date(x.getFullYear(), x.getMonth(), x.getDate()).getTime();
  const days = Math.round((startOf(now) - startOf(d)) / 86400000);
  if (days === 0) return `today ${hm}`;
  if (days === 1) return `yesterday ${hm}`;
  if (days > 1 && days < 7) return `${d.toLocaleDateString("en-GB", { weekday: "long" })} ${hm}`;
  return `${d.getDate()} ${d.toLocaleDateString("en-GB", { month: "long" })}${d.getFullYear() === now.getFullYear() ? "" : " " + d.getFullYear()} ${hm}`;
}

/** "Last backup: today 14:05" (secs = unix seconds). */
export function lastBackupStamp(secs: number | null | undefined, now: Date = new Date()): string {
  if (secs == null) return "Last backup: none yet";
  return `Last backup: ${dayTime(new Date(secs * 1000), now)}`;
}

/** "12 saved versions". */
export function versionCountLine(n: number): string {
  return n === 1 ? "1 saved version" : `${n} saved versions`;
}

/** Date part only, for lists: "today", "yesterday", "3 March 2026". Empty for an unknown date. */
export function plainDate(iso: string, now: Date = new Date()): string {
  const d = new Date(iso);
  if (!iso || Number.isNaN(d.getTime())) return "";
  const t = dayTime(d, now);
  return t.replace(/ \d\d:\d\d$/, "");
}

/** The last piece of a path. */
export function baseName(path: string): string {
  return path.split(/[\\/]+/).filter(Boolean).pop() ?? path;
}

/** Folder name for a new restore: "Lifeboat restore 2026-10-25". */
export function restoreFolderName(now: Date = new Date()): string {
  return `Lifeboat restore ${now.getFullYear()}-${pad2(now.getMonth() + 1)}-${pad2(now.getDate())}`;
}

/** "Last backup: 10 minutes ago", or a plain note when there has been none. `secs` = unix seconds. */
export function lastBackupLine(secs: number | null | undefined, now: Date = new Date()): string {
  if (secs == null) return "Last backup: none yet";
  return `Last backup: ${relativeTime(new Date(secs * 1000).toISOString(), now)}`;
}

/** Whole percent (0-100) of a drive that is used; 0 when the numbers are missing. */
export function usagePercent(used: number | null | undefined, total: number | null | undefined): number {
  if (used == null || !total || total <= 0) return 0;
  return Math.max(0, Math.min(100, Math.round((used / total) * 100)));
}

/** The bar turns to a warning colour above this share of the space used. */
export const USAGE_WARN_PERCENT = 85;

export function usageIsHigh(percent: number): boolean {
  return percent > USAGE_WARN_PERCENT;
}

/** "38 GB used, 64 GB free" in plain words. */
export function usageLine(used: number | null | undefined, free: number | null | undefined): string {
  if (used == null || free == null) return "Plug in and unlock the drive to see how much space is left.";
  return `${sizeLabel(used)} used, ${sizeLabel(free)} free`;
}

/**
 * A plain-English name for a drive's connection type, as Windows reports it.
 * It never guesses "inside this computer": some USB boxes report SATA.
 */
export function busLabel(bus: string): string {
  if (/usb/i.test(bus)) return "USB drive";
  if (/nvme/i.test(bus)) return "NVMe drive";
  if (/sata|^ata$/i.test(bus)) return "SATA drive";
  return "Drive";
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

/** The scheduled backup runs about once an hour. */
export const BACKUP_INTERVAL_MIN = 60;

/** "Next: in about 50 minutes". */
export function nextBackupLine(lastRunSecs: number | null | undefined, now: Date = new Date()): string {
  if (lastRunSecs == null) return "Next: as soon as you press Back up now";
  const dueMin = Math.round((lastRunSecs * 1000 + BACKUP_INTERVAL_MIN * 60000 - now.getTime()) / 60000);
  if (dueMin <= 1) return "Next: very soon";
  if (dueMin < 60) return `Next: in about ${dueMin} minutes`;
  const h = Math.round(dueMin / 60);
  return `Next: in about ${h} ${h === 1 ? "hour" : "hours"}`;
}

/** A folder's last part ("Documents") and where it lives, for a friendly list. */
export function folderParts(path: string): { name: string; where: string } {
  const clean = path.replace(/[\\/]+$/, "");
  const i = Math.max(clean.lastIndexOf("\\"), clean.lastIndexOf("/"));
  if (i < 0) return { name: clean || path, where: "" };
  const name = clean.slice(i + 1) || clean;
  const where = clean.slice(0, i);
  return { name, where: /^[A-Za-z]:$/.test(where) ? where + "\\" : where };
}

/** A last backup older than a day means something is probably wrong. */
export function isStale(lastRunSecs: number | null | undefined, now: Date = new Date()): boolean {
  if (lastRunSecs == null) return false;
  return now.getTime() - lastRunSecs * 1000 > 24 * 3600 * 1000;
}

/** Same as snapshotLabel but for use inside a sentence: "today at 3 PM", "on Tuesday at 3 PM". */
export function inlineTimeLabel(iso: string, now: Date = new Date()): string {
  const label = snapshotLabel(iso, now);
  const m = /^(Today|Yesterday) (.+)$/.exec(label);
  if (m) return `${m[1].toLowerCase()} at ${m[2]}`;
  const d = /^([A-Z][a-z]+day) (.+)$/.exec(label);
  if (d) return `on ${d[1]} at ${d[2]}`;
  return label === "Unknown time" ? "an unknown time" : `on ${label}`;
}

const BASIC_DATA = "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7";
const MIN_LIFEBOAT_SIZE = 512 * 1024 * 1024;

/** Does this drive already hold a Lifeboat partition? Mirrors PartitionInfo::is_lifeboat in the service. */
export function hasLifeboatPartition(d: {
  partitions: { size: number; gpt_type: string | null; drive_letter: string | null; is_hidden?: boolean; no_default_letter?: boolean }[];
}): boolean {
  return d.partitions.some(
    (p) =>
      (p.gpt_type ?? "").replace(/[{}]/g, "").toLowerCase() === BASIC_DATA &&
      p.size >= MIN_LIFEBOAT_SIZE &&
      !!(p.is_hidden || p.no_default_letter) &&
      !p.drive_letter,
  );
}

/** "Made on OLD-PC" for a backup that names the PC it came from, else empty. */
export function madeOnNote(hostname: string | undefined): string {
  const h = (hostname ?? "").trim();
  return h ? `Made on ${h}` : "";
}

/** Days between asking for a delete and it running (matches the service). */
export const DELETE_DELAY_DAYS = 7;

/** "25 October 2026" for a date. */
export function longDate(d: Date): string {
  return `${d.getDate()} ${MONTHS[d.getMonth()]} ${d.getFullYear()}`;
}

/** The date a delete asked for now will run. */
export function deleteDueDate(now: Date = new Date()): Date {
  return new Date(now.getFullYear(), now.getMonth(), now.getDate() + DELETE_DELAY_DAYS);
}

/** The plain-words confirmation shown before a delete is queued. */
export function deleteConfirmText(now: Date = new Date()): string {
  return `This backup will be deleted on ${longDate(deleteDueDate(now))}. You can cancel until then.`;
}

/** The plain-words confirmation shown before a vault item removal is queued. */
export function vaultRemoveConfirmText(now: Date = new Date()): string {
  return `It will be removed from the vault on ${longDate(deleteDueDate(now))}. You can cancel until then.`;
}

/** "Removes on 1 November 2026" for a queued vault removal (`dueSecs` in unix seconds). */
export function removesOnLine(dueSecs: number): string {
  const d = new Date(dueSecs * 1000);
  return Number.isNaN(d.getTime()) ? "Will be removed soon" : `Removes on ${longDate(d)}`;
}

/** Title for the waiting-deletes bar, counting backups and vault items. */
export function waitingDeletesTitle(backups: number, items: number): string {
  const b = backups === 1 ? "1 backup" : `${backups} backups`;
  const i = items === 1 ? "1 vault item" : `${items} vault items`;
  const what = backups && items ? `${b} and ${i}` : backups ? b : i;
  return `${what} will be ${items && !backups ? "removed" : "deleted"}`;
}

export interface Pending {
  id: string;
  due: number;
}

/** The queued delete for backup `id`, or undefined. */
export function pendingFor<T extends Pending>(pending: T[] | undefined, id: string): T | undefined {
  return (pending ?? []).find((p) => p.id === id);
}

/** "Deletes on 1 November 2026" for a queued delete (`due` in unix seconds). */
export function deletesOnLine(dueSecs: number): string {
  const d = new Date(dueSecs * 1000);
  return Number.isNaN(d.getTime()) ? "Will be deleted soon" : `Deletes on ${longDate(d)}`;
}

const GIB = 1024 ** 3;

/** How full the Lifeboat partition is, 0-100, or null when the used size is unknown. */
export function usedPercent(used: number | null | undefined, total: number): number | null {
  if (used == null || !(total > 0)) return null;
  return Math.min(100, Math.max(0, Math.round((used / total) * 100)));
}

/** True above 85% full. */
export function isNearlyFull(used: number | null | undefined, total: number): boolean {
  const p = usedPercent(used, total);
  return p != null && p > 85;
}

/** Whole GB that can be picked when growing: from 1 GB more than now up to the most that fits. */
export function growRangeGb(total: number, max: number): { min: number; max: number } | null {
  const min = Math.floor(total / GIB) + 1;
  const top = Math.floor(max / GIB);
  return top >= min ? { min, max: top } : null;
}

/** Turn the size typed in the "Make bigger" dialog into bytes, or say what is wrong. */
export function parseGrowGb(text: string, total: number, max: number): { bytes: number } | { error: string } {
  const range = growRangeGb(total, max);
  if (!range) return { error: "There is no room to grow." };
  const gb = Number(text.trim().replace(",", "."));
  if (!text.trim() || !Number.isFinite(gb)) return { error: "Type the new size in GB, for example 200." };
  if (gb < range.min) return { error: `Pick at least ${range.min} GB. Lifeboat can only grow, not shrink.` };
  if (gb > range.max) return { error: `Pick at most ${range.max} GB. That is all the free space there is.` };
  return { bytes: Math.round(gb * GIB) };
}

/** How "Make bigger" can work for a given Lifeboat size report (#54, #62). */
export type GrowOption = { mode: "grow" | "move"; max: number } | { mode: "none"; reason: string };

export const MOVE_GROW_TEXT =
  "Lifeboat will make a bigger copy of itself in space taken from the drive in front of it, check the copy, then remove the old one. Your backups stay safe the whole time. This can take a while.";

/** Grow in place when there is free space right behind Lifeboat, else by moving, else say why not. */
export function growOption(info: { total: number; max_size: number; can_grow: boolean; can_move?: boolean; move_max?: number; reason?: string }): GrowOption {
  if (info.can_grow && growRangeGb(info.total, info.max_size)) return { mode: "grow", max: info.max_size };
  if (info.can_move && growRangeGb(info.total, info.move_max ?? 0)) return { mode: "move", max: info.move_max ?? 0 };
  return { mode: "none", reason: info.reason || "The drive in front of Lifeboat has no room left to give." };
}

/** The word that must be typed to delete Lifeboat. */
export const DELETE_WORD = "DELETE";

/** True when the typed text is exactly the confirm word (case matters). */
export function deleteWordOk(text: string): boolean {
  return text === DELETE_WORD;
}
