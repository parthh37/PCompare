import { getCurrentWebview } from "@tauri-apps/api/webview";
import { Globe, Plus, Save, Settings, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { AppSettingsDialog } from "./components/AppSettingsDialog";
import { FolderCompare } from "./components/FolderCompare";
import { FolderMerge } from "./components/FolderMerge";
import { FolderSync } from "./components/FolderSync";
import { HexCompare } from "./components/HexCompare";
import { Home } from "./components/Home";
import { Mp3Compare } from "./components/Mp3Compare";
import { PictureCompare } from "./components/PictureCompare";
import { Profiles } from "./components/Profiles";
import { RegistryCompare } from "./components/RegistryCompare";
import { SaveSessionDialog } from "./components/SaveSessionDialog";
import { SessionIcon } from "./components/SessionIcon";
import { ToolIcon } from "./components/icons";
import { TableCompare } from "./components/TableCompare";
import { TextCompare } from "./components/TextCompare";
import { TextMerge } from "./components/TextMerge";
import { VersionCompare } from "./components/VersionCompare";
import { api, errorMessage, type SavedSession } from "./lib/api";
import { basename } from "./lib/format";
import type { MergePaths } from "./lib/sessionProps";
import { SESSIONS, isCompareKind, sessionForFiles, type CompareKind, type SessionKind } from "./lib/sessions";
import { ShellContext } from "./lib/shell";
import { useUpdater } from "./lib/useUpdater";

export type { SessionKind } from "./lib/sessions";

export interface Tab {
  id: number;
  kind: SessionKind;
  left: string;
  right: string;
  /** 3-way sessions: common ancestor and merge destination. */
  base?: string;
  output?: string;
  /** View settings reported by the session, persisted when saved. */
  settings?: Record<string, unknown>;
  /** Set once the tab is backed by a saved session. */
  sessionId?: string;
  sessionName?: string;
}

function tabTitle(t: Tab): string {
  if (t.sessionName) return t.sessionName;
  if (!isCompareKind(t.kind) || !(t.left || t.right)) return SESSIONS[t.kind].label;
  if (!t.left || !t.right) return basename(t.left || t.right);
  const l = basename(t.left);
  const r = basename(t.right);
  return l === r ? l : `${l} ↔ ${r}`;
}

const canSaveTab = (t: Tab | undefined): t is Tab & { kind: CompareKind } =>
  !!t && isCompareKind(t.kind) && !!t.left && !!t.right;

export default function App() {
  const nextId = useRef(1);
  const [tabs, setTabs] = useState<Tab[]>([{ id: 0, kind: "home", left: "", right: "" }]);
  const [active, setActive] = useState(0);
  const [saved, setSaved] = useState<SavedSession[]>([]);
  const [saving, setSaving] = useState<Tab | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  /** Tabs whose session reported unsaved edits. */
  const dirtyTabs = useRef(new Set<number>());
  const updater = useUpdater(setToast);
  const [settingsOpen, setSettingsOpen] = useState(false);

  const refreshSaved = useCallback(
    () =>
      api
        .listSessions()
        .then(setSaved)
        .catch((e) => setToast(errorMessage(e))),
    [],
  );
  useEffect(() => {
    refreshSaved();
  }, [refreshSaved]);

  const openTab = useCallback((kind: SessionKind, left = "", right = "", extra: Partial<Tab> = {}) => {
    const id = nextId.current++;
    setTabs((t) => [...t, { id, kind, left, right, ...extra }]);
    setActive(id);
  }, []);

  /** Focuses an existing tab of this kind (or saved session) instead of duplicating it. */
  const focusOr = (match: (t: Tab) => boolean, open: () => void) => {
    const existing = tabs.find(match);
    if (existing) setActive(existing.id);
    else open();
  };

  const openProfiles = () => focusOr((t) => t.kind === "profiles", () => openTab("profiles"));

  const openSaved = (s: SavedSession) =>
    focusOr(
      (t) => t.sessionId === s.id,
      () =>
        openTab(s.kind, s.left, s.right, {
          base: s.base,
          output: s.output,
          settings: s.settings ?? {},
          sessionId: s.id,
          sessionName: s.name,
        }),
    );

  /** Picks the session type from what the two paths are; `right` may be empty to open one side. */
  const openAuto = useCallback(
    async (left: string, right: string) => {
      // A missing side takes the other's kind, so one folder still opens a folder compare.
      const [lk, rk] = await Promise.all([api.pathKind(left), api.pathKind(right || left)]);
      openTab(lk === "directory" && rk === "directory" ? "folder" : sessionForFiles(left, right || left), left, right);
    },
    [openTab],
  );

  // "Select Left Side for Compare" lives in a file the Explorer menu also
  // writes, so re-read it whenever the window regains focus.
  const [leftSelection, setLeftSelection] = useState<string | null>(null);
  useEffect(() => {
    const load = () => api.getLeftSelection().then(setLeftSelection).catch(() => {});
    load();
    window.addEventListener("focus", load);
    return () => window.removeEventListener("focus", load);
  }, []);
  const selectLeft = useCallback((path: string | null) => {
    setLeftSelection(path);
    api.setLeftSelection(path).catch((e) => setToast(errorMessage(e)));
  }, []);
  const compareToLeft = useCallback(
    (path: string) => {
      if (!leftSelection) return;
      openAuto(leftSelection, path).catch((e) => setToast(errorMessage(e)));
      selectLeft(null);
    },
    [leftSelection, openAuto, selectLeft],
  );

  const updateTab = (id: number, patch: Partial<Tab>) =>
    setTabs((ts) => ts.map((t) => (t.id === id ? { ...t, ...patch } : t)));

  const closeTab = (id: number) => {
    if (dirtyTabs.current.has(id) && !window.confirm("This tab has unsaved changes. Close it and discard them?")) return;
    dirtyTabs.current.delete(id);
    setTabs((ts) => {
      const idx = ts.findIndex((t) => t.id === id);
      const rest = ts.filter((t) => t.id !== id);
      const remaining = rest.length ? rest : [{ id: nextId.current++, kind: "home" as const, left: "", right: "" }];
      if (id === active) setActive(remaining[Math.max(0, idx - 1)].id);
      return remaining;
    });
  };

  // `pcompare.exe <left> <right>` from the shell or a git difftool, and
  // `pcompare.exe <left> <right> <base> [<output>]` (Beyond Compare's order) as a git mergetool.
  useEffect(() => {
    api
      .launchArgs()
      .then(async ([left, right, base, output]) => {
        if (!left) return;
        if (!base) return openAuto(left, right ?? "");
        const kind = (await api.pathKind(base)) === "directory" ? "folderMerge" : "merge";
        openTab(kind, left, right, { base, output: output ?? "" });
      })
      .catch(() => {});
  }, [openAuto, openTab]);

  // Dropping two files/folders onto the window starts a comparison.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    getCurrentWebview()
      .onDragDropEvent((e) => {
        if (e.payload.type !== "drop") return;
        const [first, second] = e.payload.paths;
        if (second) return openAuto(first, second);
        if (first) dropOne.current(first);
      })
      .then((u) => (unlisten = u))
      .catch(() => {});
    return () => unlisten?.();
  }, [openAuto]);

  const activeTab = tabs.find((t) => t.id === active);

  /** One dropped path fills the empty side of the active compare, or opens on its own. */
  const dropOne = useRef((_path: string) => {});
  dropOne.current = (path: string) => {
    const t = activeTab;
    if (t && isCompareKind(t.kind) && !SESSIONS[t.kind].threeWay && (!t.left || !t.right)) {
      updateTab(t.id, t.left ? { right: path } : { left: path });
    } else {
      openAuto(path, "").catch((e) => setToast(errorMessage(e)));
    }
  };
  const canSave = canSaveTab(activeTab);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const mod = e.ctrlKey || e.metaKey;
      if (mod && e.key.toLowerCase() === "t") openTab("home");
      else if (mod && e.key.toLowerCase() === "w") closeTab(active);
      else if (mod && e.shiftKey && e.key.toLowerCase() === "s") canSave && setSaving(activeTab!);
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  useEffect(() => {
    if (!toast) return;
    const t = setTimeout(() => setToast(null), 4000);
    return () => clearTimeout(t);
  }, [toast]);

  const openFiles = (left: string, right: string) => openTab(sessionForFiles(left, right), left, right);

  const renderSession = (t: Tab) => {
    const two = {
      left: t.left,
      right: t.right,
      settings: t.settings,
      onSettingsChange: (settings: object) => updateTab(t.id, { settings: { ...settings } }),
      onPathsChange: (left: string, right: string) => updateTab(t.id, { left, right }),
      onDirtyChange: (dirty: boolean) => (dirty ? dirtyTabs.current.add(t.id) : dirtyTabs.current.delete(t.id)),
    };
    const three = {
      left: t.left,
      right: t.right,
      base: t.base ?? "",
      output: t.output ?? "",
      settings: t.settings,
      onSettingsChange: two.onSettingsChange,
      onDirtyChange: two.onDirtyChange,
      onPathsChange: (p: MergePaths) => updateTab(t.id, p),
    };
    switch (t.kind) {
      case "home":
        return (
          <Home
            onNew={(kind) => (kind === "profiles" ? openProfiles() : updateTab(t.id, { kind }))}
            saved={saved}
            onOpenSaved={openSaved}
            onRenameSaved={(s, name) =>
              api
                .renameSession(s.id, name)
                .then((r) => {
                  setTabs((ts) => ts.map((x) => (x.sessionId === r.id ? { ...x, sessionName: r.name } : x)));
                  return refreshSaved();
                })
                .catch((e) => setToast(errorMessage(e)))
            }
            onDeleteSaved={(s) =>
              api
                .deleteSession(s.id)
                .then(() => {
                  // Open tabs keep working, they just stop being linked to the deleted session.
                  setTabs((ts) =>
                    ts.map((x) => (x.sessionId === s.id ? { ...x, sessionId: undefined, sessionName: undefined } : x)),
                  );
                  return refreshSaved();
                })
                .catch((e) => setToast(errorMessage(e)))
            }
          />
        );
      case "profiles":
        return <Profiles />;
      case "text":
        return <TextCompare {...two} onOpenHex={() => updateTab(t.id, { kind: "hex" })} />;
      case "merge":
        return <TextMerge {...three} />;
      case "table":
        return <TableCompare {...two} />;
      case "picture":
        return <PictureCompare {...two} />;
      case "folder":
        return (
          <FolderCompare
            {...two}
            onOpenFiles={(left, right, rules) => {
              const kind = sessionForFiles(left, right);
              // Text rules only mean something to a text compare.
              openTab(kind, left, right, kind === "text" ? { settings: { options: rules } } : {});
            }}
            onOpenHex={(left, right) => openTab("hex", left, right)}
            onOpenSync={(left, right) => openTab("sync", left, right)}
          />
        );
      case "sync":
        return <FolderSync {...two} onOpenFiles={openFiles} />;
      case "folderMerge":
        return (
          <FolderMerge
            {...three}
            onOpenFiles={openFiles}
            onOpenTextMerge={(p) => openTab("merge", p.left, p.right, { base: p.base, output: p.output })}
          />
        );
      case "hex":
        return <HexCompare left={t.left} right={t.right} onPathsChange={two.onPathsChange} />;
      case "registry":
        return <RegistryCompare {...two} />;
      case "version":
        return <VersionCompare {...two} />;
      case "mp3":
        return <Mp3Compare {...two} />;
    }
  };

  return (
    <ShellContext.Provider value={{ openProfiles, leftSelection, selectLeft, compareToLeft }}>
      <div className="app">
        <nav className="tabbar">
          {tabs.map((t) => (
            <div
              key={t.id}
              className={`tab${t.id === active ? " active" : ""}`}
              onClick={() => setActive(t.id)}
              onAuxClick={(e) => e.button === 1 && closeTab(t.id)}
              title={t.left && t.right ? `${t.left}\n${t.right}` : undefined}
            >
              <SessionIcon kind={t.kind} />
              <span className="tab-title">{tabTitle(t)}</span>
              <button
                className="tab-close"
                aria-label="Close tab"
                onClick={(e) => {
                  e.stopPropagation();
                  closeTab(t.id);
                }}
              >
                <X size={13} />
              </button>
            </div>
          ))}
          <button className="tab-new" title="New session (Ctrl+T)" aria-label="New session" onClick={() => openTab("home")}>
            <Plus size={15} />
          </button>
          <span className="spacer" />
          {leftSelection && (
            <span className="left-chip" title={`Left side for compare:\n${leftSelection}\nRight-click another item → Compare to`}>
              <ToolIcon name="selectLeft" size={14} />
              Left: {basename(leftSelection)}
              <button aria-label="Clear left side" onClick={() => selectLeft(null)}>
                <X size={12} />
              </button>
            </span>
          )}
          <button
            className="tab-action"
            title="Save session (Ctrl+Shift+S)"
            disabled={!canSave}
            onClick={() => canSave && setSaving(activeTab!)}
          >
            <Save size={14} /> Save session
          </button>
          <button className="tab-action" title="Connection profiles" onClick={openProfiles}>
            <Globe size={14} /> Profiles
          </button>
          <button className="tab-action" title="Settings and updates" onClick={() => setSettingsOpen(true)}>
            <Settings size={14} /> Settings
          </button>
        </nav>

        {updater.bannerVisible && (
          <div className="update-banner">
            {updater.phase.kind === "downloading" ? (
              <>
                <span>
                  Downloading PCompare {updater.phase.update.version}
                  {updater.phase.total
                    ? ` — ${Math.round((updater.phase.done / updater.phase.total) * 100)}%`
                    : "…"}
                </span>
                <progress value={updater.phase.done} max={updater.phase.total ?? undefined} />
              </>
            ) : updater.phase.kind === "available" ? (
              <>
                <span>
                  <b>PCompare {updater.phase.update.version}</b> is available (you have {updater.version}).
                </span>
                {updater.phase.update.body && (
                  <span className="muted update-notes" title={updater.phase.update.body}>
                    {updater.phase.update.body.split("\n")[0]}
                  </span>
                )}
                <span className="spacer" />
                <label className="check muted">
                  <input
                    type="checkbox"
                    checked={updater.autoCheck}
                    onChange={(e) => updater.setAutoCheck(e.target.checked)}
                  />
                  Check automatically
                </label>
                <button onClick={updater.dismiss}>Later</button>
                <button className="primary" onClick={updater.install}>
                  Install &amp; restart
                </button>
              </>
            ) : null}
          </div>
        )}

        {tabs.map((t) => (
          // Inactive tabs stay mounted so their state and scroll position survive.
          <main key={t.id} className="tabpanel" hidden={t.id !== active}>
            {renderSession(t)}
          </main>
        ))}

        {saving && canSaveTab(saving) && (
          <SaveSessionDialog
            kind={saving.kind}
            left={saving.left}
            right={saving.right}
            base={saving.base}
            output={saving.output}
            settings={saving.settings ?? {}}
            sessionId={saving.sessionId}
            defaultName={tabTitle(saving)}
            onClose={() => setSaving(null)}
            onSaved={(s) => {
              updateTab(saving.id, { sessionId: s.id, sessionName: s.name });
              setToast(`Saved session “${s.name}”`);
              refreshSaved();
            }}
          />
        )}
        {settingsOpen && <AppSettingsDialog updater={updater} onClose={() => setSettingsOpen(false)} />}
        {toast && <div className="toast">{toast}</div>}
      </div>
    </ShellContext.Provider>
  );
}
