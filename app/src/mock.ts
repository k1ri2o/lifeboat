// Dev-only fake backend so every screen can be previewed in a plain browser:
//   http://localhost:1420/?mock=ok|attention|attack|firstrun|error[&screen=restore|settings]
// Only imported under import.meta.env.DEV, so it never reaches a production build.
import { mockIPC } from "@tauri-apps/api/mocks";

export async function installMock(mode: string): Promise<void> {
  const now = Math.floor(Date.now() / 1000);
  const iso = (minsAgo: number) => new Date((now - minsAgo * 60) * 1000).toISOString();
  let folders =
    mode === "firstrun"
      ? []
      : ["C:\\Users\\Dad\\Documents", "C:\\Users\\Dad\\Pictures", "D:\\Family videos\\Holidays 2024"];
  const attackAt = mode === "attack" ? iso(95) : null;
  const lastRun = mode === "ok" ? now - 10 * 60 : mode === "attack" ? now - 3 * 3600 : mode === "attention" ? now - 3 * 86400 : null;
  const versions = [30, 70, 130, 25 * 60, 49 * 60, 80 * 60, 200 * 60, 400 * 60, 700 * 60, 900 * 60].map((m, i) => ({
    id: `v${i}full`,
    short_id: `v${i}`,
    time: iso(m),
    paths: folders,
  }));
  const disks = [
    { number: 2, model: "SanDisk Ultra USB", size: 500_000_000_000, bus_type: "USB", is_boot: false, is_system: false, partitions: [] },
    { number: 1, model: "Samsung SSD 870", size: 1_000_000_000_000, bus_type: "SATA", is_boot: false, is_system: false, partitions: [] },
  ];
  const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

  mockIPC(async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    if (cmd.startsWith("plugin:event")) return 0;
    if (cmd === "plugin:dialog|open") return "C:\\Users\\Dad\\Desktop\\New folder";
    if (mode === "error" && cmd === "home_state") {
      throw { kind: "not_running", message: "pipe not reachable: The system cannot find the file specified. (os error 2)" };
    }
    switch (cmd) {
      case "home_state":
        return { status: { version: "0.1.1", frozen: mode === "attack", frozen_since: attackAt, last_run: lastRun }, folders };
      case "backup_now":
        await wait(1200);
        return null;
      case "list_snapshots":
        return versions;
      case "list_disks":
        return disks;
      case "prepare_disk":
        await wait(800);
        return "ok";
      case "init_repo":
        await wait(800);
        return "ABCD1234EFGH5678JKLM9012NPQR3456STUV7890WXYZ2345";
      case "add_folder":
        folders = [...folders, String(a.folder)];
        return folders;
      case "remove_folder":
        if (a.password !== "password") throw { kind: "service", message: "That password is not right. Please try again." };
        folders = folders.filter((f) => f !== a.folder);
        return folders;
      case "restore":
        await wait(800);
        return null;
      default:
        return null;
    }
  });
}
