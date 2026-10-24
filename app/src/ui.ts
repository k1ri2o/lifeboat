// Small UI helpers: a tiny DOM builder, inline SVG icons, and the friendly error box.
import { friendlyError } from "./friendly";

export type Child = Node | string | null | undefined | false;
export type Attrs = Record<string, string | boolean | ((e: Event) => void)>;

/** Tiny DOM builder: h("button", {class: "big", onclick: fn}, "Text"). */
export function h(tag: string, attrs: Attrs = {}, ...kids: Child[]): HTMLElement {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (typeof v === "function") el.addEventListener(k.replace(/^on/, ""), v);
    else if (v === true) el.setAttribute(k, "");
    else if (v !== false) el.setAttribute(k, v);
  }
  for (const kid of kids) if (kid) el.append(kid);
  return el;
}

const PATHS = {
  lifebuoy:
    '<circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="3.5"/><path d="M5.6 5.6l3.9 3.9M14.5 14.5l3.9 3.9M18.4 5.6l-3.9 3.9M9.5 14.5l-3.9 3.9"/>',
  shield: '<path d="M12 3l7 3v5c0 4.5-3 8-7 10-4-2-7-5.5-7-10V6l7-3z"/><path d="M8.7 12l2.4 2.4 4.3-4.8"/>',
  alert: '<path d="M12 4l9 16H3L12 4z"/><path d="M12 10v4M12 17.2v.1"/>',
  pause: '<circle cx="12" cy="12" r="9"/><path d="M10 9v6M14 9v6"/>',
  home: '<path d="M4 11l8-7 8 7"/><path d="M6 10v9h4v-5h4v5h4v-9"/>',
  restore: '<path d="M4 12a8 8 0 1 0 2.5-5.8"/><path d="M4 4v4.5h4.5"/><path d="M12 8v4.5l3 1.8"/>',
  settings:
    '<circle cx="12" cy="12" r="3"/><path d="M12 3v2.5M12 18.5V21M3 12h2.5M18.5 12H21M5.6 5.6l1.8 1.8M16.6 16.6l1.8 1.8M18.4 5.6l-1.8 1.8M7.4 16.6l-1.8 1.8"/>',
  folder: '<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7z"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  trash: '<path d="M5 7h14M10 7V5h4v2M7 7l1 12h8l1-12"/>',
  drive: '<rect x="3" y="8" width="18" height="9" rx="2"/><path d="M7 14h.01M10 14h.01"/><path d="M6 8l1.5-3h9L18 8"/>',
  usb: '<rect x="8" y="3" width="8" height="6" rx="1"/><path d="M10 6h.01M14 6h.01"/><path d="M7 9h10v5a5 5 0 0 1-10 0V9z"/>',
  printer: '<path d="M7 9V4h10v5"/><rect x="4" y="9" width="16" height="8" rx="2"/><path d="M7 14h10v6H7z"/>',
  check: '<path d="M5 12.5l4.5 4.5L19 7.5"/>',
  refresh: '<path d="M20 12a8 8 0 1 1-2.3-5.7"/><path d="M20 4v5h-5"/>',
  lock: '<rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/>',
  info: '<circle cx="12" cy="12" r="9"/><path d="M12 11v5M12 8v.1"/>',
} as const;

export type IconName = keyof typeof PATHS;

/** Inline SVG icon that follows the text colour. Decorative, so hidden from screen readers. */
export function icon(name: IconName, size = 20): HTMLElement {
  const span = document.createElement("span");
  span.className = "icon";
  span.setAttribute("aria-hidden", "true");
  span.innerHTML = `<svg viewBox="0 0 24 24" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round">${PATHS[name]}</svg>`;
  return span;
}

/** A button with an optional leading icon. */
export function button(label: string, opts: { cls?: string; icon?: IconName; onclick?: (e: Event) => void } = {}): HTMLButtonElement {
  const attrs: Attrs = { type: "button", class: `btn ${opts.cls ?? ""}`.trim() };
  if (opts.onclick) attrs.onclick = opts.onclick;
  return h("button", attrs, opts.icon ? icon(opts.icon, 18) : null, h("span", {}, label)) as HTMLButtonElement;
}

/** A friendly error: what happened, what to do, and the raw text behind a small "Details" toggle. */
export function errorBox(e: unknown, onRetry?: () => void): HTMLElement {
  const f = friendlyError(e);
  const box = h(
    "div",
    { class: "notice bad", role: "alert" },
    icon("alert", 22),
    h(
      "div",
      { class: "notice-body" },
      h("p", { class: "notice-title" }, f.title),
      h("p", { class: "muted" }, f.next),
      f.details ? h("details", {}, h("summary", {}, "Details"), h("pre", {}, f.details)) : null,
      onRetry ? button("Try again", { icon: "refresh", onclick: onRetry }) : null,
    ),
  );
  return box;
}
