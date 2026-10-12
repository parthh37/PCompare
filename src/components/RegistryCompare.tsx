import type { DialogFilter } from "@tauri-apps/plugin-dialog";
import {
  ArrowDown,
  ArrowLeftRight,
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
  Server,
  TriangleAlert,
  type LucideIcon,
} from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { errorMessage } from "../lib/api";
import {
  metaApi,
  type DiffStatus,
  type RegistryCompareResult,
  type RegistrySourceInfo,
  type RegistrySourceKind,
  type RegKeyDiff,
  type RegValueDiff,
} from "../lib/metaApi";
import type { CompareProps } from "../lib/sessionProps";
import { PathField } from "./PathField";
import { STATUS_ICON } from "./PropertyCompare";
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

interface FlatRow {
  key: RegKeyDiff;
  depth: number;
}

function SourceInfo({ info, base }: { info: RegistrySourceInfo; base: string }) {
  const Icon = SOURCE_ICON[info.kind];
  return (
    <div title={`${SOURCE_LABEL[info.kind]}: ${info.spec}`}>
      <Icon size={13} className="muted" />
      <span className="headline">{base || info.spec}</span>
      <span className="muted">
        · {info.keys.toLocaleString()} keys, {info.values.toLocaleString()} values
      </span>
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

export function RegistryCompare({ left, right, onPathsChange, settings, onSettingsChange }: CompareProps<RegistrySettings>) {
  const [filter, setFilter] = useState<Filter>(settings?.filter ?? "all");
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => reportSettings.current?.({ filter }), [filter]);

  const [result, setResult] = useState<RegistryCompareResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<string | null>(null);
  const [scrollTo, setScrollTo] = useState<string | null>(null);
  const root = useRef<HTMLDivElement>(null);
  const tree = useRef<VirtualListHandle>(null);

  const run = useCallback(async () => {
    if (!left || !right) return;
    setLoading(true);
    setError(null);
    try {
      const res = await metaApi.compareRegistry(left, right);
      setResult(res);
      // Open the top level so there is something to look at; keep prior
      // expansion on refresh where the keys still exist.
      setExpanded((prev) => new Set([...prev, ...res.roots.map((k) => k.id)]));
      setSelected((prev) => (prev && containsKey(res.roots, prev) ? prev : res.roots[0]?.id ?? null));
    } catch (e) {
      setError(errorMessage(e));
      setResult(null);
    } finally {
      setLoading(false);
    }
  }, [left, right]);

  useEffect(() => {
    run();
  }, [run]);

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

  const selectedKey = selected ? index.byId.get(selected) ?? null : null;
  const values = useMemo(
    () => (selectedKey ? selectedKey.values.filter((v) => matches(v.status, filter)) : []),
    [selectedKey, filter],
  );

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

  const position = selected ? index.pos.get(selected) ?? -1 : -1;
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

  useEffect(() => {
    const onKey = (e: globalThis.KeyboardEvent) => {
      // Hidden tab panels have no layout; only the visible session reacts.
      if (!root.current?.offsetParent) return;
      if (e.altKey && e.key === "ArrowDown") goDiff(1);
      else if (e.altKey && e.key === "ArrowUp") goDiff(-1);
      else if (e.key === "F5") run();
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [goDiff, run]);

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
        title={deletedNote}
      >
        <span
          className="chevron"
          onClick={(ev) => {
            ev.stopPropagation();
            if (k.children.length) toggle(k.id);
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
    return (
      <div className={`rv-row p-${v.status}`}>
        <div className={`rv-name${v.name ? "" : " muted"}`} title={name}>
          {name}
        </div>
        <div className={`rv-type${type.differs ? " differs" : ""}`} title={type.text}>
          {type.text}
        </div>
        <div className={`rv-val${v.left === null ? " absent" : ""}`} title={v.left ?? undefined}>
          {v.leftType === "(delete)" ? <span className="muted">(delete)</span> : v.left}
        </div>
        <div className={`rv-val${v.right === null ? " absent" : ""}`} title={v.right ?? undefined}>
          {v.rightType === "(delete)" ? <span className="muted">(delete)</span> : v.right}
        </div>
      </div>
    );
  };

  const st = result?.stats;
  const hiddenValues = selectedKey ? selectedKey.values.length - values.length : 0;
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
        <span className="spacer" />
        <button onClick={run} title="Refresh (F5)">
          <RefreshCw size={14} /> Refresh
        </button>
      </div>

      <RegistryPathBar left={left} right={right} onCompare={onPathsChange} />

      {result && (
        <div className="meta-info">
          <SourceInfo info={result.left} base={result.leftBase} />
          <SourceInfo info={result.right} base={result.rightBase} />
        </div>
      )}

      <div className="session-body">
        {loading && <div className="overlay">Reading registry…</div>}
        {error && <div className="banner error">{error}</div>}
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
                <div className="empty-state">
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
      </div>
    </div>
  );
}
