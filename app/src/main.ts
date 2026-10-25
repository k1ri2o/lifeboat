import "./style.css";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { desktopDir, join } from "@tauri-apps/api/path";
import { api, toApiError, type Disk, type FileEntry, type HomeState, type Snapshot as Version } from "./api";
import {
  folderParts,
  isStale,
  keyGroups,
  lastBackupStamp,
  versionCountLine,
  smallSize,
  plainDate,
  baseName,
  restoreFolderName,
  lastBefore,
  nextBackupLine,
  relativeTime,
  sizeLabel,
  snapshotLabel,
  inlineTimeLabel,
} from "./format";
import { adminButton, button, errorBox, h, icon, spinner, type Child } from "./ui";

type Screen = "home" | "restore" | "vault" | "settings" | "setup";
let screen: Screen = "home";
/** Version to preselect on the Restore screen (set by the "Restore files from before..." button). */
let preselect: string | undefined;
const app = document.getElementById("app")!;

function go(s: Screen) {
  screen = s;
  void listen("status-changed", refresh);
  void listen("backup-done", refresh);
  void listen("service-down", refresh);
  void render();
}

// ---------- Shell ----------

function shell(body: HTMLElement, withNav: boolean): HTMLElement {
  const brand = h("div", { class: "brand" }, icon("lifebuoy", 26), h("span", {}, "Lifeboat"));
  const item = (s: Screen, label: string, ic: "home" | "restore" | "vault" | "settings") =>
    h(
      "button",
      { type: "button", class: "nav-item", "aria-current": screen === s ? "page" : "false", onclick: () => go(s) },
      icon(ic, 20),
      h("span", {}, label),
    );
  const nav = withNav
    ? h("nav", { "aria-label": "Main" }, item("home", "Home", "home"), item("restore", "Restore", "restore"), item("vault", "Vault", "vault"), item("settings", "Settings", "settings"))
    : null;
  return h("div", { class: "shell" }, h("header", {}, brand, nav), h("main", { id: "main" }, body));
}

function pageTitle(text: string, sub?: string): HTMLElement {
  return h("div", { class: "page-head" }, h("h1", {}, text), sub ? h("p", { class: "muted lead" }, sub) : null);
}

async function render() {
  const body = h("div", { class: "page" });
  app.replaceChildren(shell(body, screen !== "setup"));
  try {
    if (screen === "setup") return renderSetup(body);
    // First run: nothing backed up and nothing chosen yet means the wizard comes first.
    const st = await api.homeState();
    if (screen === "home" && isFirstRun(st)) {
      screen = "setup";
      return void render();
    }
    if (screen === "home") renderHome(body, st);
    else if (screen === "restore") await renderRestore(body, st);
    else if (screen === "vault") await renderVault(body);
    else renderSettings(body, st);
  } catch (e) {
    body.replaceChildren(pageTitle("Lifeboat"), errorBox(toApiError(e), () => void render()));
  }
}

function isFirstRun(st: HomeState): boolean {
  return st.status.last_run == null && st.folders.length === 0 && !st.status.frozen;
}

// ---------- Password dialog ----------

/** Ask for the Lifeboat password. If `submit` throws, the friendly message appears in the dialog. */
function askPassword(title: string, text: string, submit: (password: string) => Promise<void>): void {
  const pw = h("input", { type: "password", id: "dlg-pw", autocomplete: "off" }) as HTMLInputElement;
  const msg = h("div", { "aria-live": "polite" });
  const ok = button("Continue", { cls: "primary" });
  const cancel = button("Cancel");
  const box = h(
    "div",
    { class: "dialog", role: "dialog", "aria-modal": "true", "aria-labelledby": "dlg-title" },
    h("h2", { id: "dlg-title" }, title),
    h("p", { class: "muted" }, text),
    h("label", { for: "dlg-pw" }, "Your Lifeboat password"),
    pw,
    msg,
    h("div", { class: "actions" }, ok, cancel),
  );
  const overlay = h("div", { class: "overlay" }, box);
  const close = () => overlay.remove();
  const run = async () => {
    ok.disabled = true;
    msg.replaceChildren();
    try {
      await submit(pw.value);
      close();
    } catch (e) {
      msg.replaceChildren(errorBox(e));
      pw.select();
      ok.disabled = false;
    }
  };
  ok.addEventListener("click", () => void run());
  cancel.addEventListener("click", close);
  overlay.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Escape") close();
    if ((e as KeyboardEvent).key === "Enter") void run();
  });
  document.body.append(overlay);
  pw.focus();
}

// ---------- Folder list (shared by Home and the setup wizard) ----------

function folderRow(path: string, onRemove: () => void): HTMLElement {
  const p = folderParts(path);
  return h(
    "li",
    { class: "folder-row" },
    icon("folder", 22),
    h("div", { class: "grow" }, h("div", { class: "folder-name" }, p.name), p.where ? h("div", { class: "muted small", title: path }, p.where) : null),
    h("button", { type: "button", class: "btn quiet", "aria-label": `Stop backing up ${p.name}`, onclick: onRemove }, icon("trash", 18), h("span", {}, "Remove")),
  );
}

async function pickFolder(title: string): Promise<string | undefined> {
  const picked = await open({ directory: true, multiple: false, title });
  return typeof picked === "string" ? picked : undefined;
}

// ---------- Home ----------

type Health = "ok" | "attention" | "paused";

function renderHome(body: HTMLElement, st: HomeState) {
  const { status } = st;
  const health: Health = status.frozen ? "paused" : status.last_run == null || isStale(status.last_run) ? "attention" : "ok";
  const busy = h("p", { class: "busy-note", role: "status", "aria-live": "polite" });
  const message = h("div");

  const backupBtn = button("Back up now", { cls: "primary big", icon: "refresh" });
  backupBtn.addEventListener("click", async () => {
    backupBtn.disabled = true;
    message.replaceChildren();
    busy.textContent = "Backing up your files. This can take a few minutes...";
    try {
      await api.backupNow();
      await render();
    } catch (e) {
      backupBtn.disabled = false;
      busy.textContent = "";
      message.replaceChildren(errorBox(e));
    }
  });

  // Saved versions count and a shortcut into the newest one.
  const versions = h("div", { class: "muted", "aria-live": "polite" }, spinner("Counting saved versions..."));
  const seeBtn = button("See what's backed up", {
    icon: "folder",
    onclick: async () => {
      try {
        const snaps = await api.snapshots();
        preselect = [...snaps].sort((a, b) => b.time.localeCompare(a.time))[0]?.id;
        go("restore");
      } catch (e) {
        message.replaceChildren(errorBox(e));
      }
    },
  });
  void api
    .snapshots()
    .then((snaps) => versions.replaceChildren(document.createTextNode(snaps.length ? versionCountLine(snaps.length) : "No saved versions yet")))
    .catch(() => versions.replaceChildren());

  let card: HTMLElement;
  if (health === "paused") {
    const since = status.frozen_since;
    card = h(
      "section",
      { class: "card status paused", role: "status" },
      icon("pause", 40),
      h(
        "div",
        { class: "status-text" },
        h("h1", {}, "Backups paused: possible ransomware"),
        h("h2", {}, "What happened"),
        h(
          "p",
          {},
          "Lifeboat saw a very large number of your files change at once. That is a sign of ransomware, a program that locks up files. To keep your older copies safe, no new backups are being made. Please do not delete anything.",
        ),
        since ? h("p", { class: "muted" }, `This was noticed ${inlineTimeLabel(since)}.`) : null,
        h("div", { class: "actions", id: "paused-actions" }),
      ),
    );
    // Find the last good version from before the problem and offer it in one big button.
    void api
      .snapshots()
      .then((snaps) => {
        const safe = since ? lastBefore(snaps, since) : undefined;
        const holder = card.querySelector("#paused-actions")!;
        holder.replaceChildren(
          button(safe ? `Restore files from before ${inlineTimeLabel(since!)}` : "Go to Restore", {
            cls: "danger big",
            icon: "restore",
            onclick: () => {
              preselect = safe?.id;
              go("restore");
            },
          }),
        );
      })
      .catch((e) => message.replaceChildren(errorBox(e)));
  } else if (health === "attention") {
    const never = status.last_run == null;
    card = h(
      "section",
      { class: "card status attention", role: "status" },
      icon("alert", 40),
      h(
        "div",
        { class: "status-text" },
        h("h1", {}, "Needs your attention"),
        h(
          "p",
          {},
          never
            ? "Your files have not been backed up yet. Press the button to make your first backup."
            : `Your last backup was ${relativeTime(new Date(status.last_run! * 1000).toISOString())}, which is longer than usual. Press the button to back up now.`,
        ),
        h("div", { class: "actions" }, backupBtn),
        busy,
      ),
    );
  } else {
    card = h(
      "section",
      { class: "card status ok", role: "status" },
      icon("shield", 40),
      h(
        "div",
        { class: "status-text" },
        h("h1", {}, "Your files are protected"),
        h("p", { class: "facts" }, h("span", {}, lastBackupStamp(status.last_run)), h("span", {}, nextBackupLine(status.last_run))),
        versions,
        h("div", { class: "actions" }, backupBtn, seeBtn),
        busy,
      ),
    );
  }

  // Folders
  const list = h("ul", { class: "folders" });
  const draw = (folders: string[]) => {
    list.replaceChildren(
      ...(folders.length
        ? folders.map((f) => folderRow(f, () => confirmRemove(f, draw)))
        : [h("li", { class: "muted empty" }, "No folders yet. Add a folder to start protecting it.")]),
    );
  };
  draw(st.folders);
  const add = button("Add a folder", {
    icon: "plus",
    onclick: async () => {
      const picked = await pickFolder("Choose a folder to protect");
      if (!picked) return;
      try {
        draw(await api.addFolder(picked));
      } catch (e) {
        message.replaceChildren(errorBox(e));
      }
    },
  });

  body.append(
    card,
    message,
    h(
      "section",
      { class: "section" },
      h("div", { class: "section-head" }, h("h2", {}, "Folders we protect"), add),
      h("p", { class: "muted" }, "Backed up: stays on your PC, and a copy is in the safe."),
      h("div", { class: "card flush" }, list),
    ),
    h(
      "section",
      { class: "section" },
      h("div", { class: "section-head" }, h("h2", {}, "Vault"), button("Open the vault", { icon: "vault", onclick: () => go("vault") })),
      h("p", { class: "muted" }, "Vault: only in the safe. Files you put there are taken off your PC."),
    ),
  );
}

function confirmRemove(folder: string, draw: (folders: string[]) => void): void {
  const name = folderParts(folder).name;
  askPassword(
    `Stop protecting "${name}"?`,
    "Backups you already have stay safe. Type your password so nobody else can switch your protection off.",
    async (password) => draw(await api.removeFolder(folder, password)),
  );
}

// ---------- Small shared pieces ----------

/** A plain yes/no dialog with a body. Calls `onOk` after it closes. */
function confirmDialog(title: string, content: Child, okLabel: string, onOk: () => void): void {
  const ok = button(okLabel, { cls: "primary" });
  const cancel = button("Cancel");
  const box = h(
    "div",
    { class: "dialog", role: "dialog", "aria-modal": "true", "aria-label": title },
    h("h2", {}, title),
    content,
    h("div", { class: "actions" }, ok, cancel),
  );
  const overlay = h("div", { class: "overlay" }, box);
  const close = () => overlay.remove();
  ok.addEventListener("click", () => {
    close();
    onOk();
  });
  cancel.addEventListener("click", close);
  overlay.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Escape") close();
  });
  document.body.append(overlay);
  ok.focus();
}

/** Green "done" box with an "Open folder" button. */
function doneBox(title: string, text: string, folder: string): HTMLElement {
  const msg = h("div", { "aria-live": "polite" });
  return h(
    "div",
    { class: "notice good" },
    icon("check", 22),
    h(
      "div",
      { class: "notice-body" },
      h("p", { class: "notice-title" }, title),
      h("p", { class: "muted" }, text),
      h(
        "div",
        { class: "actions" },
        button("Open folder", {
          icon: "folder",
          cls: "primary",
          onclick: async () => {
            msg.replaceChildren();
            try {
              await api.openFolder(folder);
            } catch (e) {
              msg.replaceChildren(errorBox(e));
            }
          },
        }),
      ),
      msg,
    ),
  );
}

/** A new folder on the Desktop named "Lifeboat restore <date>", or "" if Windows will not say where the Desktop is. */
async function defaultRestoreTarget(): Promise<string> {
  try {
    const p = await join(await desktopDir(), restoreFolderName());
    return typeof p === "string" ? p : "";
  } catch {
    return "";
  }
}

/** True when `path` sits inside one of the chosen folders. */
function insideChosen(path: string, chosen: Iterable<string>): boolean {
  for (const c of chosen) {
    if (c !== path && path.startsWith(c) && /[\\/]/.test(path.charAt(c.length))) return true;
  }
  return false;
}

// ---------- Restore ----------

async function renderRestore(body: HTMLElement, home: HomeState) {
  body.append(pageTitle("Restore your files", "Go back to how your files looked at an earlier time."));
  const loading = spinner("Looking for your saved versions...");
  body.append(loading);
  const versions: Version[] = [...(await api.snapshots())].sort((a, b) => b.time.localeCompare(a.time));
  const defaultTarget = await defaultRestoreTarget();
  loading.remove();
  if (!versions.length) {
    body.append(h("div", { class: "card empty-state" }, icon("restore", 32), h("p", {}, "There is nothing to restore yet. Once your first backup is done, it will show up here.")));
    return;
  }
  const frozenSince = home.status.frozen ? home.status.frozen_since : null;
  const safe = frozenSince ? lastBefore(versions, frozenSince) : undefined;
  let chosen: string = preselect && versions.some((v) => v.id === preselect) ? preselect : (safe?.id ?? versions[0].id);
  preselect = undefined;
  let showAll = false;
  let target = defaultTarget;
  const label = (id: string) => {
    const v = versions.find((x) => x.id === id);
    return v ? snapshotLabel(v.time) : "";
  };

  // What is picked inside the version. A chosen folder includes everything in it.
  const sel = new Map<string, FileEntry>();
  let trail: { name: string; path?: string }[] = [{ name: "All files" }];
  let token = 0;

  const result = h("div", { role: "status", "aria-live": "polite" });
  const targetText = h("span", {}, target || "No folder chosen yet");
  if (!target) targetText.className = "muted";
  const goBtn = button("Restore everything in this version", { cls: "primary big", icon: "restore" });
  const summary = h("p", { class: "muted small", role: "status", "aria-live": "polite" });
  const crumbBar = h("nav", { class: "crumbs", "aria-label": "Where you are" });
  const fileList = h("ul", { class: "file-list" });

  const updateSummary = () => {
    const n = sel.size;
    summary.textContent = n ? `${n} ${n === 1 ? "item" : "items"} chosen. A folder you choose includes everything inside it.` : "Nothing chosen yet. Tick the files or folders you want, or restore everything.";
    goBtn.querySelector("span")!.textContent = n ? `Restore the ${n} chosen ${n === 1 ? "item" : "items"}` : "Restore everything in this version";
  };

  const drawCrumbs = () => {
    crumbBar.replaceChildren(
      ...trail.flatMap((c, i) => {
        const last = i === trail.length - 1;
        const part = last
          ? h("span", { class: "crumb current", "aria-current": "page" }, c.name)
          : h(
              "button",
              {
                type: "button",
                class: "crumb link",
                onclick: () => {
                  trail = trail.slice(0, i + 1);
                  void loadLevel();
                },
              },
              c.name,
            );
        return i === 0 ? [part] : [icon("chevron", 14), part];
      }),
    );
  };

  const drawFiles = (items: FileEntry[]) => {
    if (!items.length) return fileList.replaceChildren(h("li", { class: "muted empty" }, "This folder is empty."));
    fileList.replaceChildren(
      ...items.map((f, i) => {
        const covered = insideChosen(f.path, sel.keys());
        const cb = h("input", { type: "checkbox", id: `f-${i}`, checked: sel.has(f.path) || covered, disabled: covered, "aria-label": `Choose ${f.name}` }) as HTMLInputElement;
        cb.addEventListener("change", () => {
          if (cb.checked) sel.set(f.path, f);
          else sel.delete(f.path);
          updateSummary();
          drawFiles(items);
        });
        const name = f.dir
          ? h(
              "button",
              {
                type: "button",
                class: "link file-name",
                title: `Open ${f.name}`,
                onclick: () => {
                  trail = [...trail, { name: f.name, path: f.path }];
                  void loadLevel();
                },
              },
              f.name,
            )
          : h("span", { class: "file-name" }, f.name);
        const when = plainDate(f.mtime);
        return h(
          "li",
          { class: `file-row${covered ? " covered" : ""}`, title: covered ? "Included because the folder around it is chosen" : "" },
          cb,
          icon(f.dir ? "folder" : "file", 20),
          h("div", { class: "grow" }, name),
          h("span", { class: "muted small file-meta" }, f.dir ? "Folder" : smallSize(f.size)),
          h("span", { class: "muted small file-meta" }, when),
        );
      }),
    );
  };

  const loadLevel = async () => {
    const mine = ++token;
    drawCrumbs();
    fileList.replaceChildren(h("li", {}, spinner("Opening...")));
    try {
      const items = [...(await api.listFiles(chosen, trail[trail.length - 1].path))].sort((a, b) => Number(b.dir) - Number(a.dir) || a.name.localeCompare(b.name));
      if (mine === token) drawFiles(items);
    } catch (e) {
      if (mine === token) fileList.replaceChildren(h("li", {}, errorBox(e, () => void loadLevel())));
    }
  };

  const pickVersion = (id: string) => {
    chosen = id;
    sel.clear();
    trail = [{ name: "All files" }];
    result.replaceChildren();
    drawVersions();
    updateSummary();
    void loadLevel();
  };

  const list = h("div", { class: "choices", role: "radiogroup", "aria-label": "Backup times" });
  const drawVersions = () => {
    const shown = showAll ? versions : versions.slice(0, 8);
    list.replaceChildren(
      ...shown.map((v) => {
        const isSafe = safe?.id === v.id;
        const radio = h("input", { type: "radio", name: "ver", id: `v-${v.id}`, checked: v.id === chosen });
        radio.addEventListener("change", () => pickVersion(v.id));
        return h(
          "label",
          { class: `choice${v.id === chosen ? " selected" : ""}`, for: `v-${v.id}` },
          radio,
          h("span", { class: "grow" }, h("span", { class: "choice-title" }, snapshotLabel(v.time)), h("span", { class: "muted small" }, relativeTime(v.time))),
          isSafe ? h("span", { class: "pill ok" }, "Last good copy") : null,
        );
      }),
      !showAll && versions.length > 8 ? button(`Show ${versions.length - 8} older`, { cls: "quiet", onclick: () => ((showAll = true), drawVersions()) }) : "",
    );
  };
  drawVersions();
  updateSummary();

  body.append(
    frozenSince && safe
      ? h("div", { class: "notice warn" }, icon("alert", 22), h("div", { class: "notice-body" }, h("p", { class: "notice-title" }, "Backups are paused"), h("p", { class: "muted" }, `We picked the last good copy, from ${inlineTimeLabel(safe.time)}, before the problem started.`)))
      : "",
    step(1, "Pick a time", "Choose how far back to go.", list),
    step(2, "Choose what to bring back", "Open folders to look inside. Tick what you want, or leave everything unticked to restore the whole version.", h("div", {}, crumbBar, h("div", { class: "card flush browser" }, fileList), summary)),
    step(
      3,
      "Choose where the files go",
      "Your files are copied into a new, empty folder. Nothing on your computer is replaced or deleted.",
      h(
        "div",
        { class: "target-row" },
        icon("folder", 22),
        h("div", { class: "grow" }, targetText),
        button(target ? "Choose another folder" : "Choose a folder", {
          onclick: async () => {
            const picked = await pickFolder("Choose a new, empty folder");
            if (picked) {
              target = picked;
              targetText.textContent = picked;
              targetText.className = "";
            }
          },
        }),
      ),
    ),
    h("div", { class: "actions" }, goBtn),
    result,
  );
  void loadLevel();

  goBtn.addEventListener("click", () => {
    if (!target) return void result.replaceChildren(errorBox({ kind: "other", message: "Please choose a folder for your files first." }));
    result.replaceChildren();
    const paths = [...sel.keys()];
    askPassword(
      paths.length ? "Restore the chosen files" : "Restore everything",
      `Files from ${label(chosen)} will be copied into ${target}. Type your password so we know it is you.`,
      async (password) => {
        const out = await api.restore(chosen, target, password, paths);
        const where = out || target;
        body.replaceChildren(
          pageTitle("Your files are back"),
          doneBox("Done", `Your files from ${label(chosen)} are in ${where}`, where),
          h("div", { class: "actions" }, button("Back to Home", { onclick: () => go("home") })),
        );
      },
    );
  });
}

function step(n: number, title: string, hint: string, content: Child): HTMLElement {
  return h("section", { class: "section" }, h("h2", {}, h("span", { class: "num" }, String(n)), title), h("p", { class: "muted" }, hint), content);
}

// ---------- Vault ----------

async function renderVault(body: HTMLElement) {
  const message = h("div", { "aria-live": "polite" });
  const listHolder = h("div", { class: "card flush" });
  const chosen = new Set<string>();
  const takeOut = button("Take out", { cls: "primary", icon: "restore" });
  takeOut.disabled = true;

  const updateTakeOut = () => {
    takeOut.disabled = chosen.size === 0;
    takeOut.querySelector("span")!.textContent = chosen.size ? `Take out ${chosen.size} ${chosen.size === 1 ? "item" : "items"}` : "Take out";
  };

  const loadList = async () => {
    chosen.clear();
    updateTakeOut();
    listHolder.replaceChildren(spinner("Opening the vault..."));
    try {
      const items = [...(await api.vaultList())].sort((a, b) => b.added.localeCompare(a.added));
      if (!items.length) return listHolder.replaceChildren(h("p", { class: "muted empty" }, "The vault is empty. Add files or a folder to keep them only in the safe."));
      listHolder.replaceChildren(
        h(
          "ul",
          { class: "file-list" },
          ...items.map((it, i) => {
            const cb = h("input", { type: "checkbox", id: `vi-${i}`, "aria-label": `Choose ${baseName(it.path)}` }) as HTMLInputElement;
            cb.addEventListener("change", () => {
              if (cb.checked) chosen.add(it.path);
              else chosen.delete(it.path);
              updateTakeOut();
            });
            const p = folderParts(it.path);
            return h(
              "li",
              { class: "file-row" },
              cb,
              icon(it.dir ? "folder" : "file", 20),
              h("div", { class: "grow" }, h("div", { class: "file-name" }, p.name), p.where ? h("div", { class: "muted small", title: it.path }, p.where) : null),
              h("span", { class: "muted small file-meta" }, it.dir ? "Folder" : smallSize(it.size)),
              h("span", { class: "muted small file-meta" }, it.added ? `Added ${plainDate(it.added)}` : ""),
            );
          }),
        ),
      );
    } catch (e) {
      listHolder.replaceChildren(errorBox(e, () => void loadLevelAgain()));
    }
  };
  const loadLevelAgain = () => loadList();

  const addFlow = async (kind: "files" | "folder") => {
    message.replaceChildren();
    const r = kind === "files" ? await open({ multiple: true, directory: false, title: "Choose files for the vault" }) : await open({ multiple: false, directory: true, title: "Choose a folder for the vault" });
    const picked: string[] = Array.isArray(r) ? r : typeof r === "string" ? [r] : [];
    if (!picked.length) return;
    confirmDialog(
      "Move this into the vault?",
      h(
        "div",
        {},
        h("p", { class: "muted" }, "These will be copied into Lifeboat's safe. After Lifeboat has checked the safe copy, they are removed from your PC."),
        h("ul", { class: "plain-list" }, ...picked.slice(0, 12).map((p) => h("li", {}, h("strong", {}, baseName(p)), h("span", { class: "muted small", title: p }, ` ${folderParts(p).where}`)))),
        picked.length > 12 ? h("p", { class: "muted small" }, `and ${picked.length - 12} more`) : null,
      ),
      "Move to the vault",
      () =>
        askPassword("Move to the vault", "Type your password so we know it is you.", async (password) => {
          const n = await api.vaultAdd(password, picked);
          message.replaceChildren(h("div", { class: "notice good" }, icon("check", 22), h("div", { class: "notice-body" }, h("p", { class: "notice-title" }, n === 1 ? "1 item is now in the vault" : `${n} items are now in the vault`), h("p", { class: "muted" }, "Lifeboat checked the safe copy and removed the originals from your PC."))));
          void loadList();
        }),
    );
  };

  takeOut.addEventListener("click", async () => {
    message.replaceChildren();
    const folder = await pickFolder("Choose a new, empty folder to put the files in");
    if (!folder) return;
    const paths = [...chosen];
    askPassword("Take files out of the vault", `They will be copied into ${folder}. Type your password so we know it is you.`, async (password) => {
      const out = await api.vaultExtract(password, paths, folder);
      const where = out || folder;
      message.replaceChildren(doneBox("Your files are out of the vault", `They are in ${where}`, where));
      void loadList();
    });
  });

  body.append(
    pageTitle("Vault", "A private place for files you want off this PC."),
    h("div", { class: "notice info" }, icon("vault", 22), h("div", { class: "notice-body" }, h("p", {}, "Files in the vault are kept only inside Lifeboat's hidden safe. They are removed from your PC after Lifeboat has checked the safe copy. If you forget the password and lose the recovery key, they are gone."))),
    h("p", { class: "muted" }, "Backed up: stays on your PC, a copy is in the safe. Vault: only in the safe."),
    h("div", { class: "actions" }, button("Add files", { icon: "file", cls: "primary", onclick: () => void addFlow("files") }), button("Add a folder", { icon: "folder", onclick: () => void addFlow("folder") })),
    message,
    h("section", { class: "section" }, h("div", { class: "section-head" }, h("h2", {}, "In the vault"), takeOut), listHolder),
  );
  await loadList();
}

// ---------- Settings ----------

function renderSettings(body: HTMLElement, st: HomeState) {
  body.append(
    pageTitle("Settings"),
    h(
      "section",
      { class: "card rows" },
      h("div", { class: "setting" }, icon("drive", 24), h("div", { class: "grow" }, h("h3", {}, "Backup drive"), h("p", { class: "muted" }, "Choose a different drive for your backups. Do this only if you were asked to.")), button("Set up a drive", { onclick: () => go("setup") })),
      h("div", { class: "setting" }, icon("lock", 24), h("div", { class: "grow" }, h("h3", {}, "Your password and recovery key"), h("p", { class: "muted" }, "Keep them somewhere safe, not on this computer. Lifeboat cannot show them again."))),
      h("div", { class: "setting" }, icon("shield", 24), h("div", { class: "grow" }, h("h3", {}, "Extra protection"), h("p", { class: "muted" }, "Old backups are never changed. Deleting a backup needs your password and waits 7 days."))),
      h("div", { class: "setting" }, icon("info", 24), h("div", { class: "grow" }, h("h3", {}, "About"), h("p", { class: "muted" }, `Lifeboat version ${st.status.version}`))),
    ),
  );
}

// ---------- First-run setup wizard ----------

function diskKind(d: Disk): "external" | "internal" {
  return /usb/i.test(d.bus_type) ? "external" : "internal";
}

/** Lettered parts (C:, D:...) on a drive, biggest first. Lifeboat shrinks one of them. */
function lettersOn(d: Disk): { letter: string; size: number }[] {
  return d.partitions
    .filter((p) => p.drive_letter)
    .map((p) => ({ letter: p.drive_letter!.replace(":", "").toUpperCase(), size: p.size }))
    .sort((a, b) => b.size - a.size);
}

function internalLabel(d: Disk): string {
  return d.is_boot || d.is_system ? "The drive Windows runs on" : "A second drive in this computer";
}

interface Wizard {
  step: 1 | 2 | 3;
  disk?: Disk;
  letter: string;
  gb: string;
  erase: boolean;
  folders: string[];
  password: string;
  password2: string;
}

const wiz: Wizard = { step: 1, letter: "", gb: "100", erase: false, folders: [], password: "", password2: "" };

function stepper(current: number): HTMLElement {
  const names = ["Backup drive", "Folders", "Password"];
  return h(
    "ol",
    { class: "stepper", "aria-label": `Step ${current} of 3` },
    ...names.map((n, i) =>
      h("li", { class: i + 1 === current ? "current" : i + 1 < current ? "done" : "", "aria-current": i + 1 === current ? "step" : "false" }, h("span", { class: "dot" }, i + 1 < current ? icon("check", 14) : String(i + 1)), h("span", {}, n)),
    ),
  );
}

/** Shown at the top of every setup step when Lifeboat is not running as administrator. */
function adminBanner(): HTMLElement {
  return h(
    "div",
    { class: "notice warn" },
    icon("shield", 22),
    h(
      "div",
      { class: "notice-body" },
      h("p", { class: "notice-title" }, "Setup needs administrator rights"),
      h("p", { class: "muted" }, "Lifeboat has to hide the backup drive from other programs, and Windows only allows that for an administrator. Press the button and choose Yes when Windows asks."),
      adminButton(),
    ),
  );
}

async function renderSetup(body: HTMLElement) {
  let admin = true;
  try {
    admin = await api.isAdmin();
  } catch {
    // If we cannot tell, do not nag.
  }
  const draw = () => {
    body.replaceChildren();
    if (!admin) body.append(adminBanner());
    if (wiz.step === 1) return void drawStep1(body, draw);
    if (wiz.step === 2) return drawStep2(body, draw);
    drawStep3(body, draw);
  };
  draw();
}

async function drawStep1(body: HTMLElement, redraw: () => void) {
  body.append(
    h("p", { class: "eyebrow" }, "Step 1 of 3"),
    stepper(1),
    pageTitle("Where should your backups go?", "Lifeboat keeps your backups on a hidden part of a drive, where viruses and ransomware cannot reach them."),
  );
  const holder = h("div", { class: "section" }, spinner("Looking for drives..."));
  body.append(holder);
  let disks: Disk[];
  try {
    disks = (await api.disks()).filter((d) => !d.is_boot && !d.is_system || diskKind(d) === "internal");
  } catch (e) {
    return holder.replaceChildren(errorBox(e, redraw));
  }
  if (!disks.length) {
    return holder.replaceChildren(h("div", { class: "card empty-state" }, icon("usb", 32), h("p", {}, "We could not find a drive. Plug in a USB drive and try again."), button("Look again", { icon: "refresh", onclick: redraw })));
  }
  // USB drives first, then second internal drives, then the Windows drive.
  const rank = (d: Disk) => (diskKind(d) === "external" ? 0 : d.is_boot || d.is_system ? 2 : 1);
  disks.sort((a, b) => rank(a) - rank(b));
  const detail = h("div");
  const list = h("div", { class: "choices", role: "radiogroup", "aria-label": "Drives" });
  const next = button("Continue", { cls: "primary big" });

  const drawDetail = () => {
    const d = wiz.disk;
    next.disabled = !d;
    if (!d) return detail.replaceChildren();
    if (diskKind(d) === "external") {
      const cb = h("input", { type: "checkbox", id: "erase", checked: wiz.erase }) as HTMLInputElement;
      cb.addEventListener("change", () => ((wiz.erase = cb.checked), (next.disabled = !wiz.erase)));
      next.disabled = !wiz.erase;
      detail.replaceChildren(h("div", { class: "notice warn" }, icon("alert", 22), h("div", { class: "notice-body" }, h("p", { class: "notice-title" }, "Everything on this drive will be erased"), h("label", { class: "check", for: "erase" }, cb, "I understand and I have nothing on this drive I need."))));
    } else {
      const parts = lettersOn(d);
      if (!parts.length) {
        next.disabled = true;
        return detail.replaceChildren(h("div", { class: "notice warn" }, icon("alert", 22), h("div", { class: "notice-body" }, h("p", { class: "notice-title" }, "Lifeboat cannot use this drive"), h("p", { class: "muted" }, "It has no part with a drive letter (like C: or D:) that Lifeboat could make smaller. Pick another drive."))));
      }
      if (!parts.some((p) => p.letter === wiz.letter)) wiz.letter = parts[0].letter;
      const gb = h("input", { type: "number", id: "gb", min: "1", value: wiz.gb }) as HTMLInputElement;
      gb.addEventListener("input", () => (wiz.gb = gb.value));
      const windowsDrive = d.is_boot || d.is_system;
      const warn = windowsDrive
        ? h("div", { class: "notice warn" }, icon("alert", 22), h("div", { class: "notice-body" }, h("p", { class: "notice-title" }, "This is less safe"), h("p", { class: "muted" }, "This is the same drive Windows is on. If that drive fails, your files and the backups are lost together. A USB drive or a second drive is a better choice.")))
        : h("div", { class: "notice info" }, icon("info", 22), h("div", { class: "notice-body" }, h("p", { class: "notice-title" }, "Good choice"), h("p", { class: "muted" }, "This is a different drive from the one Windows is on, so the backups survive if the Windows drive fails. Nothing on it is deleted: Lifeboat only borrows some of its free space.")));
      let where: HTMLElement;
      if (parts.length === 1) {
        where = h("p", {}, `Lifeboat will take the space from drive ${parts[0].letter}: on this drive. Your files on ${parts[0].letter}: stay where they are.`);
      } else {
        where = h(
          "div",
          { class: "choices", role: "radiogroup", "aria-label": "Drive letters" },
          ...parts.map((p) => {
            const r = h("input", { type: "radio", name: "letter", id: `l-${p.letter}`, checked: p.letter === wiz.letter });
            r.addEventListener("change", () => ((wiz.letter = p.letter), drawDetail()));
            return h("label", { class: `choice${p.letter === wiz.letter ? " selected" : ""}`, for: `l-${p.letter}` }, r, h("span", { class: "grow" }, h("span", { class: "choice-title" }, `Drive ${p.letter}:`), h("span", { class: "muted small" }, sizeLabel(p.size))));
          }),
        );
      }
      detail.replaceChildren(
        warn,
        parts.length > 1 ? h("p", { class: "muted" }, "Which part of this drive should Lifeboat take space from?") : "",
        where,
        h("div", { class: "fields" }, h("div", {}, h("label", { for: "gb" }, "How many GB should be set aside for backups?"), gb)),
        h("p", { class: "muted small" }, `Pick more than the folders you want to protect take up. It must fit in the free space on ${wiz.letter}:; if it does not, Lifeboat will tell you the most it can use.`),
      );
    }
  };
  const drawList = () =>
    list.replaceChildren(
      ...disks.map((d) => {
        const ext = diskKind(d) === "external";
        const radio = h("input", { type: "radio", name: "disk", id: `d-${d.number}`, checked: wiz.disk?.number === d.number });
        radio.addEventListener("change", () => {
          wiz.disk = d;
          wiz.erase = false;
          drawList();
          drawDetail();
        });
        return h(
          "label",
          { class: `choice${wiz.disk?.number === d.number ? " selected" : ""}`, for: `d-${d.number}` },
          radio,
          icon(ext ? "usb" : "drive", 24),
          h("span", { class: "grow" }, h("span", { class: "choice-title" }, d.model || "Drive"), h("span", { class: "muted small" }, `${sizeLabel(d.size)} - ${ext ? "USB drive" : internalLabel(d)}${lettersOn(d).length ? ` (${lettersOn(d).map((p) => p.letter + ":").join(", ")})` : ""}`)),
          ext ? h("span", { class: "pill ok" }, "Recommended") : null,
        );
      }),
    );
  next.addEventListener("click", () => ((wiz.step = 2), redraw()));
  holder.replaceChildren(
    h("p", { class: "hint" }, icon("info", 18), "Best: a USB drive that stays plugged in. Next best: a second drive in this computer. Avoid the drive Windows runs on: if it fails, the backups go with it."),
    list,
    detail,
    h("div", { class: "actions" }, next),
  );
  drawList();
  drawDetail();
}

function drawStep2(body: HTMLElement, redraw: () => void) {
  const list = h("ul", { class: "folders" });
  const next = button("Continue", { cls: "primary big" });
  const drawFolders = () => {
    list.replaceChildren(
      ...(wiz.folders.length
        ? wiz.folders.map((f) => folderRow(f, () => ((wiz.folders = wiz.folders.filter((x) => x !== f)), drawFolders())))
        : [h("li", { class: "muted empty" }, "No folders chosen yet. Pick the ones that hold things you would hate to lose, like Documents and Pictures.")]),
    );
    next.disabled = wiz.folders.length === 0;
  };
  drawFolders();
  next.addEventListener("click", () => ((wiz.step = 3), redraw()));
  const back = button("Back", { onclick: () => ((wiz.step = 1), redraw()) });
  body.append(
    h("p", { class: "eyebrow" }, "Step 2 of 3"),
    stepper(2),
    pageTitle("What should we protect?", "Lifeboat will back these folders up automatically, about once an hour. You can change this later."),
    h("div", { class: "card flush" }, list),
    h("div", { class: "actions" }, button("Add a folder", { icon: "plus", onclick: async () => {
      const picked = await pickFolder("Choose a folder to protect");
      if (picked && !wiz.folders.some((f) => f.toLowerCase() === picked.toLowerCase())) {
        wiz.folders.push(picked);
        drawFolders();
      }
    } })),
    h("div", { class: "actions split" }, back, next),
  );
}

function drawStep3(body: HTMLElement, redraw: () => void) {
  const pw = h("input", { type: "password", id: "spw", autocomplete: "new-password", value: wiz.password }) as HTMLInputElement;
  const pw2 = h("input", { type: "password", id: "spw2", autocomplete: "new-password", value: wiz.password2 }) as HTMLInputElement;
  const msg = h("div", { "aria-live": "polite" });
  const progress = h("div", { class: "progress-box", "aria-live": "polite" });
  const go1 = button("Start protecting my files", { cls: "primary big", icon: "shield" });
  const back = button("Back", { onclick: () => ((wiz.step = 2), redraw()) });
  const external = wiz.disk ? diskKind(wiz.disk) === "external" : true;
  // The drive can be prepared while the final step fails: then only the last step is retried.
  let prepared = false;

  const steps = ["Preparing the backup drive", "Setting up your protection", "Making your recovery key"];
  const showProgress = (active: number) =>
    progress.replaceChildren(
      h("ul", { class: "progress" }, ...steps.map((s, i) => h("li", { class: i < active ? "done" : i === active ? "active" : "" }, h("span", { class: "dot" }, i < active ? icon("check", 14) : ""), s))),
      h("p", { class: "muted" }, "Please keep this window open. This can take a few minutes."),
    );

  go1.addEventListener("click", async () => {
    wiz.password = pw.value;
    wiz.password2 = pw2.value;
    if (pw.value.length < 8) return void msg.replaceChildren(errorBox({ kind: "other", message: "Your password needs at least 8 characters." }));
    if (pw.value !== pw2.value) return void msg.replaceChildren(errorBox({ kind: "other", message: "The two passwords are not the same." }));
    msg.replaceChildren();
    go1.disabled = back.disabled = true;
    try {
      if (!prepared) {
        showProgress(0);
        await api.prepareDisk(wiz.disk!.number, pw.value, external ? undefined : { driveLetter: wiz.letter, sizeGb: Number(wiz.gb) });
        prepared = true;
      }
      showProgress(1);
      const key = await api.initRepo(pw.value, wiz.folders);
      showProgress(2);
      wiz.password = wiz.password2 = "";
      showRecoveryKey(body, key);
      return;
    } catch (e) {
      progress.replaceChildren();
      msg.replaceChildren(errorBox(e));
      if (prepared) go1.querySelector("span")!.textContent = "Try the last step again";
    }
    go1.disabled = back.disabled = false;
  });

  body.append(
    h("p", { class: "eyebrow" }, "Step 3 of 3"),
    stepper(3),
    pageTitle("Choose a password", "You will need it to get your files back or to change your protection. Pick something you will remember, and write it down."),
    h("div", { class: "card form" }, h("label", { for: "spw" }, "Password (at least 8 characters)"), pw, h("label", { for: "spw2" }, "Type it again"), pw2),
    msg,
    progress,
    h("div", { class: "actions split" }, back, go1),
  );
}

/** Show the recovery key once, large, in groups. Continue only after "I wrote it down". */
function showRecoveryKey(body: HTMLElement, key: string) {
  const groups = keyGroups(key);
  const saved = h("input", { type: "checkbox", id: "saved" }) as HTMLInputElement;
  const next = button("Finish", { cls: "primary big", icon: "check" });
  next.disabled = true;
  saved.addEventListener("change", () => (next.disabled = !saved.checked));
  next.addEventListener("click", () => go("home"));
  body.replaceChildren(
    h("p", { class: "eyebrow" }, "Almost done"),
    pageTitle("Write down your recovery key", "If you ever forget your password, this key is the only other way to open your backups. It is shown just this once, and Lifeboat does not keep a copy."),
    h(
      "div",
      { id: "key-print", class: "card key-card" },
      h("h2", { class: "print-only" }, "Lifeboat recovery key"),
      h("div", { class: "key", "aria-label": "Recovery key" }, ...groups.map((g) => h("span", { class: "key-group" }, g))),
      h("p", { class: "print-only" }, "Keep this paper safe. Anyone who has this key can open your backups. Do not store it on this computer."),
    ),
    h("div", { class: "actions" }, button("Print this key", { icon: "printer", onclick: () => window.print() })),
    h("div", { class: "notice warn" }, icon("alert", 22), h("div", { class: "notice-body" }, h("p", { class: "notice-title" }, "Keep it somewhere safe"), h("p", { class: "muted" }, "Print it or write it on paper and keep it in a drawer. Do not keep it only on this computer."))),
    h("label", { class: "check", for: "saved" }, saved, "I have printed or written down my recovery key."),
    h("div", { class: "actions" }, next),
  );
}

// ---------- Live updates from the tray / poller ----------

const refresh = () => screen === "home" && void render();

// Dev-only: `?mock=ok|attention|attack|firstrun|error` swaps in fake data so screens can be previewed
// in a plain browser. This import is removed from production builds.
async function start() {
  if (import.meta.env.DEV) {
    const m = new URLSearchParams(location.search).get("mock");
    if (m) {
      await (await import("./mock")).installMock(m);
      const s = new URLSearchParams(location.search).get("screen") as Screen | null;
      if (s) screen = s;
    }
  }
  void listen("status-changed", refresh);
  void listen("backup-done", refresh);
  void listen("service-down", refresh);
  void render();
}
void start();
