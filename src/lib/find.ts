// Find & Replace matching for the Text Compare view, as pure functions over
// line arrays.

export interface FindOptions {
  matchCase: boolean;
  wholeWord: boolean;
  regex: boolean;
}

export const DEFAULT_FIND_OPTIONS: FindOptions = { matchCase: false, wholeWord: false, regex: false };

/** One hit: 0-based line, half-open character range. */
export interface LineMatch {
  line: number;
  start: number;
  end: number;
}

const escapeRegex = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

/**
 * Compiles the query into a global regex, or returns an error message for an
 * invalid pattern. An empty query compiles to null (nothing to find).
 */
export function compileQuery(query: string, opts: FindOptions): { re: RegExp | null; error: string | null } {
  if (!query) return { re: null, error: null };
  let source = opts.regex ? query : escapeRegex(query);
  // Lookarounds instead of \b so queries that start or end with punctuation
  // still respect word boundaries.
  if (opts.wholeWord) source = `(?<![\\w])(?:${source})(?![\\w])`;
  try {
    return { re: new RegExp(source, opts.matchCase ? "g" : "gi"), error: null };
  } catch (e) {
    return { re: null, error: e instanceof Error ? e.message : String(e) };
  }
}

/** All non-empty matches of `re` (a global regex) in `lines`, capped at `limit`. */
export function findInLines(lines: string[], re: RegExp, limit = 100_000): LineMatch[] {
  const out: LineMatch[] = [];
  for (let line = 0; line < lines.length && out.length < limit; line++) {
    const text = lines[line];
    re.lastIndex = 0;
    let m: RegExpExecArray | null;
    while ((m = re.exec(text))) {
      if (m[0].length === 0) {
        // Zero-length hits (e.g. `^`) can't be highlighted; step past them.
        re.lastIndex++;
        if (re.lastIndex > text.length) break;
        continue;
      }
      out.push({ line, start: m.index, end: m.index + m[0].length });
      if (out.length >= limit) break;
    }
  }
  re.lastIndex = 0;
  return out;
}

/**
 * Replaces the single match starting at `start` in `text`. Regex replacements
 * may use `$1`-style groups; plain-text replacements are inserted literally.
 * Returns null when the pattern no longer matches there.
 */
export function replaceAt(text: string, re: RegExp, start: number, replacement: string, regex: boolean): string | null {
  const sticky = new RegExp(re.source, re.flags.replace("g", "") + "y");
  sticky.lastIndex = start;
  const m = sticky.exec(text);
  if (!m || m[0].length === 0) return null;
  sticky.lastIndex = start;
  return regex ? text.replace(sticky, replacement) : text.replace(sticky, () => replacement);
}

/**
 * Expands `$$`, `$&`, `` $` ``, `$'`, `$1`…`$99` and `$<name>` in a regex
 * replacement, given the arguments String.replace passes to a replacer.
 */
export function expandReplacement(replacement: string, args: unknown[]): string {
  const hasNamed = typeof args[args.length - 1] === "object" && args[args.length - 1] !== null;
  const named = (hasNamed ? args[args.length - 1] : {}) as Record<string, string | undefined>;
  const tail = hasNamed ? 3 : 2;
  const text = args[args.length - tail + 1] as string;
  const offset = args[args.length - tail] as number;
  const match = args[0] as string;
  const groups = args.slice(1, args.length - tail) as (string | undefined)[];
  return replacement.replace(/\$(\$|&|`|'|\d{1,2}|<([^>]*)>)/g, (tok, what: string, name?: string) => {
    if (what === "$") return "$";
    if (what === "&") return match;
    if (what === "`") return text.slice(0, offset);
    if (what === "'") return text.slice(offset + match.length);
    if (name !== undefined) return hasNamed ? (named[name] ?? "") : tok;
    let idx = Number(what);
    // `$12` with fewer than 12 groups means group 1 followed by a literal "2".
    if (idx > groups.length && what.length === 2) {
      idx = Number(what[0]);
      return idx >= 1 && idx <= groups.length ? (groups[idx - 1] ?? "") + what[1] : tok;
    }
    return idx >= 1 && idx <= groups.length ? (groups[idx - 1] ?? "") : tok;
  });
}

/** Replaces every match in every line; returns the new lines and the count. */
export function replaceAllInLines(
  lines: string[],
  re: RegExp,
  replacement: string,
  regex: boolean,
): { lines: string[]; count: number } {
  let count = 0;
  let next: string[] | null = null;
  for (let i = 0; i < lines.length; i++) {
    const text = lines[i];
    let n = 0;
    re.lastIndex = 0;
    const out = text.replace(re, (...args: unknown[]) => {
      const match = args[0] as string;
      if (!match) return match;
      n++;
      return regex ? expandReplacement(replacement, args) : replacement;
    });
    if (n) {
      next ??= lines.slice();
      next[i] = out;
      count += n;
    }
  }
  re.lastIndex = 0;
  return { lines: next ?? lines, count };
}
