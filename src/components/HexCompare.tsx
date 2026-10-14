import {
  ArrowDown,
  ArrowUp,
  CaseSensitive,
  ChevronDown,
  ChevronUp,
  Redo2,
  RefreshCw,
  Replace,
  ReplaceAll,
  Save,
  Search,
  Settings2,
  Undo2,
  X,
} from "lucide-react";
import { embedInactive } from "../lib/embeddedPane"; // araxis:folder
import { useCallback, useEffect, useLayoutEffect, useRef, useState, type MouseEvent as ReactMouseEvent } from "react";
import { errorMessage, isRemote, type HexRow, type HexRowFilter, type HexState, type HexView } from "../lib/api";
import { formatSize, hex } from "../lib/format";
import { encodePattern, hexApi, type FindEncoding, type HexSide } from "../lib/hexApi";
import { LAUNCH_READ_ONLY_HINT, type CompareProps } from "../lib/sessionProps";
import { autoRefreshTitle, useAutoRefresh } from "../lib/useAutoRefresh";
import { PathBar } from "./PathBar";
import "./HexCompare.css";

/** Per-session view settings, saved with the session. */
export interface HexSettings {
  filter: HexRowFilter;
  /** Bytes per row, or "fit" to fill the window width. */
  bytesPerRow: number | "fit";
  /** Rows around each difference in the "context" filter. */
  context: number;
  autoRefresh: boolean;
  /** Keep `<name>.bak` when saving. */
  backup: boolean;
}

const DEFAULT_SETTINGS: HexSettings = { filter: "all", bytesPerRow: 16, context: 2, autoRefresh: false, backup: true };
const ROW_WIDTHS = [8, 16, 24, 32] as const;

/** App-wide: files larger than this (per side) are compared offset by offset. */
const LIMIT_KEY = "pcompare.hex.alignLimitMb";
const DEFAULT_LIMIT_MB = 64;
const readLimit = () => {
  const n = Number(localStorage.getItem(LIMIT_KEY));
  return Number.isFinite(n) && n >= 1 ? Math.min(1024, Math.round(n)) : DEFAULT_LIMIT_MB;
};

const ROW_HEIGHT = 20;
const CHUNK_ROWS = 128;
/** Browsers cap element heights; taller content scrolls proportionally. */
const MAX_SCROLL_PX = 8_000_000;
/** Idle time after an edit before the files are fully recompared. */
const REALIGN_DELAY_MS = 450;
const READ_ONLY_HINT = "Remote files are read-only; copy the file locally to edit it";

const ENCODINGS: [FindEncoding, string][] = [
  ["hex", "Hex bytes"],
  ["ascii", "ASCII"],
  ["utf8", "UTF-8"],
  ["utf16le", "UTF-16 LE"],
  ["utf16be", "UTF-16 BE"],
];

interface Cursor {
  side: HexSide;
  /** Byte offset in that side's (edited) content; may equal its size (end of file). */
  offset: number;
  pane: "hex" | "ascii";
  /** Which hex digit the next typed digit replaces. */
  nibble: 0 | 1;
}

interface Cell {
  kind: "byte" | "gap" | "absent";
  b: number;
  /** The byte's offset, or for gaps the offset a typed byte would be inserted at. */
  at: number;
  diff: boolean;
}

function sideCells(row: HexRow, side: HexSide, bpr: number): Cell[] {
  const bytes = side === 0 ? row.left : row.right;
  const other = side === 0 ? row.right : row.left;
  let pos = row.start[side];
  const out: Cell[] = [];
  for (let i = 0; i < bpr; i++) {
    if (i >= bytes.length) out.push({ kind: "absent", b: 0, at: pos, diff: false });
    else if (bytes[i] === null) out.push({ kind: "gap", b: 0, at: pos, diff: true });
    else out.push({ kind: "byte", b: bytes[i]!, at: pos++, diff: bytes[i] !== other[i] });
  }
  return out;
}

const printable = (b: number) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : "·");
const sameBytes = (a: number[], b: number[], fold: boolean) => {
  const low = (x: number) => (fold && x >= 0x41 && x <= 0x5a ? x + 32 : x);
  return a.length === b.length && a.every((x, i) => low(x) === low(b[i]));
};

export function HexCompare({
  left,
  right,
  onPathsChange,
  settings,
  onSettingsChange,
  onDirtyChange,
  readOnlySides, // bc5:reports-script
}: CompareProps<HexSettings>) {
  const [prefs, setPrefs] = useState<HexSettings>({ ...DEFAULT_SETTINGS, ...settings });
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => void reportSettings.current?.(prefs), [prefs]);
  const setPref = <K extends keyof HexSettings>(key: K, value: HexSettings[K]) =>
    setPrefs((p) => ({ ...p, [key]: value }));
  const [limitMb, setLimitMb] = useState(readLimit);

  const baseId = useRef(crypto.randomUUID()).current;
  /** Backend session id; a fresh one per (re)open so a late close can't hit the new session. */
  const sessionKey = useRef("");
  const sid = () => sessionKey.current;
  const [state, setStateRaw] = useState<HexState | null>(null);
  const stateRef = useRef<HexState | null>(null);
  /** Bumped on every (re)open; responses from an older open are dropped. */
  const epoch = useRef(0);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  useEffect(() => {
    if (!message) return;
    const t = setTimeout(() => setMessage(null), 4000);
    return () => clearTimeout(t);
  }, [message]);
  /** Sides changed on disk while they had unsaved edits. */
  const [staleOnDisk, setStaleOnDisk] = useState<[boolean, boolean] | null>(null);
  const [cursor, setCursorRaw] = useState<Cursor | null>(null);
  const cursorRef = useRef<Cursor | null>(null);
  /** Selection anchor on the cursor's side; the selection runs to the cursor. */
  const [anchor, setAnchorRaw] = useState<number | null>(null);
  const anchorRef = useRef<number | null>(null);
  /** Where the last difference jump landed; a cursor at a gap can't say on its own. */
  const navAt = useRef<{ side: HexSide; offset: number; display: number } | null>(null);
  const [insertMode, setInsertMode] = useState(false);
  const [optionsOpen, setOptionsOpen] = useState(false);
  const [, bump] = useState(0);

  const [find, setFind] = useState({
    open: false,
    replace: false,
    text: "",
    replacement: "",
    encoding: "hex" as FindEncoding,
    matchCase: false,
    side: 0 as HexSide,
    status: "",
    focus: 0,
  });
  const findInput = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (find.open) findInput.current?.select();
  }, [find.open, find.focus]);

  const root = useRef<HTMLDivElement>(null);
  const body = useRef<HTMLDivElement>(null);
  const scroller = useRef<HTMLDivElement>(null);
  const [viewport, setViewport] = useState({ width: 800, height: 600, charWidth: 7.5 });
  const [scrollTop, setScrollTop] = useState(0);

  const remote: [boolean, boolean] = [isRemote(left), isRemote(right)];
  // bc5:reports-script — `/ro`, `/lro`, `/rro` lock a side like a remote one.
  const launchLocked: [boolean, boolean] = [!!readOnlySides?.left, !!readOnlySides?.right];
  const readOnly: [boolean, boolean] = [remote[0] || launchLocked[0], remote[1] || launchLocked[1]];
  const readOnlyHint = (side: HexSide) => (launchLocked[side] ? LAUNCH_READ_ONLY_HINT : READ_ONLY_HINT);

  const offWidth = Math.max(8, Math.max(state?.sizes[0] ?? 0, state?.sizes[1] ?? 0).toString(16).length);
  // Row geometry (see HexCompare.css): per side an offset column, 3ch per hex
  // byte plus 1ch per group of 8, and 1ch per ASCII byte, with margins.
  const rowWidthCh = (n: number) => 2 * (offWidth + 4 + n * 4 + Math.floor((n - 1) / 8)) + 4;
  const availCh = (viewport.width - 40) / viewport.charWidth;
  const fitBpr = Math.max(4, Math.min(64, Math.floor((availCh - rowWidthCh(0)) / 8.25 / 4) * 4));
  const bpr = prefs.bytesPerRow === "fit" ? fitBpr : prefs.bytesPerRow;
  const view: HexView = { bytesPerRow: bpr, filter: prefs.filter, context: prefs.context };
  const viewRef = useRef(view);
  viewRef.current = view;
  const viewKey = `${bpr}:${prefs.filter}:${prefs.context}`;

  const setCursor = (c: Cursor | null) => {
    cursorRef.current = c;
    setCursorRaw(c);
  };
  const setAnchor = (a: number | null) => {
    anchorRef.current = a;
    setAnchorRaw(a);
  };
  /** The selected byte range `[start, end)` on the cursor's side, if any. */
  const selection = (): [number, number] | null => {
    const c = cursorRef.current;
    const a = anchorRef.current;
    if (!c || a == null || a === c.offset) return null;
    return [Math.min(a, c.offset), Math.max(a, c.offset)];
  };

  /** Installs a state from the backend unless a newer one already arrived. */
  const accept = useCallback((s: HexState, ep: number) => {
    if (ep !== epoch.current) return;
    const cur = stateRef.current;
    if (cur && s.version < cur.version) return;
    stateRef.current = s;
    setStateRaw(s);
  }, []);

  const dirty = !!state && (state.dirty[0] || state.dirty[1]);
  const reportDirty = useRef(onDirtyChange);
  reportDirty.current = onDirtyChange;
  useEffect(() => void reportDirty.current?.(dirty), [dirty]);

  // ---------- row cache ----------
  const cache = useRef(new Map<number, { key: string; rows: HexRow[] }>());
  const pending = useRef(new Map<number, string>());
  const visible = useRef<[number, number]>([0, 0]);
  const dataKey = () => `${epoch.current}:${stateRef.current?.version}:${viewKey}`;
  const viewKeyRef = useRef(viewKey);
  if (viewKeyRef.current !== viewKey) {
    // Rows laid out for another width or filter can't be shown even briefly.
    viewKeyRef.current = viewKey;
    cache.current.clear();
    pending.current.clear();
  }

  const ensureRows = () => {
    const st = stateRef.current;
    if (!st) return;
    const key = dataKey();
    const ep = epoch.current;
    const [first, last] = visible.current;
    for (let c = Math.floor(first / CHUNK_ROWS); c <= Math.floor(Math.max(first, last - 1) / CHUNK_ROWS); c++) {
      if (c * CHUNK_ROWS >= st.rowCount) break;
      if (cache.current.get(c)?.key === key || pending.current.get(c) === key) continue;
      pending.current.set(c, key);
      const vk = viewKey;
      hexApi
        .rows(sid(), viewRef.current, c * CHUNK_ROWS, CHUNK_ROWS)
        .then((rows) => {
          // Older content still beats a blank row until the fresh page arrives.
          if (ep !== epoch.current || vk !== viewKeyRef.current) return;
          // Don't let a late page of older content replace a fresher one.
          if (key !== dataKey() && cache.current.get(c)?.key === dataKey()) return;
          cache.current.set(c, { key, rows });
          bump((n) => n + 1);
        })
        .catch((e) => setError(errorMessage(e)))
        .finally(() => pending.current.get(c) === key && pending.current.delete(c));
    }
  };
  useEffect(ensureRows);

  const rowAt = (index: number): HexRow | undefined => {
    const chunk = cache.current.get(Math.floor(index / CHUNK_ROWS));
    return chunk?.rows[index - Math.floor(index / CHUNK_ROWS) * CHUNK_ROWS];
  };
  /** A cached row showing byte `offset` of `side`, with the cell's column. */
  const cachedCell = (side: HexSide, offset: number): { row: HexRow; col: number } | null => {
    for (const chunk of cache.current.values()) {
      if (chunk.key !== dataKey()) continue;
      for (const row of chunk.rows) {
        const bytes = side === 0 ? row.left : row.right;
        const count = bytes.reduce<number>((n, b) => n + (b === null ? 0 : 1), 0);
        if (offset < row.start[side] || offset >= row.start[side] + count) continue;
        const col = sideCells(row, side, bpr).findIndex((c) => c.kind === "byte" && c.at === offset);
        return { row, col };
      }
    }
    return null;
  };

  // ---------- scrolling ----------
  const rowCount = state?.rowCount ?? 0;
  const totalPx = rowCount * ROW_HEIGHT;
  const spacerPx = Math.min(totalPx, MAX_SCROLL_PX);
  const maxScroll = Math.max(1, spacerPx - viewport.height);
  const ratio = totalPx > MAX_SCROLL_PX ? (totalPx - viewport.height) / maxScroll : 1;
  const virtualTop = scrollTop * ratio;
  const firstRow = Math.max(0, Math.floor(virtualTop / ROW_HEIGHT) - 4);
  const lastRow = Math.min(rowCount, Math.ceil((virtualTop + viewport.height) / ROW_HEIGHT) + 4);
  visible.current = [firstRow, lastRow];
  const ratioRef = useRef(ratio);
  ratioRef.current = ratio;

  useLayoutEffect(() => {
    const el = scroller.current;
    const host = body.current;
    if (!el || !host) return;
    const probe = document.createElement("span");
    probe.className = "hx-probe";
    probe.textContent = "0".repeat(100);
    el.appendChild(probe);
    const charWidth = probe.getBoundingClientRect().width / 100 || 7.5;
    probe.remove();
    const measure = () => setViewport({ width: host.clientWidth, height: el.clientHeight, charWidth });
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    ro.observe(host);
    measure();
    // Scaled scrolling: one wheel notch should still move a few rows.
    const onWheel = (e: WheelEvent) => {
      if (ratioRef.current <= 1 || Math.abs(e.deltaY) < Math.abs(e.deltaX)) return;
      e.preventDefault();
      el.scrollTop += e.deltaY / ratioRef.current;
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => {
      ro.disconnect();
      el.removeEventListener("wheel", onWheel);
    };
  }, []);

  /** Row to bring into view after the next render (once the spacer has its new height). */
  const scrollTarget = useRef<number | null>(null);
  const scrollToRow = (row: number) => {
    scrollTarget.current = row;
    bump((n) => n + 1);
  };
  useLayoutEffect(() => {
    const el = scroller.current;
    const row = scrollTarget.current;
    if (!el || row == null) return;
    scrollTarget.current = null;
    const top = row * ROW_HEIGHT;
    const vt = el.scrollTop * ratio;
    if (top >= vt && top + ROW_HEIGHT <= vt + el.clientHeight) return;
    el.scrollTop = Math.max(0, (top - el.clientHeight / 3) / ratio);
  });

  /** Scrolls so the cursor is visible. */
  const reveal = async (c: Cursor) => {
    const hit = cachedCell(c.side, c.offset);
    if (hit) return scrollToRow(hit.row.index);
    const t = await hexApi.locate(sid(), viewRef.current, c.side, c.offset);
    if (t.row != null) scrollToRow(t.row);
  };

  const moveTo = (c: Cursor, extend = false) => {
    const size = stateRef.current?.sizes[c.side] ?? 0;
    const next = { ...c, offset: Math.max(0, Math.min(size, c.offset)) };
    const prev = cursorRef.current;
    if (extend && prev && prev.side === next.side) {
      if (anchorRef.current == null) setAnchor(prev.offset);
    } else setAnchor(null);
    setCursor(next);
    reveal(next).catch((e) => setError(errorMessage(e)));
  };

  // ---------- opening ----------
  /** Edits and other state-changing calls run one at a time, in order. */
  const queue = useRef<Promise<void>>(Promise.resolve());
  const run = (op: () => Promise<void>) => {
    queue.current = queue.current.then(op).catch((e) => setError(errorMessage(e)));
  };

  const open = useCallback(
    async (keepPosition: boolean) => {
      if (!left || !right) return;
      const ep = ++epoch.current;
      const previous = sessionKey.current;
      sessionKey.current = `${baseId}-${ep}`;
      if (previous) hexApi.close(previous).catch(() => {});
      setLoading(true);
      try {
        const key = sid();
        const s = await hexApi.open(key, left, right, limitMb, viewRef.current);
        if (ep !== epoch.current) {
          // Superseded (or unmounted) while opening; don't leave it open in the backend.
          if (sessionKey.current !== key) hexApi.close(key).catch(() => {});
          return;
        }
        cache.current.clear();
        pending.current.clear();
        stateRef.current = null;
        accept(s, ep);
        setError(null);
        setStaleOnDisk(null);
        if (keepPosition && cursorRef.current) {
          const c = cursorRef.current;
          setCursor({ ...c, offset: Math.min(c.offset, s.sizes[c.side]), nibble: 0 });
        } else {
          setAnchor(null);
          const first = s.identical ? null : await hexApi.navigate(sid(), viewRef.current, null, true);
          if (ep !== epoch.current) return;
          setCursor({ side: 0, offset: first?.start[0] ?? 0, pane: "hex", nibble: 0 });
          navAt.current = first ? { side: 0, offset: first.start[0], display: first.display } : null;
          scrollToRow(first?.row ?? 0);
        }
      } catch (e) {
        if (ep === epoch.current) setError(errorMessage(e));
      } finally {
        if (ep === epoch.current) setLoading(false);
      }
    },
    [left, right, baseId, limitMb, accept],
  );

  useEffect(() => {
    setStateRaw(null);
    stateRef.current = null;
    setCursor(null);
    open(false);
  }, [left, right]);

  useEffect(
    () => () => {
      if (sessionKey.current) hexApi.close(sessionKey.current).catch(() => {});
      sessionKey.current = "";
    },
    [],
  );

  // The row count depends on the view; refresh it when the view changes.
  useEffect(() => {
    if (!stateRef.current) return;
    const ep = epoch.current;
    hexApi
      .state(sid(), view)
      .then((s) => {
        accept(s, ep);
        if (cursorRef.current) reveal(cursorRef.current).catch(() => {});
      })
      .catch((e) => setError(errorMessage(e)));
  }, [viewKey]);

  // After edits settle, recompare in full so the alignment catches up.
  useEffect(() => {
    if (!state?.stale) return;
    const ep = epoch.current;
    const t = setTimeout(() => {
      hexApi
        .realign(sid(), limitMb, viewRef.current)
        .then((s) => accept(s, ep))
        .catch((e) => setError(errorMessage(e)));
    }, REALIGN_DELAY_MS);
    return () => clearTimeout(t);
  }, [state?.version, state?.stale, limitMb, accept]);

  const confirmDiscard = (action: string) =>
    !dirty || window.confirm(`You have unsaved changes. Discard them and ${action}?`);

  const refresh = () => {
    if (confirmDiscard("reload both files")) open(true);
  };

  const changePaths = (l: string, r: string) => {
    if (confirmDiscard("open the new files")) onPathsChange(l, r);
  };

  const onDiskChange = async () => {
    const st = stateRef.current;
    if (!st) return;
    if (remote[0] || remote[1]) {
      // Remote sides are polled; reload unless that would lose edits.
      if (!st.dirty[0] && !st.dirty[1]) open(true);
      return;
    }
    const changed = await hexApi.diskChanged(sid()).catch(() => [true, true] as [boolean, boolean]);
    if (!changed[0] && !changed[1]) return; // e.g. our own save
    // Reloading would throw away unsaved edits; ask first.
    if (st.dirty[0] || st.dirty[1]) setStaleOnDisk(changed);
    else open(true);
  };
  const watchState = useAutoRefresh({ enabled: prefs.autoRefresh, paths: [left, right], onChange: onDiskChange });

  // ---------- editing ----------
  const edit = async (side: HexSide, offset: number, remove: number, insert: number[]) => {
    const ep = epoch.current;
    accept(await hexApi.edit(sid(), side, offset, remove, insert, viewRef.current), ep);
  };

  const blockedEdit = (side: HexSide) => {
    if (!readOnly[side]) return false;
    setMessage(readOnlyHint(side));
    return true;
  };

  const typeHexDigit = (d: number) =>
    run(async () => {
      const c = cursorRef.current;
      const st = stateRef.current;
      if (!c || !st || blockedEdit(c.side)) return;
      const sel = selection();
      if (sel) {
        await edit(c.side, sel[0], sel[1] - sel[0], [d << 4]);
        setAnchor(null);
        setCursor({ ...c, offset: sel[0], pane: "hex", nibble: 1 });
        return;
      }
      if (c.nibble === 0) {
        if (insertMode || c.offset >= st.sizes[c.side]) await edit(c.side, c.offset, 0, [d << 4]);
        else {
          const [old] = await hexApi.read(sid(), c.side, c.offset, 1);
          await edit(c.side, c.offset, 1, [(d << 4) | (old & 0x0f)]);
        }
        setCursor({ ...c, pane: "hex", nibble: 1 });
      } else {
        const [old = 0] = await hexApi.read(sid(), c.side, c.offset, 1);
        await edit(c.side, c.offset, 1, [(old & 0xf0) | d]);
        const next = { ...c, offset: c.offset + 1, pane: "hex" as const, nibble: 0 as const };
        setCursor(next);
        reveal(next).catch(() => {});
      }
    });

  /** Inserts or overwrites `bytes` at the cursor (replacing any selection). */
  const typeBytes = (bytes: number[]) =>
    run(async () => {
      const c = cursorRef.current;
      const st = stateRef.current;
      if (!c || !st || !bytes.length || blockedEdit(c.side)) return;
      const sel = selection();
      const at = sel ? sel[0] : c.offset;
      const remove = sel ? sel[1] - sel[0] : insertMode ? 0 : Math.min(bytes.length, st.sizes[c.side] - c.offset);
      await edit(c.side, at, remove, bytes);
      setAnchor(null);
      const next = { ...c, offset: at + bytes.length, nibble: 0 as const };
      setCursor(next);
      reveal(next).catch(() => {});
    });

  const deleteBytes = (backward: boolean) =>
    run(async () => {
      const c = cursorRef.current;
      const st = stateRef.current;
      if (!c || !st || blockedEdit(c.side)) return;
      const sel = selection();
      let at: number;
      let len: number;
      if (sel) [at, len] = [sel[0], sel[1] - sel[0]];
      else if (backward) [at, len] = [c.offset - 1, 1];
      else [at, len] = [c.offset, 1];
      if (at < 0 || at >= st.sizes[c.side]) return;
      await edit(c.side, at, len, []);
      setAnchor(null);
      const next = { ...c, offset: at, nibble: 0 as const };
      setCursor(next);
      reveal(next).catch(() => {});
    });

  const undoRedo = (redo: boolean) =>
    run(async () => {
      const ep = epoch.current;
      const res = await (redo ? hexApi.redo : hexApi.undo)(sid(), viewRef.current);
      accept(res.state, ep);
      if (res.at) {
        setAnchor(null);
        const next: Cursor = {
          side: res.at.side,
          offset: res.at.offset,
          pane: cursorRef.current?.pane ?? "hex",
          nibble: 0,
        };
        setCursor(next);
        reveal(next).catch(() => {});
      }
    });

  const save = () =>
    run(async () => {
      const st = stateRef.current;
      if (!st) return;
      const ep = epoch.current;
      const sides = ([0, 1] as HexSide[]).filter((s) => st.dirty[s] && !readOnly[s]);
      for (const s of sides) accept(await hexApi.save(sid(), s, prefs.backup, viewRef.current), ep);
      if (sides.length) setMessage(`Saved ${sides.map((s) => (s === 0 ? "left" : "right")).join(" and ")}`);
    });

  const copySelection = () =>
    run(async () => {
      const c = cursorRef.current;
      const sel = selection();
      if (!c || !sel) return;
      const bytes = await hexApi.read(sid(), c.side, sel[0], sel[1] - sel[0]);
      const text =
        c.pane === "hex"
          ? bytes.map((b) => hex(b, 2)).join(" ")
          : new TextDecoder("utf-8").decode(new Uint8Array(bytes));
      await navigator.clipboard.writeText(text);
      if (sel[1] - sel[0] > bytes.length) setMessage(`Copied the first ${formatSize(bytes.length)}`);
    });

  // ---------- navigation ----------
  const cursorDisplay = async (): Promise<number | null> => {
    const c = cursorRef.current;
    if (!c) return null;
    const nav = navAt.current;
    if (nav && nav.side === c.side && nav.offset === c.offset) return nav.display;
    const hit = cachedCell(c.side, c.offset);
    if (hit && hit.col >= 0) return hit.row.row * bpr + hit.col;
    return (await hexApi.locate(sid(), viewRef.current, c.side, c.offset)).display;
  };

  const goDifference = (forward: boolean) =>
    run(async () => {
      const from = await cursorDisplay();
      const t = await hexApi.navigate(sid(), viewRef.current, from, forward);
      if (!t) {
        setMessage(forward ? "No more differences below" : "No more differences above");
        return;
      }
      const side = cursorRef.current?.side ?? 0;
      setAnchor(null);
      setCursor({ side, offset: t.start[side], pane: cursorRef.current?.pane ?? "hex", nibble: 0 });
      navAt.current = { side, offset: t.start[side], display: t.display };
      if (t.row != null) scrollToRow(t.row);
    });

  /** Moves the cursor to the same column one row up or down (`delta` rows). */
  const moveRows = (delta: number, extend: boolean) => {
    const c = cursorRef.current;
    if (!c) return;
    const hit = cachedCell(c.side, c.offset);
    const target = hit && hit.col >= 0 ? rowAt(hit.row.index + delta) : undefined;
    if (hit && target) {
      const cell = sideCells(target, c.side, bpr)[hit.col];
      moveTo({ ...c, offset: cell.at, nibble: 0 }, extend);
    } else moveTo({ ...c, offset: c.offset + delta * bpr, nibble: 0 }, extend);
  };

  // ---------- find & replace ----------
  const findBytes = (): number[] | null => {
    try {
      const p = encodePattern(find.text, find.encoding);
      return p.length ? p : null;
    } catch (e) {
      setFind((f) => ({ ...f, status: errorMessage(e) }));
      return null;
    }
  };
  const foldCase = !find.matchCase && (find.encoding === "ascii" || find.encoding === "utf8");

  const findCore = async (pattern: number[], forward: boolean) => {
    const st = stateRef.current;
    if (!st) return;
    const side = find.side;
    const c = cursorRef.current;
    const sel = c?.side === side ? selection() : null;
    const size = st.sizes[side];
    const from = sel ? (forward ? sel[0] + 1 : sel[0]) : c?.side === side ? c.offset : forward ? 0 : size;
    let at = await hexApi.find(sid(), side, pattern, from, forward, foldCase);
    let status = "";
    if (at == null) {
      at = await hexApi.find(sid(), side, pattern, forward ? 0 : size, forward, foldCase);
      status = at == null ? "Not found" : forward ? "Wrapped to the start" : "Wrapped to the end";
    }
    setFind((f) => ({ ...f, status }));
    if (at == null) return;
    const next: Cursor = { side, offset: at + pattern.length, pane: c?.pane ?? "hex", nibble: 0 };
    setCursor(next);
    setAnchor(at);
    await reveal({ ...next, offset: at });
  };

  const findNext = (forward: boolean) => {
    const pattern = findBytes();
    if (pattern) run(() => findCore(pattern, forward));
  };

  const replaceBytes = (): number[] | null => {
    try {
      return encodePattern(find.replacement, find.encoding);
    } catch (e) {
      setFind((f) => ({ ...f, status: errorMessage(e) }));
      return null;
    }
  };

  const replaceOne = () => {
    const pattern = findBytes();
    const replacement = replaceBytes();
    if (!pattern || !replacement || blockedEdit(find.side)) return;
    run(async () => {
      const c = cursorRef.current;
      const sel = selection();
      if (c && sel && c.side === find.side && sel[1] - sel[0] === pattern.length) {
        const current = await hexApi.read(sid(), c.side, sel[0], pattern.length);
        if (sameBytes(current, pattern, foldCase)) {
          await edit(c.side, sel[0], pattern.length, replacement);
          setAnchor(null);
          setCursor({ ...c, offset: sel[0] + replacement.length, nibble: 0 });
        }
      }
      await findCore(pattern, true);
    });
  };

  const replaceAll = () => {
    const pattern = findBytes();
    const replacement = replaceBytes();
    if (!pattern || !replacement || blockedEdit(find.side)) return;
    run(async () => {
      const ep = epoch.current;
      const res = await hexApi.replaceAll(sid(), find.side, pattern, replacement, foldCase, viewRef.current);
      accept(res.state, ep);
      setAnchor(null);
      const limited = res.count >= 1_000_000 ? " (stopped at the limit; run again for more)" : "";
      setFind((f) => ({ ...f, status: `Replaced ${res.count.toLocaleString()}${limited}` }));
    });
  };

  const openFind = (replace: boolean) =>
    setFind((f) => ({
      ...f,
      open: true,
      replace: replace || f.replace,
      side: cursorRef.current?.side ?? f.side,
      focus: f.focus + 1,
    }));

  // ---------- keyboard ----------
  // Re-registered every render so the handler always sees current state.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Inactive tabs stay mounted but hidden; only the visible session reacts.
      if (e.defaultPrevented || !root.current?.offsetParent) return;
      if (embedInactive(root.current)) return; // araxis:folder — split view: the folder list has the keys
      const mod = e.ctrlKey || e.metaKey;
      const key = e.key.toLowerCase();
      const t = e.target as HTMLElement | null;
      const inField = !!t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.tagName === "SELECT");
      const c = cursorRef.current;
      const shift = e.shiftKey;
      if (mod && !shift && !e.altKey && key === "s") save();
      else if (mod && !shift && key === "f") openFind(false);
      else if (mod && !shift && key === "h") openFind(true);
      else if (e.key === "F3") findNext(!shift);
      else if (e.key === "F5") refresh();
      else if (e.altKey && e.key === "ArrowDown") goDifference(true);
      else if (e.altKey && e.key === "ArrowUp") goDifference(false);
      else if (inField) return;
      else if (mod && !shift && key === "z") undoRedo(false);
      else if (mod && (key === "y" || (shift && key === "z"))) undoRedo(true);
      else if (mod && key === "c") copySelection();
      else if (mod && key === "a" && c) {
        setAnchor(0);
        setCursor({ ...c, offset: stateRef.current?.sizes[c.side] ?? 0, nibble: 0 });
      } else if (e.key === "Escape") {
        if (find.open) setFind((f) => ({ ...f, open: false }));
        else setAnchor(null);
      } else if (e.key === "Insert") setInsertMode((m) => !m);
      else if (!c) return;
      else if (e.key === "Tab") setCursor({ ...c, pane: c.pane === "hex" ? "ascii" : "hex", nibble: 0 });
      else if (e.key === "ArrowLeft")
        moveTo({ ...c, offset: c.offset - (c.nibble && !shift ? 0 : 1), nibble: 0 }, shift);
      else if (e.key === "ArrowRight") moveTo({ ...c, offset: c.offset + 1, nibble: 0 }, shift);
      else if (e.key === "ArrowUp") moveRows(-1, shift);
      else if (e.key === "ArrowDown") moveRows(1, shift);
      else if (e.key === "PageUp" || e.key === "PageDown") {
        const rows = Math.max(1, Math.floor(viewport.height / ROW_HEIGHT) - 1);
        moveTo({ ...c, offset: c.offset + (e.key === "PageUp" ? -rows : rows) * bpr, nibble: 0 }, shift);
      } else if (e.key === "Home" || e.key === "End") {
        const end = e.key === "End";
        if (mod) moveTo({ ...c, offset: end ? Infinity : 0, nibble: 0 }, shift);
        else {
          const hit = cachedCell(c.side, c.offset);
          const cells = hit ? sideCells(hit.row, c.side, bpr).filter((x) => x.kind === "byte") : [];
          if (!cells.length) return;
          moveTo({ ...c, offset: end ? cells[cells.length - 1].at : cells[0].at, nibble: 0 }, shift);
        }
      } else if (e.key === "Backspace") deleteBytes(true);
      else if (e.key === "Delete") deleteBytes(false);
      else if (mod || e.altKey) return;
      else if (c.pane === "hex" && /^[0-9a-f]$/.test(key)) typeHexDigit(parseInt(key, 16));
      else if (c.pane === "ascii" && e.key.length === 1) typeBytes(Array.from(new TextEncoder().encode(e.key)));
      else return;
      e.preventDefault();
    };
    const onPaste = (e: ClipboardEvent) => {
      const t = e.target as HTMLElement | null;
      if (!root.current?.offsetParent || !cursorRef.current) return;
      if (embedInactive(root.current)) return; // araxis:folder — split view: the folder list has the keys
      if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA")) return;
      const text = e.clipboardData?.getData("text") ?? "";
      if (!text) return;
      e.preventDefault();
      let bytes: number[];
      try {
        bytes = cursorRef.current.pane === "hex" ? encodePattern(text, "hex") : encodePattern(text, "utf8");
      } catch {
        bytes = encodePattern(text, "utf8");
      }
      typeBytes(bytes);
    };
    window.addEventListener("keydown", onKey);
    window.addEventListener("paste", onPaste);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("paste", onPaste);
    };
  });

  // ---------- mouse ----------
  /** Offset where a mouse drag started, while the button is down. */
  const dragOrigin = useRef<number | null>(null);
  const cellFromEvent = (e: ReactMouseEvent): Cursor | null => {
    const el = (e.target as HTMLElement).closest<HTMLElement>("[data-at]");
    if (!el) return null;
    return {
      side: Number(el.dataset.side) as HexSide,
      offset: Number(el.dataset.at),
      pane: el.dataset.pane === "ascii" ? "ascii" : "hex",
      nibble: 0,
    };
  };
  const onMouseDown = (e: ReactMouseEvent) => {
    if (e.button !== 0) return;
    const hit = cellFromEvent(e);
    if (!hit) return;
    e.preventDefault();
    (document.activeElement as HTMLElement | null)?.blur?.();
    const prev = cursorRef.current;
    if (e.shiftKey && prev && prev.side === hit.side) {
      if (anchorRef.current == null) setAnchor(prev.offset);
    } else setAnchor(hit.offset);
    setCursor(hit);
    dragOrigin.current = e.shiftKey ? null : hit.offset;
    const up = () => {
      dragOrigin.current = null;
      if (anchorRef.current === cursorRef.current?.offset) setAnchor(null);
      window.removeEventListener("mouseup", up);
    };
    window.addEventListener("mouseup", up);
  };
  const onMouseOver = (e: ReactMouseEvent) => {
    const origin = dragOrigin.current;
    const hit = cellFromEvent(e);
    const c = cursorRef.current;
    // Selections stay on one side and include both the start and hovered bytes.
    if (origin == null || !hit || !c || hit.side !== c.side) return;
    // Wobbling inside the clicked byte isn't a drag yet.
    if (hit.offset === origin && c.offset === origin) return;
    const forward = hit.offset >= origin;
    setAnchor(forward ? origin : origin + 1);
    setCursor({ ...hit, pane: c.pane, offset: forward ? hit.offset + 1 : hit.offset });
  };

  // ---------- rendering ----------
  const sel =
    cursor && anchor != null && anchor !== cursor.offset
      ? [Math.min(anchor, cursor.offset), Math.max(anchor, cursor.offset)]
      : null;

  const renderSide = (row: HexRow, side: HexSide) => {
    const cells = sideCells(row, side, bpr);
    const size = state?.sizes[side] ?? 0;
    const onSide = cursor?.side === side;
    const isSel = (c: Cell) => onSide && !!sel && c.kind === "byte" && c.at >= sel[0] && c.at < sel[1];
    // The caret sits on its byte, or past the last byte on the first empty cell.
    let eofDrawn = false;
    const isCursor = (c: Cell) => {
      if (!onSide || !cursor) return false;
      if (c.kind === "byte") return c.at === cursor.offset;
      if (cursor.offset !== size || c.at !== size || eofDrawn) return false;
      eofDrawn = true;
      return true;
    };
    const hexCells = cells.map((c, i) => {
      const cur = isCursor(c);
      const cls =
        `hx-b ${c.kind}` +
        (c.diff ? " diff" : "") +
        (isSel(c) ? " sel" : "") +
        (cur ? ` cur${cursor?.pane === "hex" ? " focus" : ""}` : "") +
        (i % 8 === 7 && i < bpr - 1 ? " grp" : "");
      const text = c.kind === "byte" ? hex(c.b, 2) : c.kind === "gap" ? "··" : "  ";
      return (
        <span key={i} className={cls} data-side={side} data-at={c.at} data-pane="hex">
          {cur && cursor?.pane === "hex" && c.kind === "byte" ? (
            <>
              <span className={cursor.nibble === 0 ? "nib" : undefined}>{text[0]}</span>
              <span className={cursor.nibble === 1 ? "nib" : undefined}>{text[1]}</span>
            </>
          ) : (
            text
          )}
        </span>
      );
    });
    eofDrawn = false;
    const asciiCells = cells.map((c, i) => {
      const cur = isCursor(c);
      const cls =
        `hx-a ${c.kind}` +
        (c.diff ? " diff" : "") +
        (isSel(c) ? " sel" : "") +
        (cur ? ` cur${cursor?.pane === "ascii" ? " focus" : ""}` : "");
      return (
        <span key={i} className={cls} data-side={side} data-at={c.at} data-pane="ascii">
          {c.kind === "byte" ? printable(c.b) : " "}
        </span>
      );
    });
    const present = cells.some((c) => c.kind === "byte");
    return (
      <>
        <span className="hx-off" style={{ width: `${offWidth}ch` }}>
          {present ? hex(row.start[side], offWidth) : ""}
        </span>
        <span className="hx-hex">{hexCells}</span>
        <span className="hx-ascii">{asciiCells}</span>
      </>
    );
  };

  const renderRow = (index: number) => {
    const row = rowAt(index);
    if (!row) return <div className="hx-row loading">…</div>;
    const differs = row.left.length !== row.right.length || row.left.some((b, i) => b !== row.right[i]);
    return (
      <div className={`hx-row${differs ? " differs" : ""}${row.breakBefore ? " break" : ""}`}>
        {renderSide(row, 0)}
        <span className="hx-mid" />
        {renderSide(row, 1)}
      </div>
    );
  };

  const items = [];
  for (let i = firstRow; i < lastRow; i++) {
    items.push(
      <div key={i} className="hx-vrow" style={{ top: i * ROW_HEIGHT - virtualTop, height: ROW_HEIGHT }}>
        {renderRow(i)}
      </div>,
    );
  }

  const canSave = !!state && ([0, 1] as HexSide[]).some((s) => state.dirty[s] && !readOnly[s]);
  const selLen = sel ? sel[1] - sel[0] : 0;

  return (
    <div className="session hex-session" ref={root}>
      <div className="toolbar">
        <button
          className="icon-btn"
          onClick={() => goDifference(false)}
          disabled={!state || state.identical}
          title="Previous difference (Alt+↑)"
        >
          <ArrowUp size={14} />
        </button>
        <button onClick={() => goDifference(true)} disabled={!state || state.identical} title="Next difference (Alt+↓)">
          <ArrowDown size={14} /> Next difference
        </button>
        <span className="sep" />
        <div className="seg-control" title="Which rows to show">
          {(
            [
              ["all", "All"],
              ["differences", "Diffs"],
              ["context", "Context"],
            ] as [HexRowFilter, string][]
          ).map(([f, label]) => (
            <button key={f} className={prefs.filter === f ? "active" : ""} onClick={() => setPref("filter", f)}>
              {label}
            </button>
          ))}
        </div>
        <label title="Bytes shown per row">
          Row
          <select
            value={String(prefs.bytesPerRow)}
            onChange={(e) => setPref("bytesPerRow", e.target.value === "fit" ? "fit" : Number(e.target.value))}
          >
            {ROW_WIDTHS.map((w) => (
              <option key={w} value={w}>
                {w} bytes
              </option>
            ))}
            <option value="fit">Fit to width ({fitBpr})</option>
          </select>
        </label>
        <span className="sep" />
        <button className="icon-btn" onClick={() => undoRedo(false)} disabled={!state?.canUndo} title="Undo (Ctrl+Z)">
          <Undo2 size={14} />
        </button>
        <button
          className="icon-btn"
          onClick={() => undoRedo(true)}
          disabled={!state?.canRedo}
          title="Redo (Ctrl+Y or Ctrl+Shift+Z)"
        >
          <Redo2 size={14} />
        </button>
        <button
          onClick={save}
          disabled={!canSave}
          title={readOnly[0] && readOnly[1] ? readOnlyHint(0) : "Save modified files (Ctrl+S)"}
        >
          <Save size={14} /> Save
        </button>
        <button
          className={`icon-btn${insertMode ? " active" : ""}`}
          onClick={() => setInsertMode((m) => !m)}
          title="Typing inserts bytes (on) or overwrites them (off) — Insert key"
        >
          {insertMode ? "INS" : "OVR"}
        </button>
        <button className="icon-btn" onClick={() => openFind(false)} title="Find and replace (Ctrl+F, Ctrl+H)">
          <Search size={14} />
        </button>
        <span className="spacer" />
        <div className="hx-options-anchor">
          <button
            className={`icon-btn${optionsOpen ? " active" : ""}`}
            onClick={() => setOptionsOpen((o) => !o)}
            title="Hex compare options"
          >
            <Settings2 size={14} />
          </button>
          {optionsOpen && (
            <div className="hx-options" onMouseLeave={() => setOptionsOpen(false)}>
              <label className="check">
                <input type="checkbox" checked={prefs.backup} onChange={(e) => setPref("backup", e.target.checked)} />
                Keep a .bak copy when saving
              </label>
              <label>
                Context rows
                <input
                  type="number"
                  min={0}
                  max={50}
                  value={prefs.context}
                  onChange={(e) => setPref("context", Math.max(0, Math.min(50, Number(e.target.value) || 0)))}
                />
              </label>
              <label title="Larger files are compared offset by offset (applies to all hex sessions)">
                Align files up to (MB)
                <input
                  type="number"
                  min={1}
                  max={1024}
                  value={limitMb}
                  onChange={(e) => {
                    const mb = Math.max(1, Math.min(1024, Math.round(Number(e.target.value) || DEFAULT_LIMIT_MB)));
                    localStorage.setItem(LIMIT_KEY, String(mb));
                    setLimitMb(mb);
                  }}
                />
              </label>
              <button
                onClick={() => {
                  setOptionsOpen(false);
                  const ep = epoch.current;
                  run(async () => accept(await hexApi.realign(sid(), limitMb, viewRef.current), ep));
                }}
              >
                Recompare now
              </button>
            </div>
          )}
        </div>
        <label
          className={`check${watchState === "watching" || watchState === "polling" ? " live" : ""}`}
          title={autoRefreshTitle(watchState)}
        >
          <input
            type="checkbox"
            checked={prefs.autoRefresh}
            onChange={(e) => setPref("autoRefresh", e.target.checked)}
          />
          Auto-refresh
        </label>
        <button onClick={refresh} title="Reload both files (F5)">
          <RefreshCw size={14} /> Refresh
        </button>
      </div>
      <PathBar left={left} right={right} directory={false} onCompare={changePaths} />
      {find.open && (
        <div className="hx-findbar">
          <div className="hx-find-row">
            <Search size={14} className="hx-find-icon" />
            <input
              ref={findInput}
              className="hx-find-input"
              value={find.text}
              spellCheck={false}
              placeholder={find.encoding === "hex" ? "Find bytes, e.g. DE AD BE EF" : "Find text"}
              onChange={(e) => setFind((f) => ({ ...f, text: e.target.value, status: "" }))}
              onKeyDown={(e) => {
                if (e.key === "Enter") findNext(!e.shiftKey);
                else if (e.key === "Escape") setFind((f) => ({ ...f, open: false }));
                else return;
                e.preventDefault();
              }}
            />
            <select
              value={find.encoding}
              onChange={(e) => setFind((f) => ({ ...f, encoding: e.target.value as FindEncoding, status: "" }))}
              title="How the search text is turned into bytes"
            >
              {ENCODINGS.map(([v, label]) => (
                <option key={v} value={v}>
                  {label}
                </option>
              ))}
            </select>
            <button
              className={`icon-btn${find.matchCase ? " active" : ""}`}
              disabled={find.encoding !== "ascii" && find.encoding !== "utf8"}
              onClick={() => setFind((f) => ({ ...f, matchCase: !f.matchCase }))}
              title="Match case (ASCII letters)"
            >
              <CaseSensitive size={15} />
            </button>
            <div className="seg-control" title="Which side to search">
              {([0, 1] as HexSide[]).map((s) => (
                <button
                  key={s}
                  className={find.side === s ? "active" : ""}
                  onClick={() => setFind((f) => ({ ...f, side: s }))}
                >
                  {s === 0 ? "Left" : "Right"}
                </button>
              ))}
            </div>
            <button className="icon-btn" onClick={() => findNext(false)} title="Previous match (Shift+F3)">
              <ChevronUp size={15} />
            </button>
            <button className="icon-btn" onClick={() => findNext(true)} title="Next match (F3)">
              <ChevronDown size={15} />
            </button>
            <button
              className={`icon-btn${find.replace ? " active" : ""}`}
              onClick={() => setFind((f) => ({ ...f, replace: !f.replace }))}
              title="Toggle replace (Ctrl+H)"
            >
              <Replace size={15} />
            </button>
            <span className="hx-find-status">{find.status}</span>
            <span className="spacer" />
            <button className="icon-btn" onClick={() => setFind((f) => ({ ...f, open: false }))} title="Close (Esc)">
              <X size={15} />
            </button>
          </div>
          {find.replace && (
            <div className="hx-find-row">
              <Replace size={14} className="hx-find-icon" />
              <input
                className="hx-find-input"
                value={find.replacement}
                spellCheck={false}
                placeholder="Replace with (empty deletes)"
                onChange={(e) => setFind((f) => ({ ...f, replacement: e.target.value }))}
                onKeyDown={(e) => {
                  if (e.key === "Enter") replaceOne();
                  else if (e.key === "Escape") setFind((f) => ({ ...f, open: false }));
                  else return;
                  e.preventDefault();
                }}
              />
              <button
                onClick={replaceOne}
                disabled={readOnly[find.side]}
                title={readOnly[find.side] ? readOnlyHint(find.side) : "Replace the selected match and find the next"}
              >
                <Replace size={14} /> Replace
              </button>
              <button
                onClick={replaceAll}
                disabled={readOnly[find.side]}
                title={readOnly[find.side] ? readOnlyHint(find.side) : "Replace every match on this side (one undo step)"}
              >
                <ReplaceAll size={14} /> Replace all
              </button>
            </div>
          )}
        </div>
      )}
      <div className="session-body" ref={body}>
        {error && <div className="banner error">{error}</div>}
        {staleOnDisk && (
          <div className="banner info hx-banner">
            {staleOnDisk[0] && staleOnDisk[1] ? "Both files" : staleOnDisk[0] ? "The left file" : "The right file"}{" "}
            changed on disk.{" "}
            {([0, 1] as HexSide[]).some((s) => staleOnDisk[s] && state?.dirty[s])
              ? "Unsaved edits to it are based on the old contents and can't be saved."
              : "Reloading shows the new contents but discards your unsaved edits."}
            <button onClick={() => open(true)}>Reload (discard edits)</button>
            <button onClick={() => setStaleOnDisk(null)}>Dismiss</button>
          </div>
        )}
        {state?.note && <div className="banner info hx-banner">{state.note}</div>}
        {loading && !state && <div className="overlay">Comparing…</div>}
        <div className="hx-scroll" ref={scroller} onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}>
          <div className="hx-spacer" style={{ height: spacerPx, width: `calc(${rowWidthCh(bpr)}ch + 20px)` }}>
            <div
              className="hx-viewport"
              style={{ height: viewport.height }}
              onMouseDown={onMouseDown}
              onMouseOver={onMouseOver}
            >
              {items}
            </div>
          </div>
        </div>
        {state && rowCount === 0 && !loading && (
          <div className="hx-empty muted">
            {state.displayLen === 0 ? "Both files are empty" : "No differences to show"}
          </div>
        )}
      </div>
      <div className="statusbar">
        {state &&
          (state.identical && !state.stale ? (
            <span className="ok">Binary identical · {formatSize(state.sizes[0])}</span>
          ) : (
            <>
              <span className="s-different-fg">
                {state.stale ? "≈ " : ""}
                {state.differingBytes.toLocaleString()} differing bytes in {state.diffBlocks.toLocaleString()}{" "}
                {state.diffBlocks === 1 ? "block" : "blocks"}
              </span>
              <span>
                Left {formatSize(state.sizes[0])}
                {state.dirty[0] ? " · modified" : ""}
              </span>
              <span>
                Right {formatSize(state.sizes[1])}
                {state.dirty[1] ? " · modified" : ""}
              </span>
            </>
          ))}
        {state?.stale && <span className="muted">Recomparing…</span>}
        {state?.mode === "offset" && <span className="muted">Offset compare</span>}
        <span className="spacer" />
        {message && <span className="muted">{message}</span>}
        {cursor && state && (
          <span>
            {cursor.side === 0 ? "Left" : "Right"}{" "}
            {cursor.offset >= state.sizes[cursor.side]
              ? "end of file"
              : `0x${hex(cursor.offset, offWidth)} (${cursor.offset.toLocaleString()})`}
            {selLen ? ` · ${selLen.toLocaleString()} selected` : ""}
          </span>
        )}
        <span className="muted">{insertMode ? "INS" : "OVR"}</span>
      </div>
    </div>
  );
}
