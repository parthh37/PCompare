export function basename(path: string): string {
  const trimmed = path.replace(/[\\/]+$/, "");
  const idx = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  return idx >= 0 ? trimmed.slice(idx + 1) : trimmed;
}

/** Joins a root with a `/`-separated relative path using the root's separator style. */
export function joinPath(root: string, rel: string): string {
  const sep = root.includes("\\") && !root.includes("/") ? "\\" : "/";
  const base = root.replace(/[\\/]+$/, "");
  return base + sep + rel.split("/").join(sep);
}

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = bytes / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

const dateFmt = new Intl.DateTimeFormat(undefined, {
  year: "numeric",
  month: "2-digit",
  day: "2-digit",
  hour: "2-digit",
  minute: "2-digit",
  second: "2-digit",
});

export function formatTime(ms: number | null): string {
  return ms == null ? "" : dateFmt.format(new Date(ms));
}

/**
 * When something happened, for lists people scan rather than read (Home's
 * saved and recent sessions); the exact time stays in the row's tooltip.
 * `now` is a parameter so the result is testable.
 */
export function formatWhen(ms: number, now: number = Date.now()): string {
  // TODO(you): choose how recency reads, e.g. "Just now" / "5 min ago" /
  // "Today 14:05" / "Yesterday" / "Mon 09:30" / a plain date for older ones.
  // Until then this falls back to the full date and time.
  void now;
  return formatTime(ms);
}

export function hex(n: number, width: number): string {
  return n.toString(16).toUpperCase().padStart(width, "0");
}
