import { invoke } from "@tauri-apps/api/core";

export interface ApiError {
  kind: "not_running" | "service" | "other";
  message: string;
}
export interface StatusInfo {
  version: string;
  frozen: boolean;
  frozen_since: string | null;
}
export interface HomeState {
  status: StatusInfo;
  last_backup: string | null;
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
  restore: (snapshot: string, target: string, password: string) =>
    call<void>("restore", { snapshot, target, password }),
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
  removeFolder: (folder: string) => call<string[]>("remove_folder", { folder }),
};
