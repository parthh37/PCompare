import type { DialogFilter } from "@tauri-apps/plugin-dialog";
import {
  ArrowDown,
  ArrowLeft,
  ArrowLeftRight,
  ArrowRight,
  ArrowUp,
  ChevronDown,
  ChevronRight,
  ChevronsDownUp,
  ChevronsUpDown,
  FileText,
  Folder,
  FolderOpen,
  FolderX,
  Monitor,
  RefreshCw,
  Save,
  Server,
  TriangleAlert,
  Undo2,
  type LucideIcon,
} from "lucide-react";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
  type MouseEvent as ReactMouseEvent,
} from "react";
import { errorMessage, isRemote } from "../lib/api";
import {
  metaApi,
  type DiffStatus,
  type RegEdit,
  type RegistryCompareResult,
  type RegistrySourceInfo,
  type RegistrySourceKind,
  type RegKeyDiff,
  type RegSide,
  type RegValueDiff,
  type SideEdit,
} from "../lib/metaApi";
import type { CompareProps } from "../lib/sessionProps";
import { SEPARATOR, type MenuEntry, type MenuItems } from "../lib/nativeMenu";
import { useContextMenu } from "./ContextMenu";
import { PathField } from "./PathField";
import { STATUS_ICON } from "./PropertyCompare";
import { LiveConfirmDialog, RegKeyDialog, RegValueDialog, type ValueDraft } from "./RegistryEditDialogs";
import { VirtualList, type VirtualListHandle } from "./VirtualList";
import "./PropertyCompare.css";
import "./RegistryCompare.css";

type Filter = "all" | "diffs" | "same" | "orphans";

/** What a saved registry session remembers besides its paths. */
export interface RegistrySettings {
  filter: Filter;
}

const ROW_HEIGHT = 22;

const FILE_FILTERS: DialogFilter[] = [
  { name: "Registry exports", extensions: ["reg"] },
  { name: "All files", extensions: ["*"] },
];

const SOURCE_ICON: Record<RegistrySourceKind, LucideIcon> = {
  file: FileText,
  live: Monitor,
  remote: Server,
};

const SOURCE_LABEL: Record<RegistrySourceKind, string> = {
  file: ".reg export",
  live: "Live registry",
  remote: "Remote registry",
};

const SIDES: RegSide[] = ["left", "right"];
const other = (s: RegSide): RegSide => (s === "left" ? "right" : "left");

/** A key's own status: orphans as-is, otherwise whether its values differ. */
function ownStatus(k: RegKeyDiff): DiffStatus {
  if (k.status === "leftOnly" || k.status === "rightOnly") return k.status;
  return k.valuesDiffer ? "different" : "same";
}

function matches(s: DiffStatus, f: Filter): boolean {
  switch (f) {
    case "all":
      return true;
    case "diffs":
      return s !== "same";
    case "same":
      return s === "same";
    case "orphans":
      return s === "leftOnly" || s === "rightOnly";
  }
}

/** Post-order pass: a key stays visible if it or any descendant matches. */
function visibleSet(roots: RegKeyDiff[], f: Filter): Set<string> {
  const out = new Set<string>();
  const visit = (k: RegKeyDiff): boolean => {
    let childVisible = false;
    for (const c of k.children) childVisible = visit(c) || childVisible;
    const show = childVisible || matches(ownStatus(k), f);
    if (show) out.add(k.id);
    return show;
  };
  roots.forEach(visit);
  return out;
}

interface Index {
  parent: Map<string, string | null>;
  byId: Map<string, RegKeyDiff>;
  /** Depth-first order of every key, and each key's position in it. */
  order: RegKeyDiff[];
  pos: Map<string, number>;
  /** Keys to stop at for prev/next difference (not every key inside an orphan subtree). */
  stops: number[];
  withChildren: string[];
}

function buildIndex(roots: RegKeyDiff[]): Index {
  const idx: Index = { parent: new Map(), byId: new Map(), order: [], pos: new Map(), stops: [], withChildren: [] };
  const walk = (k: RegKeyDiff, parent: RegKeyDiff | null) => {
    idx.parent.set(k.id, parent?.id ?? null);
    idx.byId.set(k.id, k);
    const orphanParent = parent && parent.status === k.status && (k.status === "leftOnly" || k.status === "rightOnly");
    if (k.valuesDiffer && !orphanParent) idx.stops.push(idx.order.length);
    idx.pos.set(k.id, idx.order.length);
    idx.order.push(k);
    if (k.children.length) idx.withChildren.push(k.id);
    k.children.forEach((c) => walk(c, k));
  };
  roots.forEach((r) => walk(r, null));
  return idx;
}

function containsKey(keys: RegKeyDiff[], id: string): boolean {
  return keys.some((k) => k.id === id || (id.startsWith(k.id) && containsKey(k.children, id)));
}

/** Ids of `k` and every descendant that has children (what "expand all under" opens). */
function expandableUnder(k: RegKeyDiff): string[] {
  const out: string[] = [];
  const walk = (n: RegKeyDiff) => {
    if (!n.children.length) return;
    out.push(n.id);
    n.children.forEach(walk);
  };
  walk(k);
  return out;
}

const keyOn = (k: RegKeyDiff, side: RegSide) => (side === "left" ? k.status !== "rightOnly" : k.status !== "leftOnly");
const keyPath = (k: RegKeyDiff, side: RegSide) => (side === "left" ? k.leftPath : k.rightPath);
const valueOn = (v: RegValueDiff, side: RegSide) => (side === "left" ? v.leftType : v.rightType) !== null;
const valueLabel = (v: RegValueDiff) => (v.name ? `"${v.name}"` : "(Default)");

interface FlatRow {
  key: RegKeyDiff;
  depth: number;
}

type MenuTarget = { kind: "key"; key: RegKeyDiff } | { kind: "value"; key: RegKeyDiff; value: RegValueDiff };

type Dialog =
  | { kind: "value"; side: RegSide; key: RegKeyDiff; initial: ValueDraft; adding: boolean }
  | { kind: "key"; side: RegSide; parent: RegKeyDiff }
  | {
      kind: "live";
      side: RegSide;
      description: string;
      group: RegEdit[];
      commands: string[] | null;
      busy: boolean;
      error: string | null;
    };

function SourceInfo({ info, base, dirty }: { info: RegistrySourceInfo; base: string; dirty: boolean }) {
  const Icon = SOURCE_ICON[info.kind];
  return (
    <div title={`${SOURCE_LABEL[info.kind]}: ${info.spec}`}>
      <Icon size={13} className="muted" />
      <span className="headline">{base || info.spec}</span>
      <span className="muted">
        · {info.keys.toLocaleString()} keys, {info.values.toLocaleString()} values
      </span>
      {dirty && (
        <span className="reg-dirty" title="This .reg file has unsaved edits (Ctrl+S saves)">
          ● modified
        </span>
      )}
      {info.warnings.length > 0 && (
        <span className="reg-warn" title={info.warnings.join("\n")}>
          <TriangleAlert size={13} /> {info.warnings.length} skipped line{info.warnings.length === 1 ? "" : "s"}
        </span>
      )}
    </div>
  );
}

/** Like `PathBar`, but each side may also be a live key spec. */
function RegistryPathBar({
  left,
  right,
  onCompare,
}: {
  left: string;
  right: string;
  onCompare: (left: string, right: string) => void;
}) {
  const [l, setL] = useState(left);
  const [r, setR] = useState(right);
  useEffect(() => setL(left), [left]);
  useEffect(() => setR(right), [right]);

  const apply = (picked: string, isLeft: boolean) => {
    const other = isLeft ? r : l;
    (isLeft ? setL : setR)(picked);
    if (other) onCompare(isLeft ? picked : other, isLeft ? other : picked);
  };
  const submit = () => l.trim() && r.trim() && onCompare(l.trim(), r.trim());

  return (
    <div className="pathbar">
      <PathField
        value={l}
        onChange={setL}
        onSubmit={submit}
        onPick={(p) => apply(p, true)}
        directory={false}
        filters={FILE_FILTERS}
        placeholder="Left .reg file or live key e.g. HKLM\Software\Vendor"
      />
      <button className="icon-btn swap" title="Swap sides" onClick={() => l && r && onCompare(r, l)}>
        <ArrowLeftRight size={15} />
      </button>
      <PathField
        value={r}
        onChange={setR}
        onSubmit={submit}
        onPick={(p) => apply(p, false)}
        directory={false}
        filters={FILE_FILTERS}
        placeholder="Right .reg file or live key e.g. \\PC01\HKLM\Software\Vendor"
      />
    </div>
  );
}

function typeCell(v: RegValueDiff): { text: string; differs: boolean } {
  if (v.leftType && v.rightType && v.leftType !== v.rightType) {
    return { text: `${v.leftType} / ${v.rightType}`, differs: true };
  }
  return { text: v.leftType ?? v.rightType ?? "", differs: false };
}

export function RegistryCompare({
  left,
  right,
  onPathsChange,
  settings,
  onSettingsChange,
  onDirtyChange,
}: CompareProps<RegistrySettings>) {
  const [filter, setFilter] = useState<Filter>(settings?.filter ?? "all");
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => void reportSettings.current?.({ filter }), [filter]);

  const [result, setResult] = useState<RegistryCompareResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<string | null>(null);
  const [selectedValue, setSelectedValue] = useState<string | null>(null);
  const [scrollTo, setScrollTo] = useState<string | null>(null);
  const root = useRef<HTMLDivElement>(null);
  const tree = useRef<VirtualListHandle>(null);

  // ---------- pending .reg edits ----------

  /** Unsaved edits to .reg sides, grouped per user action (undo pops a group). */
  const [history, setHistory] = useState<SideEdit[][]>([]);
  const historyRef = useRef(history);
  historyRef.current = history;
  const edits = useMemo(() => history.flat(), [history]);
  const dirtySides = useMemo(() => new Set(edits.map((e) => e.side)), [edits]);
  const dirty = history.length > 0;
  const [notice, setNotice] = useState<string | null>(null);
  const [working, setWorking] = useState<string | null>(null);
  const [dialog, setDialog] = useState<Dialog | null>(null);

  const reportDirty = useRef(onDirtyChange);
  reportDirty.current = onDirtyChange;
  // Save all (closing the window or quitting) calls the latest save.
  const saveForClose = useRef<() => unknown>(() => {});
  useEffect(() => void reportDirty.current?.(dirty, () => saveForClose.current()), [dirty]);

  // Edits belong to the files they were made against.
  useEffect(() => {
    historyRef.current = [];
    setHistory([]);
    setNotice(null);
  }, [left, right]);

  const fetchResult = useCallback(
    async (pending: SideEdit[], reload: boolean) => {
      const res = await metaApi.compareRegistry(left, right, pending, reload);
      setResult(res);
      // Open the top level so there is something to look at; keep prior
      // expansion on refresh where the keys still exist.
      setExpanded((prev) => new Set([...prev, ...res.roots.map((k) => k.id)]));
      setSelected((prev) => (prev && containsKey(res.roots, prev) ? prev : (res.roots[0]?.id ?? null)));
      return res;
    },
    [left, right],
  );

  const run = useCallback(async () => {
    if (!left || !right) return;
    setLoading(true);
    setError(null);
    try {
      // Pending edits are replayed on the freshly read files.
      await fetchResult(historyRef.current.flat(), true);
    } catch (e) {
      setError(errorMessage(e));
      setResult(null);
    } finally {
      setLoading(false);
    }
  }, [left, right, fetchResult]);

  useEffect(() => {
    run();
  }, [run]);

  const changePaths = (l: string, r: string) => {
    if (dirty && !window.confirm("The registry exports have unsaved edits. Discard them?")) return;
    onPathsChange(l, r);
  };

  const roots = useMemo(() => result?.roots ?? [], [result]);
  const index = useMemo(() => buildIndex(roots), [roots]);
  const visible = useMemo(() => visibleSet(roots, filter), [roots, filter]);

  const flat = useMemo(() => {
    const out: FlatRow[] = [];
    const walk = (keys: RegKeyDiff[], depth: number) => {
      for (const k of keys) {
        if (!visible.has(k.id)) continue;
        out.push({ key: k, depth });
        if (expanded.has(k.id)) walk(k.children, depth + 1);
      }
    };
    walk(roots, 0);
    return out;
  }, [roots, visible, expanded]);

  const selectedKey = selected ? (index.byId.get(selected) ?? null) : null;
  const values = useMemo(
    () => (selectedKey ? selectedKey.values.filter((v) => matches(v.status, filter)) : []),
    [selectedKey, filter],
  );
  const selectedVal =
    selectedKey && selectedValue !== null
      ? (selectedKey.values.find((v) => v.name.toLowerCase() === selectedValue) ?? null)
      : null;
  useEffect(() => setSelectedValue(null), [selected]);

  // Scroll a key into view once the flattened list contains it.
  useLayoutEffect(() => {
    if (!scrollTo) return;
    const row = flat.findIndex((r) => r.key.id === scrollTo);
    if (row >= 0) tree.current?.scrollToRow(row);
    setScrollTo(null);
  }, [flat, scrollTo]);

  const toggle = (id: string) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      next.has(id) ? next.delete(id) : next.add(id);
      return next;
    });
  const expandUnder = (k: RegKeyDiff) => setExpanded((prev) => new Set([...prev, ...expandableUnder(k)]));
  const collapseUnder = (k: RegKeyDiff) =>
    setExpanded((prev) => {
      const next = new Set(prev);
      expandableUnder(k).forEach((id) => next.delete(id));
      return next;
    });

  const reveal = useCallback(
    (id: string) => {
      setExpanded((prev) => {
        const next = new Set(prev);
        for (let p = index.parent.get(id); p; p = index.parent.get(p)) next.add(p);
        return next;
      });
      // A filter that hides the target would make it unreachable.
      if (!visible.has(id)) setFilter("all");
      setSelected(id);
      setScrollTo(id);
    },
    [index, visible],
  );

  const position = selected ? (index.pos.get(selected) ?? -1) : -1;
  const nextDiff = index.stops.find((i) => i > position);
  let prevDiff: number | undefined;
  for (const i of index.stops) {
    if (i >= position) break;
    prevDiff = i;
  }
  const goDiff = useCallback(
    (dir: 1 | -1) => {
      const target = dir > 0 ? nextDiff : prevDiff;
      if (target !== undefined) reveal(index.order[target].id);
    },
    [nextDiff, prevDiff, reveal, index],
  );

  // ---------- editing ----------

  const sideSpec = (side: RegSide) => (side === "left" ? left : right);
  const isLive = (side: RegSide) => !!result && result[side].kind !== "file";
  /** Why a side can't be edited, or null when it can. */
  const readOnlyReason = (side: RegSide): string | null => {
    if (!result) return "Nothing loaded";
    if (result[side].kind === "file" && isRemote(sideSpec(side))) return "Remote files are read-only";
    return null;
  };
  const canEdit = (side: RegSide) => readOnlyReason(side) === null;

  /** Queues a group of edits to a .reg side, or asks to apply it to a live side. */
  const commit = async (side: RegSide, group: RegEdit[], description: string) => {
    setDialog(null);
    if (!canEdit(side)) return;
    if (isLive(side)) {
      setDialog({ kind: "live", side, description, group, commands: null, busy: false, error: null });
      try {
        const commands: string[] = [];
        for (const edit of group) commands.push(...(await metaApi.registryLivePlan(left, right, edits, side, edit)));
        setDialog((d) => (d?.kind === "live" ? { ...d, commands } : d));
      } catch (e) {
        setDialog((d) => (d?.kind === "live" ? { ...d, commands: [], error: errorMessage(e) } : d));
      }
      return;
    }
    const next = [...historyRef.current, group.map((edit) => ({ side, edit }))];
    setWorking("Applying…");
    try {
      await fetchResult(next.flat(), false);
      historyRef.current = next;
      setHistory(next);
      setNotice(null);
    } catch (e) {
      // The edit is dropped; the previous result stays on screen.
      setNotice(`${description} failed: ${errorMessage(e)}`);
    } finally {
      setWorking(null);
    }
  };

  const applyLive = async () => {
    if (dialog?.kind !== "live") return;
    const { side, group } = dialog;
    setDialog({ ...dialog, busy: true, error: null });
    try {
      for (const edit of group) await metaApi.registryApplyLive(left, right, edits, side, edit);
      setDialog(null);
    } catch (e) {
      setDialog((d) => (d?.kind === "live" ? { ...d, busy: false, error: errorMessage(e) } : d));
    }
    // Re-read the live key either way: a failure may have applied some steps.
    setWorking("Reading registry…");
    try {
      await fetchResult(historyRef.current.flat(), true);
    } catch (e) {
      setNotice(errorMessage(e));
    } finally {
      setWorking(null);
    }
  };

  const undo = async () => {
    const prev = historyRef.current;
    if (!prev.length) return;
    const next = prev.slice(0, -1);
    setWorking("Undoing…");
    try {
      await fetchResult(next.flat(), false);
      historyRef.current = next;
      setHistory(next);
      setNotice(null);
    } catch (e) {
      setNotice(`Undo failed: ${errorMessage(e)}`);
    } finally {
      setWorking(null);
    }
  };

  const save = async () => {
    const sides = SIDES.filter((s) => dirtySides.has(s));
    if (!sides.length) return;
    setWorking("Saving…");
    try {
      await metaApi.registrySave(left, right, edits, sides);
      historyRef.current = [];
      setHistory([]);
      setNotice(null);
      await fetchResult([], false);
    } catch (e) {
      setNotice(`Save failed: ${errorMessage(e)}`);
    } finally {
      setWorking(null);
    }
  };

  saveForClose.current = () => save();

  const copyKey = (k: RegKeyDiff, to: RegSide) =>
    commit(
      to,
      [{ op: "copyKey", from: keyPath(k, other(to)), to: keyPath(k, to) }],
      keyOn(k, other(to)) ? `Copy key ${k.name} to the ${to}` : `Delete key ${k.name} on the ${to}`,
    );
  const deleteKey = (k: RegKeyDiff, side: RegSide) =>
    commit(side, [{ op: "deleteKey", key: keyPath(k, side) }], `Delete key ${k.name} and everything under it`);
  const copyValue = (k: RegKeyDiff, v: RegValueDiff, to: RegSide) =>
    commit(
      to,
      [{ op: "copyValue", fromKey: keyPath(k, other(to)), toKey: keyPath(k, to), name: v.name }],
      valueOn(v, other(to)) ? `Copy value ${valueLabel(v)} to the ${to}` : `Delete value ${valueLabel(v)} on the ${to}`,
    );
  const deleteValue = (k: RegKeyDiff, v: RegValueDiff, side: RegSide) =>
    commit(side, [{ op: "deleteValue", key: keyPath(k, side), name: v.name }], `Delete value ${valueLabel(v)}`);

  const editValue = (k: RegKeyDiff, v: RegValueDiff | null, side: RegSide) => {
    if (!canEdit(side)) return;
    const type = v ? (side === "left" ? v.leftType : v.rightType) : null;
    const text = v ? (side === "left" ? v.leftText : v.rightText) : null;
    // Values marked "(delete)" in a .reg file have no data to edit.
    const present = v && type && type !== "(delete)";
    setDialog({
      kind: "value",
      side,
      key: k,
      adding: !present,
      initial: { name: v?.name ?? "", type: present ? type : "REG_SZ", text: present ? (text ?? "") : "" },
    });
  };

  const submitValue = (draft: ValueDraft) => {
    if (dialog?.kind !== "value") return;
    const { side, key, initial, adding } = dialog;
    const path = keyPath(key, side);
    const group: RegEdit[] = [];
    if (!adding && draft.name.toLowerCase() !== initial.name.toLowerCase()) {
      group.push({ op: "deleteValue", key: path, name: initial.name });
    }
    group.push({ op: "setValue", key: path, name: draft.name, valueType: draft.type, data: draft.text });
    const label = draft.name ? `"${draft.name}"` : "(Default)";
    commit(side, group, `${adding ? "Add" : "Set"} value ${label} in ${key.name}`);
  };

  /** Copies the selected value, or the selected key when no value is selected. */
  const copySelection = (to: RegSide) => {
    if (!selectedKey) return;
    if (selectedVal) copyValue(selectedKey, selectedVal, to);
    else copyKey(selectedKey, to);
  };

  useEffect(() => {
    const onKey = (e: globalThis.KeyboardEvent) => {
      // Hidden tab panels have no layout; only the visible session reacts.
      if (!root.current?.offsetParent || dialog) return;
      const mod = e.ctrlKey || e.metaKey;
      const typing = e.target instanceof HTMLElement && e.target.closest("input, textarea, select");
      if (e.altKey && e.key === "ArrowDown") goDiff(1);
      else if (e.altKey && e.key === "ArrowUp") goDiff(-1);
      else if (e.altKey && e.key === "ArrowRight") copySelection("right");
      else if (e.altKey && e.key === "ArrowLeft") copySelection("left");
      else if (mod && e.key.toLowerCase() === "s") save();
      else if (mod && !e.shiftKey && e.key.toLowerCase() === "z" && !typing) undo();
      else if (e.key === "F5") run();
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const menu = useContextMenu((target: MenuTarget): MenuItems => {
    const k = target.key;
    const sideItems = (make: (side: RegSide) => MenuEntry | null) => SIDES.map(make);
    // Native menus have no tooltips, so say up front why a side's edits are greyed out.
    const readOnly: MenuItems = SIDES.map((side) => {
      const reason = readOnlyReason(side);
      return reason && { heading: `${side === "left" ? "Left" : "Right"}: ${reason}` };
    });
    if (target.kind === "key") {
      const either = keyOn(k, "left") || keyOn(k, "right");
      return [
        ...readOnly,
        SEPARATOR,
        { label: "Expand all under this key", disabled: !k.children.length, onClick: () => expandUnder(k) },
        { label: "Collapse all under this key", disabled: !k.children.length, onClick: () => collapseUnder(k) },
        SEPARATOR,
        {
          label: "Copy key to right  →",
          shortcut: "Alt+→",
          disabled: !canEdit("right") || !either,
          onClick: () => copyKey(k, "right"),
        },
        {
          label: "←  Copy key to left",
          shortcut: "Alt+←",
          disabled: !canEdit("left") || !either,
          onClick: () => copyKey(k, "left"),
        },
        SEPARATOR,
        ...sideItems((side) =>
          keyOn(k, side)
            ? { label: `Add ${side} value…`, disabled: !canEdit(side), onClick: () => editValue(k, null, side) }
            : null,
        ),
        ...sideItems((side) =>
          keyOn(k, side)
            ? {
                label: `Add ${side} subkey…`,
                disabled: !canEdit(side),
                onClick: () => setDialog({ kind: "key", side, parent: k }),
              }
            : null,
        ),
        ...sideItems((side) =>
          keyOn(k, side)
            ? { label: `Delete ${side} key`, disabled: !canEdit(side), onClick: () => deleteKey(k, side) }
            : null,
        ),
      ];
    }
    const v = target.value;
    return [
      ...readOnly,
      SEPARATOR,
      ...sideItems((side) => ({
        label: `${valueOn(v, side) ? "Edit" : "Add"} ${side} value…`,
        disabled: !canEdit(side) || !keyOn(k, side),
        onClick: () => editValue(k, v, side),
      })),
      SEPARATOR,
      {
        label: "Copy value to right  →",
        shortcut: "Alt+→",
        disabled: !canEdit("right"),
        onClick: () => copyValue(k, v, "right"),
      },
      { label: "←  Copy value to left", shortcut: "Alt+←", disabled: !canEdit("left"), onClick: () => copyValue(k, v, "left") },
      SEPARATOR,
      ...sideItems((side) =>
        valueOn(v, side)
          ? { label: `Delete ${side} value`, disabled: !canEdit(side), onClick: () => deleteValue(k, v, side) }
          : null,
      ),
    ];
  });

  const openKeyMenu = (e: ReactMouseEvent, k: RegKeyDiff) => {
    e.preventDefault();
    setSelected(k.id);
    menu.open(e, { kind: "key", key: k });
  };
  const openValueMenu = (e: ReactMouseEvent, k: RegKeyDiff, v: RegValueDiff) => {
    e.preventDefault();
    setSelectedValue(v.name.toLowerCase());
    menu.open(e, { kind: "value", key: k, value: v });
  };

  // Arrow-key navigation inside the key tree.
  const onTreeKey = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.altKey || !flat.length) return;
    const i = flat.findIndex((r) => r.key.id === selected);
    const cur = i >= 0 ? flat[i].key : null;
    let target: string | null = null;
    switch (e.key) {
      case "ArrowDown":
        target = flat[Math.min(flat.length - 1, i + 1)].key.id;
        break;
      case "ArrowUp":
        target = flat[Math.max(0, i - 1)].key.id;
        break;
      case "ArrowRight":
        if (cur && cur.children.length && !expanded.has(cur.id)) toggle(cur.id);
        else if (cur && expanded.has(cur.id) && i + 1 < flat.length && flat[i + 1].depth > flat[i].depth)
          target = flat[i + 1].key.id;
        break;
      case "ArrowLeft":
        if (cur && expanded.has(cur.id)) toggle(cur.id);
        else if (cur) target = index.parent.get(cur.id) ?? null;
        break;
      case "*":
        // As in regedit: expand everything under the selected key.
        if (cur) expandUnder(cur);
        break;
      case "Home":
        target = flat[0].key.id;
        break;
      case "End":
        target = flat[flat.length - 1].key.id;
        break;
      default:
        return;
    }
    e.preventDefault();
    if (target) {
      setSelected(target);
      setScrollTo(target);
    }
  };

  const renderKey = (i: number) => {
    const { key: k, depth } = flat[i];
    const own = ownStatus(k);
    const StatusIcon = STATUS_ICON[k.status];
    const open = expanded.has(k.id);
    const deleted = k.leftDeleted || k.rightDeleted;
    const KeyIcon = deleted ? FolderX : open && k.children.length ? FolderOpen : Folder;
    const deletedNote = deleted
      ? `Marked for deletion ([-KEY]) on the ${k.leftDeleted && k.rightDeleted ? "both sides" : k.leftDeleted ? "left" : "right"}`
      : undefined;
    return (
      <div
        className={`rk-row s-${k.status}${own !== "same" ? " own-diff" : ""}${selected === k.id ? " selected" : ""}`}
        style={{ paddingLeft: 4 + depth * 16 }}
        onClick={() => setSelected(k.id)}
        onDoubleClick={() => k.children.length && toggle(k.id)}
        onContextMenu={(e) => openKeyMenu(e, k)}
        title={deletedNote}
      >
        <span
          className="chevron"
          title={k.children.length ? "Shift+click expands or collapses everything under this key" : undefined}
          onClick={(ev) => {
            ev.stopPropagation();
            if (!k.children.length) return;
            if (ev.shiftKey) (open ? collapseUnder : expandUnder)(k);
            else toggle(k.id);
          }}
        >
          {k.children.length > 0 && (open ? <ChevronDown size={13} /> : <ChevronRight size={13} />)}
        </span>
        <KeyIcon size={14} className="rk-icon" />
        <span className="rk-name">{k.name}</span>
        <StatusIcon size={12} className="rk-status" />
      </div>
    );
  };

  const renderValue = (i: number) => {
    const v = values[i];
    const type = typeCell(v);
    const name = v.name || "(Default)";
    const isSel = selectedValue === v.name.toLowerCase();
    const cell = (side: RegSide) => {
      const text = side === "left" ? v.left : v.right;
      const t = side === "left" ? v.leftType : v.rightType;
      return (
        <div
          className={`rv-val${text === null ? " absent" : ""}`}
          title={canEdit(side) ? `${text ?? ""}\n(double-click to edit the ${side} value)`.trim() : (text ?? undefined)}
          onDoubleClick={() => selectedKey && editValue(selectedKey, v, side)}
        >
          {t === "(delete)" ? <span className="muted">(delete)</span> : text}
        </div>
      );
    };
    return (
      <div
        className={`rv-row p-${v.status}${isSel ? " selected" : ""}`}
        onClick={() => setSelectedValue(isSel ? null : v.name.toLowerCase())}
        onContextMenu={(e) => selectedKey && openValueMenu(e, selectedKey, v)}
      >
        <div className={`rv-name${v.name ? "" : " muted"}`} title={name}>
          {name}
        </div>
        <div className={`rv-type${type.differs ? " differs" : ""}`} title={type.text}>
          {type.text}
        </div>
        {cell("left")}
        {cell("right")}
      </div>
    );
  };

  const renderDialog = () => {
    if (!dialog) return null;
    const close = () => setDialog(null);
    switch (dialog.kind) {
      case "value": {
        const names = new Set(
          dialog.key.values.filter((v) => valueOn(v, dialog.side)).map((v) => v.name.toLowerCase()),
        );
        return (
          <RegValueDialog
            side={dialog.side}
            keyPath={keyPath(dialog.key, dialog.side)}
            initial={dialog.initial}
            adding={dialog.adding}
            existing={names}
            onSubmit={submitValue}
            onClose={close}
          />
        );
      }
      case "key": {
        const { side, parent } = dialog;
        const names = new Set(parent.children.filter((c) => keyOn(c, side)).map((c) => c.name.toLowerCase()));
        return (
          <RegKeyDialog
            side={side}
            parentPath={keyPath(parent, side)}
            existing={names}
            onSubmit={(name) =>
              commit(side, [{ op: "addKey", key: `${keyPath(parent, side)}\\${name}` }], `Add key ${name}`).then(() =>
                setExpanded((prev) => new Set(prev).add(parent.id)),
              )
            }
            onClose={close}
          />
        );
      }
      case "live":
        return (
          <LiveConfirmDialog
            side={dialog.side}
            description={dialog.description}
            commands={dialog.commands}
            busy={dialog.busy}
            error={dialog.error}
            onConfirm={applyLive}
            onClose={close}
          />
        );
    }
  };

  const st = result?.stats;
  const hiddenValues = selectedKey ? selectedKey.values.length - values.length : 0;
  const copyTarget = selectedVal ? "value" : "key";
  return (
    <div className="session" ref={root}>
      <div className="toolbar">
        <div className="seg-control">
          {(
            [
              ["all", "All"],
              ["diffs", "Differences"],
              ["same", "Same"],
              ["orphans", "Orphans"],
            ] as [Filter, string][]
          ).map(([f, label]) => (
            <button key={f} className={filter === f ? "active" : ""} onClick={() => setFilter(f)}>
              {label}
            </button>
          ))}
        </div>
        <span className="sep" />
        <button onClick={() => setExpanded(new Set(index.withChildren))} title="Expand all" disabled={!result}>
          <ChevronsUpDown size={14} /> Expand
        </button>
        <button onClick={() => setExpanded(new Set())} title="Collapse all" disabled={!result}>
          <ChevronsDownUp size={14} /> Collapse
        </button>
        <span className="sep" />
        <button onClick={() => goDiff(-1)} disabled={prevDiff === undefined} title="Previous difference (Alt+Up)">
          <ArrowUp size={14} /> Prev
        </button>
        <button onClick={() => goDiff(1)} disabled={nextDiff === undefined} title="Next difference (Alt+Down)">
          <ArrowDown size={14} /> Next
        </button>
        <span className="sep" />
        <button
          onClick={() => copySelection("left")}
          disabled={!selectedKey || !canEdit("left")}
          title={readOnlyReason("left") ?? `Copy the selected ${copyTarget} to the left (Alt+Left)`}
        >
          <ArrowLeft size={14} /> Copy to left
        </button>
        <button
          onClick={() => copySelection("right")}
          disabled={!selectedKey || !canEdit("right")}
          title={readOnlyReason("right") ?? `Copy the selected ${copyTarget} to the right (Alt+Right)`}
        >
          Copy to right <ArrowRight size={14} />
        </button>
        <button onClick={undo} disabled={!dirty || !!working} title="Undo the last unsaved .reg edit (Ctrl+Z)">
          <Undo2 size={14} /> Undo
        </button>
        <button
          className={dirty ? "reg-save-dirty" : ""}
          onClick={save}
          disabled={!dirty || !!working}
          title="Write the edited .reg files, keeping their format (Ctrl+S)"
        >
          <Save size={14} /> Save
        </button>
        <span className="spacer" />
        <button onClick={run} title="Refresh (F5)">
          <RefreshCw size={14} /> Refresh
        </button>
      </div>

      <RegistryPathBar left={left} right={right} onCompare={changePaths} />

      {result && (
        <div className="meta-info">
          <SourceInfo info={result.left} base={result.leftBase} dirty={dirtySides.has("left")} />
          <SourceInfo info={result.right} base={result.rightBase} dirty={dirtySides.has("right")} />
        </div>
      )}

      <div className="session-body">
        {(loading || working) && <div className="overlay">{working ?? "Reading registry…"}</div>}
        {error && <div className="banner error">{error}</div>}
        {notice && (
          <div className="banner error reg-notice" onClick={() => setNotice(null)} title="Click to dismiss">
            {notice}
          </div>
        )}
        {!result && !error && !loading && (
          <div className="empty-state">
            Compare two .reg exports, or live keys such as <code>HKCU\Software\Vendor</code>.
            <br />
            <span className="muted">
              Live and remote (<code>\\MACHINE\HKLM\…</code>) keys are read on Windows only; remote access needs the
              Remote Registry service.
            </span>
          </div>
        )}
        {result && (
          <div className="reg-split">
            <div className="reg-tree" tabIndex={0} onKeyDown={onTreeKey}>
              <div className="reg-head">Key</div>
              {flat.length === 0 ? (
                <div className="empty-state">Nothing to show for this filter.</div>
              ) : (
                <VirtualList ref={tree} count={flat.length} rowHeight={ROW_HEIGHT} renderRow={renderKey} />
              )}
            </div>
            <div className="reg-values">
              <div className="rv-row reg-head">
                <div>Name</div>
                <div>Type</div>
                <div>Left value</div>
                <div>Right value</div>
              </div>
              {!selectedKey ? (
                <div className="empty-state">Select a key to see its values.</div>
              ) : values.length === 0 ? (
                <div
                  className="empty-state"
                  onContextMenu={(e) => selectedKey && openKeyMenu(e, selectedKey)}
                  title="Right-click to add a value"
                >
                  {selectedKey.values.length === 0 ? "This key has no values." : "No values match this filter."}
                </div>
              ) : (
                <VirtualList count={values.length} rowHeight={ROW_HEIGHT} renderRow={renderValue} />
              )}
              {hiddenValues > 0 && values.length > 0 && (
                <div className="rv-hidden muted">
                  {hiddenValues} value{hiddenValues === 1 ? "" : "s"} hidden by the filter
                </div>
              )}
            </div>
          </div>
        )}
      </div>

      {menu.element}
      {renderDialog()}

      <div className="statusbar">
        {st && (
          <>
            <span className="muted">Keys:</span>
            <span className="s-same-fg">{st.keys.same} same</span>
            <span className="s-different-fg">{st.keys.different} different</span>
            <span className="s-leftOnly-fg">{st.keys.leftOnly} left only</span>
            <span className="s-rightOnly-fg">{st.keys.rightOnly} right only</span>
            <span className="sep-v" />
            <span className="muted">Values:</span>
            <span className="s-same-fg">{st.values.same} same</span>
            <span className="s-different-fg">{st.values.different} different</span>
            <span className="s-leftOnly-fg">{st.values.leftOnly} left only</span>
            <span className="s-rightOnly-fg">{st.values.rightOnly} right only</span>
          </>
        )}
        <span className="spacer" />
        {dirty && (
          <span className="muted">
            {edits.length} unsaved edit{edits.length === 1 ? "" : "s"}
          </span>
        )}
      </div>
    </div>
  );
}
