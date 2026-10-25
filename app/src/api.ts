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
}
export interface Partition {
  number: number;
  offset: number;
  size: number;
  gpt_type: string | null;
  drive_letter: string | null;
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
  /** Restore `paths` of a version (empty list = everything). Resolves to the folder used. */
  restore: (snapshot: string, target: string, password: string, paths: string[] = []) =>
    call<string>("restore", { snapshot, target, password, paths }),
  /** One level of a version's files. `path` omitted = the top. */
  listFiles: (snapshot: string, path?: string) =>
    call<FileEntry[]>("list_files", { snapshot, path: path ?? null }),
  vaultAdd: (password: string, paths: string[]) => call<number>("vault_add", { password, paths }),
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
  initRepo: (password: string, folders: string[]) =>
    call<string>("init_repo", { password, folders }),
};
