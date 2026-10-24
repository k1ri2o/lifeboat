// Turns raw errors into plain sentences with a next step. The raw text is kept for a "Details" toggle.

export interface FriendlyError {
  /** What went wrong, in plain words. */
  title: string;
  /** What to do next. */
  next: string;
  /** The original text, shown only behind "Details". Empty when there is nothing extra. */
  details: string;
  /** True when the background helper is not reachable (so the screen can offer a retry). */
  unreachable: boolean;
}

interface Raw {
  kind?: string;
  message?: string;
}

const RULES: { test: RegExp; title: string; next: string; unreachable?: boolean }[] = [
  {
    test: /os error 3\b|cannot find the path|path not found/i,
    title: "Lifeboat could not find that folder or drive.",
    next: "Check that your backup drive is plugged in and the folder still exists, then try again.",
  },
  {
    test: /os error 2\b|cannot find the file|no such file/i,
    title: "Lifeboat could not find a file it needed.",
    next: "Make sure the folder was not moved or renamed, then try again.",
  },
  {
    test: /os error 5\b|access is denied|permission denied/i,
    title: "Windows did not allow that.",
    next: "Choose a different folder, or ask the person who set up your computer for help.",
  },
  {
    test: /os error 32\b|being used by another process/i,
    title: "A file is open in another program.",
    next: "Close the other program and try again.",
  },
  {
    test: /os error 112\b|not enough space|no space left|disk full/i,
    title: "There is not enough room on the drive.",
    next: "Free up some space or use a bigger drive, then try again.",
  },
  {
    test: /needs administrator/i,
    title: "This step needs administrator permission.",
    next: 'Close Lifeboat, open it again with "Run as administrator", and repeat this step.',
  },
  {
    test: /too many wrong|locked for \d+ minutes/i,
    title: "Too many wrong passwords.",
    next: "Please wait a few minutes and try again.",
  },
  {
    test: /password (is )?(not right|wrong)|wrong password/i,
    title: "That password is not right.",
    next: "Check it and try again. Too many wrong tries pause password checks for a while.",
  },
  {
    test: /not configured|no repo|not set up/i,
    title: "Lifeboat is not set up yet.",
    next: "Go through the first-time setup to choose where your backups go.",
  },
  {
    test: /pipe|not reachable|isn't running|is not running|service/i,
    title: "Lifeboat's helper is not running, so backups cannot work right now.",
    next: "Restart your computer. If this keeps happening, ask the person who set up Lifeboat for help.",
    unreachable: true,
  },
];

/** Map an ApiError (or anything thrown) to friendly text. */
export function friendlyError(e: unknown): FriendlyError {
  const raw: Raw =
    e && typeof e === "object" ? (e as Raw) : { kind: "other", message: typeof e === "string" ? e : "" };
  const message = (raw.message ?? "").trim();
  if (raw.kind === "not_running") {
    const r = RULES[RULES.length - 1];
    return { title: r.title, next: r.next, details: message, unreachable: true };
  }
  for (const r of RULES) {
    if (r.test.test(message)) {
      return { title: r.title, next: r.next, details: message, unreachable: !!r.unreachable };
    }
  }
  // A short message with no code in it was already written in plain words by the app.
  const plain = message.length > 0 && message.length < 160 && !/error|0x|\\\\|::|\bos\b/i.test(message);
  if (plain) return { title: message, next: "Please try again.", details: "", unreachable: false };
  return {
    title: "Something went wrong.",
    next: "Please try again. If it keeps happening, ask the person who set up Lifeboat for help.",
    details: message,
    unreachable: false,
  };
}
