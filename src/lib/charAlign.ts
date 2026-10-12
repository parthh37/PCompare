// Character-level alignment of two lines for the Line Details pane.

/** One aligned column: a character from each side, or null for a gap. */
export interface CharColumn {
  left: string | null;
  right: string | null;
}

/** Above this many DP cells the middle part is aligned position by position. */
const MAX_CELLS = 4_000_000;

/**
 * Aligns two strings character by character (longest common subsequence),
 * so equal characters share a column and insertions face gaps. Characters
 * are compared by code point; `ignoreCase` folds case for the comparison only.
 */
export function alignChars(a: string, b: string, ignoreCase = false): CharColumn[] {
  const l = Array.from(a);
  const r = Array.from(b);
  const eq = ignoreCase
    ? (x: string, y: string) => x === y || x.toLowerCase() === y.toLowerCase()
    : (x: string, y: string) => x === y;

  // Common prefix and suffix need no DP.
  let pre = 0;
  while (pre < l.length && pre < r.length && eq(l[pre], r[pre])) pre++;
  let suf = 0;
  while (suf < l.length - pre && suf < r.length - pre && eq(l[l.length - 1 - suf], r[r.length - 1 - suf])) suf++;

  const out: CharColumn[] = [];
  for (let i = 0; i < pre; i++) out.push({ left: l[i], right: r[i] });
  const lm = l.slice(pre, l.length - suf);
  const rm = r.slice(pre, r.length - suf);
  out.push(...alignMiddle(lm, rm, eq));
  for (let i = suf; i > 0; i--) out.push({ left: l[l.length - i], right: r[r.length - i] });
  return out;
}

function alignMiddle(l: string[], r: string[], eq: (x: string, y: string) => boolean): CharColumn[] {
  const n = l.length;
  const m = r.length;
  if (!n || !m || (n + 1) * (m + 1) > MAX_CELLS) {
    const out: CharColumn[] = [];
    for (let i = 0; i < Math.max(n, m); i++) out.push({ left: l[i] ?? null, right: r[i] ?? null });
    return out;
  }
  // lcs[i][j] = LCS length of l[i..] and r[j..], stored row-major.
  const w = m + 1;
  const lcs = new Uint32Array((n + 1) * w);
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i * w + j] = eq(l[i], r[j]) ? lcs[(i + 1) * w + j + 1] + 1 : Math.max(lcs[(i + 1) * w + j], lcs[i * w + j + 1]);
    }
  }
  const out: CharColumn[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (eq(l[i], r[j])) out.push({ left: l[i++], right: r[j++] });
    else if (lcs[(i + 1) * w + j] >= lcs[i * w + j + 1]) out.push({ left: l[i++], right: null });
    else out.push({ left: null, right: r[j++] });
  }
  while (i < n) out.push({ left: l[i++], right: null });
  while (j < m) out.push({ left: null, right: r[j++] });
  return regroup(out);
}

/**
 * Pairs up each run of one-sided columns: deleted and inserted characters
 * between two matches share columns (a substitution), which reads better than
 * a staircase of half-empty columns.
 */
function regroup(cols: CharColumn[]): CharColumn[] {
  const out: CharColumn[] = [];
  let k = 0;
  while (k < cols.length) {
    if (cols[k].left !== null && cols[k].right !== null) {
      out.push(cols[k++]);
      continue;
    }
    const dels: string[] = [];
    const ins: string[] = [];
    while (k < cols.length && (cols[k].left === null || cols[k].right === null)) {
      if (cols[k].left !== null) dels.push(cols[k].left!);
      else ins.push(cols[k].right!);
      k++;
    }
    for (let i = 0; i < Math.max(dels.length, ins.length); i++) {
      out.push({ left: dels[i] ?? null, right: ins[i] ?? null });
    }
  }
  return out;
}

/** UTF-8 bytes of one character as upper-case hex pairs. */
export function utf8Hex(ch: string): string[] {
  return Array.from(new TextEncoder().encode(ch), (b) => b.toString(16).toUpperCase().padStart(2, "0"));
}
