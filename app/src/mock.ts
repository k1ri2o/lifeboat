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
  let vault: { path: string; dir: boolean; size: number; added: string; snapshot: string; removes_at?: number }[] = [
    { path: "C:\\Users\\Dad\\Documents\\Will.pdf", dir: false, size: 210 * 1024, added: iso(2000), snapshot: "v3" },
  ];
  const G = 1024 ** 3;
  // ?size=full shows the 85% warning, ?size=nogrow offers the move-and-grow route, ?size=nospace shows why nothing is possible.
  const sizeMode = new URLSearchParams(location.search).get("size");
  let lbTotal = 100 * G;
  const lbUsed = () => (sizeMode === "full" ? 92 * G : 31 * G);
  const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

  mockIPC(async (cmd, args) => {
    const a = (args ?? {}) as Record<string, unknown>;
    if (cmd.startsWith("plugin:event")) return 0;
    if (cmd === "plugin:dialog|open") return "C:\\Users\\Dad\\Desktop\\New folder";
    // Realistic service latency (300-1500 ms; add ?lag=0 for none) so loading states and races show up.
    await wait(new URLSearchParams(location.search).get("lag") === "0" ? 0 : 300 + Math.random() * 1200);
    if (mode === "error" && cmd === "home_state") {
      throw { kind: "not_running", message: "pipe not reachable: The system cannot find the file specified. (os error 2)" };
    }
    switch (cmd) {
      case "home_state":
        return { status: { version: "0.1.1", frozen: mode === "attack", frozen_since: attackAt, last_run: lastRun }, folders };
      case "backup_now":
        await wait(1500);
        return null;
      case "list_snapshots":
        return versions;
      case "drive_stats":
        await wait(300);
        return {
          disks: [
            { number: 0, model: "Samsung SSD 980", size: 500_107_862_016, bus_type: "NVMe", is_system: true, letters: ["C"], holds_lifeboat: mode !== "firstrun" },
            { number: 2, model: "SanDisk Ultra USB", size: 500_000_000_000, bus_type: "USB", is_system: false, letters: ["E"], holds_lifeboat: false },
          ],
          lifeboat:
            mode === "firstrun"
              ? null
              : {
                  disk_number: 0,
                  partition_size: 107_374_182_400,
                  used_bytes: mode === "attention" ? 94_489_280_512 : 38_654_705_664,
                  free_bytes: mode === "attention" ? 12_884_901_888 : 68_719_476_736,
                  repo_bytes: mode === "attention" ? 94_000_000_000 : 38_000_000_000,
                  snapshot_count: versions.length,
                  vault_count: 12,
                  same_physical_disk: true,
                  last_check: { time: iso(60 * 20), ok: true, detail: "no errors were found" },
                  open: true,
                },
        };
      case "list_disks":
        return disks;
      case "prepare_disk":
        await wait(500);
        return "ok";
      case "init_repo":
        await wait(500);
        return "ABCD1234EFGH5678JKLM9012NPQR3456STUV7890WXYZ2345";
      case "add_folder":
        folders = [...folders, String(a.folder)];
        return folders;
      case "remove_folder":
        if (a.password !== "password") throw { kind: "service", message: "That password is not right. Please try again." };
        folders = folders.filter((f) => f !== a.folder);
        return folders;
      case "restore":
        if (a.password !== "password") throw { kind: "service", message: "That password is not right. Please try again." };
        await wait(500);
        return String(a.target);
      case "list_files": {
        const base = a.path ? String(a.path).replace(/[\/]+$/, "") : "";
        // Same shape as the service: restic paths (/C/Users/Dad/Documents), display name in Windows form for top folders.
        const e = (name: string, dir: boolean, size: number) => ({
          path: `${base}/${name}`,
          name,
          dir,
          size,
          mtime: iso(300),
        });
        if (!base) return folders.map((f) => ({ path: "/" + f.replace(":", "").split("\\").join("/"), name: f, dir: true, size: 0, mtime: iso(300) }));
        return [e("Holidays", true, 0), e("Letter to bank.docx", false, 48 * 1024), e("Photo 001.jpg", false, 3.2 * 1024 * 1024)];
      }
      case "vault_add":
        if (a.password !== "password") throw { kind: "service", message: "That password is not right. Please try again." };
        await wait(500);
        vault = [...vault, ...(a.paths as string[]).map((p) => ({ path: p, dir: false, size: 120 * 1024, added: iso(1), snapshot: "v0" }))];
        return (a.paths as string[]).length;
      case "vault_remove":
        if (a.password !== "password") throw { kind: "service", message: "That password is not right. Please try again." };
        await wait(300);
        vault = vault.map((v) => ((a.paths as string[]).includes(v.path) ? { ...v, removes_at: Math.floor(Date.now() / 1000) + 7 * 86400 } : v));
        return null;
      case "cancel_delete":
        if (a.password !== "password") throw { kind: "service", message: "That password is not right. Please try again." };
        vault = vault.map(({ removes_at, ...v }) => v);
        return null;
      case "vault_list":
        return vault;
      case "vault_extract":
        if (a.password !== "password") throw { kind: "service", message: "That password is not right. Please try again." };
        await wait(500);
        return String(a.target);
      case "lifeboat_size_info":
        return sizeMode === "nogrow"
          ? { total: lbTotal, used: lbUsed(), max_size: lbTotal, can_grow: false, can_move: true, move_max: lbTotal + 200 * G, reason: "" }
          : sizeMode === "nospace"
            ? { total: lbTotal, used: lbUsed(), max_size: lbTotal, can_grow: false, can_move: false, move_max: lbTotal, reason: "D: has no room left to give to Lifeboat." }
            : { total: lbTotal, used: lbUsed(), max_size: 300 * G, can_grow: true, can_move: false, move_max: 300 * G, reason: "" };
      case "move_grow_lifeboat":
        if (a.password !== "password") throw { kind: "service", message: "That password is not right. Please try again." };
        await wait(1500);
        lbTotal = Number(a.newSize);
        return "";
      case "resize_lifeboat":
        if (a.password !== "password") throw { kind: "service", message: "That password is not right. Please try again." };
        await wait(800);
        lbTotal = Number(a.newSize);
        return lbTotal;
      case "remove_lifeboat":
        if (a.password !== "password") throw { kind: "service", message: "That password is not right. Please try again." };
        if (a.confirm !== "DELETE") throw { kind: "service", message: "Type DELETE to confirm." };
        await wait(800);
        folders = [];
        return "";
      case "open_folder":
        return null;
      case "is_admin":
        return mode !== "firstrun";
      case "relaunch_as_admin":
        return null;
      default:
        return null;
    }
  });
}
