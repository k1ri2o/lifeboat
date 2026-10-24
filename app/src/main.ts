import "./style.css";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";
import { api, toApiError, type ApiError, type Disk, type HomeState, type Snapshot } from "./api";
import { lastBefore, relativeTime, sizeLabel, snapshotLabel } from "./format";

type Child = Node | string | null | undefined | false;
type Attrs = Record<string, string | boolean | ((e: Event) => void)>;

/** Tiny DOM builder: h("button", {class: "big", onclick: fn}, "Text"). */
function h(tag: string, attrs: Attrs = {}, ...kids: Child[]): HTMLElement {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (typeof v === "function") el.addEventListener(k.replace(/^on/, ""), v);
    else if (v === true) el.setAttribute(k, "");
    else if (v !== false) el.setAttribute(k, v);
  }
  for (const kid of kids) if (kid) el.append(kid);
  return el;
}

type Screen = "home" | "restore" | "setup";
let screen: Screen = "home";
const app = document.getElementById("app")!;

function serviceDown(): HTMLElement {
  return h(
    "div",
    { class: "banner", role: "alert" },
    "Lifeboat's background service isn't running. ",
    "Restart your computer, or ask the person who set up Lifeboat to start the Lifeboat service.",
  );
}

function errorBox(e: ApiError): HTMLElement {
  return e.kind === "not_running"
    ? serviceDown()
    : h("div", { class: "banner", role: "alert" }, e.message);
}

function nav(): HTMLElement {
  const item = (s: Screen, label: string) =>
    h("button", { type: "button", "aria-current": screen === s ? "page" : "false", onclick: () => go(s) }, label);
  return h("nav", { "aria-label": "Main" }, item("home", "Home"), item("restore", "Restore"), item("setup", "Set up a drive"));
}

function go(s: Screen) {
  screen = s;
  void render();
}

async function render() {
  const body = h("div");
  app.replaceChildren(h("h1", {}, "Lifeboat"), nav(), body);
  try {
    if (screen === "home") await renderHome(body);
    else if (screen === "restore") await renderRestore(body);
    else await renderSetup(body);
  } catch (e) {
    body.replaceChildren(errorBox(toApiError(e)));
  }
}

// ---------- Home ----------

async function renderHome(body: HTMLElement) {
  const st: HomeState = await api.homeState();
  const frozen = st.status.frozen;
  const card = frozen
    ? h("div", { class: "card status bad", role: "status" }, "Backups paused: possible ransomware detected")
    : h(
        "div",
        { class: "card status ok", role: "status" },
        st.last_backup
          ? `Your files are safe. Last backup ${relativeTime(st.last_backup)}`
          : "No backup yet. Press the big button below.",
      );
  body.append(card);
  if (frozen) {
    body.append(
      h("p", {}, "Lifeboat stopped backing up so that your older, good copies stay safe. Do not delete anything."),
      h("button", { type: "button", class: "primary big", onclick: () => go("restore") }, "Go to Restore"),
    );
  }

  const status = h("p", { role: "status", "aria-live": "polite" });
  const btn = h("button", { type: "button", class: "primary big" }, "Back up now") as HTMLButtonElement;
  btn.addEventListener("click", async () => {
    btn.disabled = true;
    status.textContent = "Backing up. This can take a few minutes...";
    try {
      await api.backupNow();
      await render();
    } catch (e) {
      btn.disabled = false;
      status.textContent = "";
      status.replaceChildren(errorBox(toApiError(e)));
    }
  });
  body.append(btn, status);

  body.append(h("h2", {}, "Folders being backed up"));
  const list = h("ul", { class: "plain" });
  const draw = (folders: string[]) => {
    list.replaceChildren(
      ...(folders.length
        ? folders.map((f) =>
            h(
              "li",
              { class: "row" },
              h("span", { class: "grow" }, f),
              h("button", { type: "button", "aria-label": `Remove ${f}`, onclick: async () => draw(await api.removeFolder(f)) }, "Remove"),
            ),
          )
        : [h("li", { class: "muted" }, "No folders chosen yet.")]),
    );
  };
  draw(st.folders);
  body.append(
    list,
    h(
      "button",
      {
        type: "button",
        onclick: async () => {
          const picked = await open({ directory: true, multiple: false, title: "Choose a folder to back up" });
          if (typeof picked === "string") draw(await api.addFolder(picked));
        },
      },
      "Add a folder",
    ),
  );
}

// ---------- Restore ----------

async function renderRestore(body: HTMLElement) {
  const [snaps, home] = await Promise.all([api.snapshots(), api.homeState()]);
  const sorted: Snapshot[] = [...snaps].sort((a, b) => b.time.localeCompare(a.time));
  let chosen: string | undefined = sorted[0]?.id;
  const frozenSince = home.status.frozen ? home.status.frozen_since : null;
  const safe = frozenSince ? lastBefore(sorted, frozenSince) : undefined;

  const result = h("div", { role: "status", "aria-live": "polite" });
  const target = h("input", { type: "text", id: "target", placeholder: "For example C:\\Restored files" }) as HTMLInputElement;
  const pw = h("input", { type: "password", id: "pw", autocomplete: "off" }) as HTMLInputElement;
  const go1 = h("button", { type: "button", class: "primary big" }, "Restore") as HTMLButtonElement;

  const choices = h("div", { role: "radiogroup", "aria-label": "Backups" });
  const drawChoices = () =>
    choices.replaceChildren(
      ...sorted.map((s) => {
        const radio = h("input", { type: "radio", name: "snap", id: `s-${s.id}`, checked: s.id === chosen });
        radio.addEventListener("change", () => {
          chosen = s.id;
          drawChoices();
        });
        return h(
          "div",
          { class: `choice${s.id === chosen ? " selected" : ""}` },
          radio,
          h("label", { for: `s-${s.id}`, style: "margin:0;font-weight:400" }, snapshotLabel(s.time)),
        );
      }),
    );

  if (!sorted.length) {
    body.append(h("p", {}, "There are no backups to restore from yet."));
    return;
  }

  if (frozenSince && safe) {
    body.append(
      h("div", { class: "banner", role: "alert" }, "Backups are paused: possible ransomware detected."),
      h(
        "button",
        {
          type: "button",
          class: "danger big",
          onclick: () => {
            chosen = safe.id;
            drawChoices();
            target.focus();
          },
        },
        `Restore everything to before ${snapshotLabel(frozenSince)}`,
      ),
      h("p", { class: "muted" }, "This picks the last good backup. Then choose a new folder and type your password."),
    );
  }

  const browse = h(
    "button",
    {
      type: "button",
      onclick: async () => {
        const picked = await open({ directory: true, multiple: false, title: "Choose a NEW, empty folder" });
        if (typeof picked === "string") target.value = picked;
      },
    },
    "Choose folder",
  );

  body.append(
    h("h2", {}, "1. Pick a backup"),
    choices,
    h("h2", {}, "2. Pick a NEW, empty folder"),
    h("p", { class: "muted" }, "Your files are copied there. Nothing you have now is replaced."),
    h("label", { for: "target" }, "Folder"),
    h("div", { class: "row" }, h("div", { class: "grow" }, target), browse),
    h("h2", {}, "3. Type your Lifeboat password"),
    h("label", { for: "pw" }, "Password"),
    pw,
    go1,
    result,
  );
  drawChoices();

  go1.addEventListener("click", async () => {
    if (!chosen) return;
    go1.disabled = true;
    result.replaceChildren(h("p", {}, "Restoring your files. Please wait..."));
    try {
      await api.restore(chosen, target.value, pw.value);
      pw.value = "";
      result.replaceChildren(h("div", { class: "card status ok" }, `Done. Your files are in ${target.value}`));
    } catch (e) {
      result.replaceChildren(errorBox(toApiError(e)));
    }
    go1.disabled = false;
  });
}

// ---------- Setup ----------

function diskKind(d: Disk): "external" | "internal" {
  return /usb/i.test(d.bus_type) ? "external" : "internal";
}

async function renderSetup(body: HTMLElement) {
  const disks = (await api.disks()).filter((d) => !d.is_boot && !d.is_system || diskKind(d) === "internal");
  body.append(
    h("p", {}, "Lifeboat keeps your backups on a drive that normal programs, and ransomware, cannot see."),
    h("div", { class: "note" }, h("strong", {}, "External drive (recommended). "), "A USB drive kept plugged in is the safest place. Everything on it will be erased."),
    h("div", { class: "note warn" }, h("strong", {}, "Internal drive. "), "Lifeboat shrinks an existing drive to make room. This is riskier, and a failing computer takes the backups with it."),
    h("div", { class: "note" }, "This step needs administrator permission. Windows may ask you to confirm."),
  );

  let selected: Disk | undefined;
  const detail = h("div");
  const list = h("div", { role: "radiogroup", "aria-label": "Drives" });
  const draw = () =>
    list.replaceChildren(
      ...disks.map((d) => {
        const radio = h("input", { type: "radio", name: "disk", id: `d-${d.number}`, checked: selected?.number === d.number });
        radio.addEventListener("change", () => {
          selected = d;
          draw();
          drawDetail();
        });
        const kind = diskKind(d) === "external" ? "External (recommended)" : "Inside this computer";
        return h(
          "div",
          { class: `choice${selected?.number === d.number ? " selected" : ""}` },
          radio,
          h("label", { for: `d-${d.number}`, style: "margin:0;font-weight:400" }, `${d.model || "Drive"}, ${sizeLabel(d.size)} - ${kind}`),
        );
      }),
    );

  const pw = h("input", { type: "password", id: "spw", autocomplete: "off" }) as HTMLInputElement;
  const pw2 = h("input", { type: "password", id: "spw2", autocomplete: "off" }) as HTMLInputElement;
  const sure = h("input", { type: "checkbox", id: "sure" }) as HTMLInputElement;
  const letter = h("input", { type: "text", id: "letter", maxlength: "1" }) as HTMLInputElement;
  const gb = h("input", { type: "number", id: "gb", min: "1" }) as HTMLInputElement;
  const go1 = h("button", { type: "button", class: "primary big" }, "Prepare this drive") as HTMLButtonElement;
  const result = h("div", { role: "status", "aria-live": "polite" });

  function drawDetail() {
    if (!selected) return detail.replaceChildren();
    const internal = diskKind(selected) === "internal";
    detail.replaceChildren(
      internal
        ? h(
            "div",
            {},
            h("label", { for: "letter" }, "Which drive letter should be shrunk? (for example C)"),
            letter,
            h("label", { for: "gb" }, "How many GB to set aside for backups?"),
            gb,
          )
        : h("label", { class: "check" }, sure, "I understand everything on this drive will be erased."),
      h("label", { for: "spw" }, "Choose a Lifeboat password (at least 8 characters)"),
      pw,
      h("label", { for: "spw2" }, "Type it again"),
      pw2,
      h("p", { class: "muted" }, "Write this password down and keep it safe. Without it your backups cannot be opened."),
      go1,
    );
  }

  go1.addEventListener("click", async () => {
    if (!selected) return;
    const internal = diskKind(selected) === "internal";
    if (!internal && !sure.checked) return void result.replaceChildren(h("div", { class: "banner" }, "Please tick the box to confirm."));
    if (pw.value !== pw2.value) return void result.replaceChildren(h("div", { class: "banner" }, "The two passwords are not the same."));
    go1.disabled = true;
    result.replaceChildren(h("p", {}, "Preparing the drive. Please wait..."));
    try {
      await api.prepareDisk(
        selected.number,
        pw.value,
        internal ? { driveLetter: letter.value, sizeGb: Number(gb.value) } : undefined,
      );
      pw.value = pw2.value = "";
      result.replaceChildren(h("div", { class: "card status ok" }, "Done. Your backup drive is ready."));
    } catch (e) {
      result.replaceChildren(errorBox(toApiError(e)));
    }
    go1.disabled = false;
  });

  body.append(h("h2", {}, "Choose a drive"), disks.length ? list : h("p", {}, "No drives found."), detail, result);
  draw();
}

// ---------- Live updates from the tray / poller ----------

void listen("status-changed", () => screen === "home" && void render());
void listen("backup-done", () => screen === "home" && void render());
void listen("service-down", () => screen === "home" && void render());

void render();
