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
    next: 'Press "Restart as administrator" and repeat this step.',
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

/** True when the error is the "needs administrator" refusal, so the screen can offer a restart button. */
export function needsAdmin(e: unknown): boolean {
  const m = e && typeof e === "object" ? ((e as Raw).message ?? "") : typeof e === "string" ? e : "";
  return /needs administrator/i.test(m);
}

/** Map an ApiError (or anything thrown) to friendly text. */
export function friendlyError(e: unknown): FriendlyError {
  const raw: Raw =
    e && typeof e === "object" ? (e as Raw) : { kind: "other", message: typeof e === "string" ? e : "" };
  const message = (raw.message ?? "").trim();
  if (raw.kind === "not_running") {
    const r = RULES[RULES.length - 1];
    return { title: r.title, next: r.next, details: message, unreachable: true };
  }
  const space = /free space on ([A-Z]):.*?at most ([\d.]+) GiB.*?asked for ([\d.]+) GiB/i.exec(message);
  if (space) {
    const [, drive, most, asked] = space;
    const max = Math.floor(Number(most));
    return {
      title: `Drive ${drive.toUpperCase()}: does not have room for ${Math.round(Number(asked))} GB of backups.`,
      next:
        max >= 1
          ? `Lifeboat can set aside at most ${max} GB there. Go back to step 1 and enter ${max} or less, free up space on ${drive.toUpperCase()}:, or use a USB drive instead.`
          : `There is almost no free room on ${drive.toUpperCase()}:. Free up space there, or use a USB drive instead.`,
      details: message,
      unreachable: false,
    };
  }
  if (/at least 512 MiB/i.test(message)) {
    return { title: "That is too little space for backups.", next: "Go back to step 1 and set aside at least 1 GB.", details: message, unreachable: false };
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
