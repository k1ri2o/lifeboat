import { invoke } from "@tauri-apps/api/core";

export interface ApiError {
  kind: "not_running" | "service" | "other";
  message: string;
}
export interface StatusInfo {
  version: string;
  frozen: boolean;
  frozen_since: string | null;
  /** Unix seconds of the last successful backup. */
  last_run: number | null;
  /** Backups waiting to be deleted (id and due time in unix seconds). */
  pending_deletes?: PendingDelete[];
  /** Vault items waiting to be removed (path and due time in unix seconds). */
  pending_vault_removes?: PendingVaultRemove[];
}
export interface PendingVaultRemove {
  path: string;
  due: number;
}
export interface PendingDelete {
  id: string;
  due: number;
}
export interface HomeState {
  status: StatusInfo;
  folders: string[];
}
export interface Snapshot {
  id: string;
  short_id: string;
  time: string;
  paths: string[];
  /** Name of the PC that made this backup (empty when unknown). */
  hostname?: string;
}
export interface Partition {
  number: number;
  offset: number;
  size: number;
  gpt_type: string | null;
  drive_letter: string | null;
  is_hidden?: boolean;
  no_default_letter?: boolean;
}
export interface Disk {
  number: number;
  model: string;
  size: number;
  bus_type: string;
  is_boot: boolean;
  is_system: boolean;
  partitions: Partition[];
}

export interface DriveDisk {
  number: number;
  model: string;
  size: number;
  bus_type: string;
  is_system: boolean;
  letters: string[];
  holds_lifeboat: boolean;
}
export interface DriveCheck {
  time: string;
  ok: boolean;
  detail: string;
}
/** The Lifeboat partition. Numbers are null when the drive is closed or offline. */
export interface LifeboatDrive {
  disk_number: number | null;
  partition_size: number | null;
  used_bytes: number | null;
  free_bytes: number | null;
  repo_bytes: number | null;
  snapshot_count: number | null;
  vault_count: number | null;
  same_physical_disk: boolean;
  last_check: DriveCheck | null;
  open: boolean;
}
export interface DriveStats {
  disks: DriveDisk[];
  lifeboat: LifeboatDrive | null;
}

export interface FileEntry {
  path: string;
  name: string;
  dir: boolean;
  size: number;
  /** ISO time the file was last changed, or empty when unknown. */
  mtime: string;
}
export interface VaultItem {
  path: string;
  dir: boolean;
  size: number;
  /** ISO time it was put in the vault. */
  added: string;
  snapshot: string;
  /** Unix seconds the item will be removed, when a removal is queued. */
  removes_at?: number | null;
}

/** Turn whatever invoke rejected with into an ApiError. */
export function toApiError(e: unknown): ApiError {
  if (e && typeof e === "object" && "message" in e && "kind" in e) return e as ApiError;
  return { kind: "other", message: typeof e === "string" ? e : "Something went wrong." };
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (e) {
    throw toApiError(e);
  }
}

export const api = {
  homeState: () => call<HomeState>("home_state"),
  backupNow: () => call<void>("backup_now"),
  snapshots: () => call<Snapshot[]>("list_snapshots"),
  /** Every disk plus the Lifeboat partition's usage. Read-only, needs no password. */
  driveStats: () => call<DriveStats>("drive_stats"),
  /** Restore `paths` of a version (empty list = everything). Resolves to the folder used. */
  restore: (snapshot: string, target: string, password: string, paths: string[] = []) =>
    call<string>("restore", { snapshot, target, password, paths }),
  /** Queue backups for deletion (the service waits 7 days). */
  deleteSnapshots: (password: string, ids: string[]) => call<void>("delete_snapshots", { password, ids }),
  /** Cancel every queued delete. */
  cancelDelete: (password: string) => call<void>("cancel_delete", { password }),
  /** One level of a version's files. `path` omitted = the top. */
  listFiles: (snapshot: string, path?: string) =>
    call<FileEntry[]>("list_files", { snapshot, path: path ?? null }),
  vaultAdd: (password: string, paths: string[]) => call<number>("vault_add", { password, paths }),
  vaultRemove: (password: string, paths: string[]) => call<void>("vault_remove", { password, paths }),
  vaultList: () => call<VaultItem[]>("vault_list"),
  vaultExtract: (password: string, paths: string[], target: string) =>
    call<string>("vault_extract", { password, paths, target }),
  openFolder: (path: string) => call<void>("open_folder", { path }),
  isAdmin: () => call<boolean>("is_admin"),
  relaunchAsAdmin: () => call<void>("relaunch_as_admin"),
  disks: () => call<Disk[]>("list_disks"),
  prepareDisk: (
    disk: number,
    password: string,
    internal?: { driveLetter: string; sizeGb: number },
  ) =>
    call<string>("prepare_disk", {
      disk,
      password,
      internalDriveLetter: internal?.driveLetter ?? null,
      internalSizeGb: internal?.sizeGb ?? null,
    }),
  addFolder: (folder: string) => call<string[]>("add_folder", { folder }),
  /** Removing a folder needs the Lifeboat password. */
  removeFolder: (folder: string, password: string) =>
    call<string[]>("remove_folder", { folder, password }),
  initRepo: (password: string, folders: string[], fresh = false) =>
    call<string>("init_repo", { password, folders, fresh }),
};
