import "./style.css";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { api, toApiError, type Disk, type HomeState, type Snapshot as Version } from "./api";
import {
  folderParts,
  isStale,
  keyGroups,
  lastBackupLine,
  lastBefore,
  nextBackupLine,
  relativeTime,
  sizeLabel,
  snapshotLabel,
  inlineTimeLabel,
} from "./format";
import { button, errorBox, h, icon, type Child } from "./ui";

type Screen = "home" | "restore" | "settings" | "setup";
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
  const item = (s: Screen, label: string, ic: "home" | "restore" | "settings") =>
    h(
      "button",
      { type: "button", class: "nav-item", "aria-current": screen === s ? "page" : "false", onclick: () => go(s) },
      icon(ic, 20),
      h("span", {}, label),
    );
  const nav = withNav
    ? h("nav", { "aria-label": "Main" }, item("home", "Home", "home"), item("restore", "Restore", "restore"), item("settings", "Settings", "settings"))
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
        h("p", { class: "facts" }, h("span", {}, lastBackupLine(status.last_run)), h("span", {}, nextBackupLine(status.last_run))),
        h("div", { class: "actions" }, backupBtn),
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
      h("div", { class: "card flush" }, list),
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

// ---------- Restore ----------

async function renderRestore(body: HTMLElement, home: HomeState) {
  const versions: Version[] = [...(await api.snapshots())].sort((a, b) => b.time.localeCompare(a.time));
  body.append(pageTitle("Restore your files", "Go back to how your files looked at an earlier time."));
  if (!versions.length) {
    body.append(h("div", { class: "card empty-state" }, icon("restore", 32), h("p", {}, "There is nothing to restore yet. Once your first backup is done, it will show up here.")));
    return;
  }
  const frozenSince = home.status.frozen ? home.status.frozen_since : null;
  const safe = frozenSince ? lastBefore(versions, frozenSince) : undefined;
  let chosen: string | undefined = preselect ?? safe?.id ?? versions[0].id;
  preselect = undefined;
  let showAll = false;
  let target = "";

  const result = h("div", { role: "status", "aria-live": "polite" });
  const targetText = h("span", { class: "muted" }, "No folder chosen yet");
  const pw = h("input", { type: "password", id: "pw", autocomplete: "off" }) as HTMLInputElement;
  const go1 = button("Restore my files", { cls: "primary big", icon: "restore" });

  const list = h("div", { class: "choices", role: "radiogroup", "aria-label": "Backup times" });
  const draw = () => {
    const shown = showAll ? versions : versions.slice(0, 8);
    list.replaceChildren(
      ...shown.map((v) => {
        const isSafe = safe?.id === v.id;
        const radio = h("input", { type: "radio", name: "ver", id: `v-${v.id}`, checked: v.id === chosen });
        radio.addEventListener("change", () => {
          chosen = v.id;
          draw();
        });
        return h(
          "label",
          { class: `choice${v.id === chosen ? " selected" : ""}`, for: `v-${v.id}` },
          radio,
          h(
            "span",
            { class: "grow" },
            h("span", { class: "choice-title" }, snapshotLabel(v.time)),
            h("span", { class: "muted small" }, relativeTime(v.time)),
          ),
          isSafe ? h("span", { class: "pill ok" }, "Last good copy") : null,
        );
      }),
      !showAll && versions.length > 8 ? button(`Show ${versions.length - 8} older`, { cls: "quiet", onclick: () => ((showAll = true), draw()) }) : "",
    );
  };
  draw();

  body.append(
    frozenSince && safe
      ? h("div", { class: "notice warn" }, icon("alert", 22), h("div", { class: "notice-body" }, h("p", { class: "notice-title" }, "Backups are paused"), h("p", { class: "muted" }, `We picked the last good copy, from ${inlineTimeLabel(safe.time)}, before the problem started.`)))
      : "",
    step(1, "Pick a time", "Choose how far back to go.", list),
    step(
      2,
      "Choose where the files go",
      "Your files are copied into a new, empty folder. Nothing on your computer is replaced or deleted.",
      h(
        "div",
        { class: "target-row" },
        icon("folder", 22),
        h("div", { class: "grow" }, targetText),
        button("Choose a folder", {
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
    step(3, "Type your password", "This makes sure it is really you.", h("div", {}, h("label", { for: "pw", class: "sr-only" }, "Your Lifeboat password"), pw)),
    h("div", { class: "actions" }, go1),
    result,
  );

  go1.addEventListener("click", async () => {
    if (!chosen) return;
    if (!target) return void result.replaceChildren(errorBox({ kind: "other", message: "Please choose a folder for your files first." }));
    go1.disabled = true;
    result.replaceChildren(h("p", { class: "busy-note" }, "Getting your files back. This can take a while..."));
    try {
      await api.restore(chosen, target, pw.value);
      pw.value = "";
      result.replaceChildren(
        h("div", { class: "notice good" }, icon("check", 22), h("div", { class: "notice-body" }, h("p", { class: "notice-title" }, "Your files are back"), h("p", { class: "muted" }, `You can find them in ${target}`))),
      );
    } catch (e) {
      result.replaceChildren(errorBox(e));
    }
    go1.disabled = false;
  });
}

function step(n: number, title: string, hint: string, content: Child): HTMLElement {
  return h("section", { class: "section" }, h("h2", {}, h("span", { class: "num" }, String(n)), title), h("p", { class: "muted" }, hint), content);
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

const wiz: Wizard = { step: 1, letter: "C", gb: "100", erase: false, folders: [], password: "", password2: "" };

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

async function renderSetup(body: HTMLElement) {
  const draw = () => {
    body.replaceChildren();
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
  const holder = h("div", { class: "section" }, h("p", { class: "muted" }, "Looking for drives..."));
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
  disks.sort((a, b) => Number(diskKind(b) === "external") - Number(diskKind(a) === "external"));
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
      const letter = h("input", { type: "text", id: "letter", maxlength: "1", value: wiz.letter }) as HTMLInputElement;
      const gb = h("input", { type: "number", id: "gb", min: "1", value: wiz.gb }) as HTMLInputElement;
      letter.addEventListener("input", () => (wiz.letter = letter.value));
      gb.addEventListener("input", () => (wiz.gb = gb.value));
      detail.replaceChildren(
        h("div", { class: "notice warn" }, icon("alert", 22), h("div", { class: "notice-body" }, h("p", { class: "notice-title" }, "This is less safe"), h("p", { class: "muted" }, "Lifeboat has to make an existing drive smaller to make room. If this computer breaks, the backups are lost with it. A USB drive is a better choice."))),
        h("div", { class: "fields" }, h("div", {}, h("label", { for: "letter" }, "Which drive letter should be made smaller?"), letter), h("div", {}, h("label", { for: "gb" }, "How many GB should be set aside?"), gb)),
        h("p", { class: "muted small" }, "Pick more than the folders you want to protect take up. It must fit in the free space on that drive; if it does not, Lifeboat will tell you the most it can use."),
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
          h("span", { class: "grow" }, h("span", { class: "choice-title" }, d.model || "Drive"), h("span", { class: "muted small" }, `${sizeLabel(d.size)} - ${ext ? "USB drive" : "Inside this computer"}`)),
          ext ? h("span", { class: "pill ok" }, "Recommended") : null,
        );
      }),
    );
  next.addEventListener("click", () => ((wiz.step = 2), redraw()));
  holder.replaceChildren(
    h("p", { class: "hint" }, icon("info", 18), "A USB drive that stays plugged in is the safest choice. If your computer breaks, your backups survive."),
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
    h("div", { class: "notice info" }, icon("shield", 22), h("div", { class: "notice-body" }, h("p", { class: "notice-title" }, "Windows will ask for permission"), h("p", { class: "muted" }, "To hide the backup drive from other programs, Lifeboat needs administrator rights. When the next window asks \"Do you want to allow this app to make changes?\", choose Yes."))),
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
