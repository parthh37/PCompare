import { getCurrentWebview } from "@tauri-apps/api/webview";
import { CircleQuestionMark, Globe, Layers, Plus, Save, Settings, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { AppSettingsDialog } from "./components/AppSettingsDialog";
import { DbCompare } from "./components/DbCompare";
import { CloseGuardDialog } from "./components/CloseGuardDialog";
import { CrashedView, ErrorBoundary } from "./components/ErrorBoundary";
import { FolderCompare } from "./components/FolderCompare";
import { FolderMerge } from "./components/FolderMerge";
import { FolderSync } from "./components/FolderSync";
import { HexCompare } from "./components/HexCompare";
import { Home } from "./components/Home";
import { Mp3Compare } from "./components/Mp3Compare";
import { PictureCompare } from "./components/PictureCompare";
import { Profiles } from "./components/Profiles";
import { RegistryCompare } from "./components/RegistryCompare";
import { RemoteAuthPrompt } from "./components/RemoteAuthPrompt"; // bc5:remote-write
import { SaveSessionDialog } from "./components/SaveSessionDialog";
import { SessionIcon } from "./components/SessionIcon";
import { ToolIcon } from "./components/icons";
import { TableCompare } from "./components/TableCompare";
import { TextCompare } from "./components/TextCompare";
import { TextEditor } from "./components/TextEditor"; // bc5:editor-patch
import { PatchViewer } from "./components/PatchViewer"; // bc5:editor-patch
import { TextMerge } from "./components/TextMerge";
import { VersionCompare } from "./components/VersionCompare";
import { api, errorMessage, type SavedSession } from "./lib/api";
import { dbApi, dbPathLabel, useDbConnectionNames } from "./lib/dbApi";
import * as closeGuard from "./lib/closeGuard";
import { basename } from "./lib/format";
import type { MergePaths } from "./lib/sessionProps";
import {
  SESSIONS,
  extension,
  isCompareKind,
  sessionForFiles,
  type CompareKind,
  type SessionKind,
} from "./lib/sessions";
import { ShellContext } from "./lib/shell";
import { useUpdater } from "./lib/useUpdater";
// bc5:app-shell — prefs/theme, keymap, workspaces, recent sessions, help
import { HelpPanel } from "./components/HelpPanel";
import { SaveWorkspaceDialog } from "./components/SaveWorkspaceDialog";
import { topicFor } from "./help/topics";
import { matches, shortcutLabel } from "./lib/keymap";
import { getPrefs, prefsReady, usePref } from "./lib/prefs";
import "./lib/theme";
import { workspaceApi, type RecentSession, type Workspace, type WorkspaceTab } from "./lib/workspaceApi";
import { openExternalIfAlways } from "./lib/convertApi"; // bc5:converters
import { isContainerPath } from "./lib/archiveApi"; // bc5:archives
import { toLaunchOptions, type LaunchOptions } from "./lib/cliApi"; // bc5:reports-script
import { vcsApi } from "./lib/vcsApi"; // araxis:vcs
// araxis:text-review — comparison archives (.pcmpz) open as read-only review tabs.
import { ReviewArchiveView } from "./components/ReviewArchiveView";
import { isReviewArchivePath } from "./lib/reviewApi";
// araxis:text-review end
import { dropOnPathField, highlightPathField } from "./lib/pathDrop"; // araxis:folder
// automation:ui — the Sync Automation tab (scheduled / real-time folder syncs)
import { CalendarClock } from "lucide-react";
import { AutomationPage, type AutomationPrefill } from "./components/AutomationPage";
import { onOpenRequest, type SyncTask } from "./lib/automationApi";
// automation:ui end

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
  /** bc5:reports-script — command-line tab labels (`/title1=`, `/title2=`) and read-only sides. */
  labels?: { left?: string; right?: string };
  readOnly?: { left: boolean; right: boolean };
  /** araxis:vcs — opened by `pcompare --wait`: closing the tab lets that process exit. */
  waitToken?: string;
}

/** `dbNames` titles Database Compare tabs by connection name rather than `db://` id. */
function tabTitle(t: Tab, dbNames: Record<string, string>): string {
  if (t.sessionName) return t.sessionName;
  // bc5:reports-script — `/title1=` / `/title2=` from the command line.
  if (t.labels?.left || t.labels?.right) {
    const [l, r] = [t.labels.left || basename(t.left), t.labels.right || basename(t.right)];
    return l && r && l !== r ? `${l} ↔ ${r}` : l || r;
  }
  if (!isCompareKind(t.kind) || !(t.left || t.right)) return SESSIONS[t.kind].label;
  const label = (p: string) => (t.kind === "database" ? dbPathLabel(p, dbNames) : basename(p));
  if (!t.left || !t.right) return label(t.left || t.right);
  const l = label(t.left);
  const r = label(t.right);
  return l === r ? l : `${l} ↔ ${r}`;
}

const canSaveTab = (t: Tab | undefined): t is Tab & { kind: CompareKind } =>
  !!t && isCompareKind(t.kind) && !!t.left && (!!t.right || !!SESSIONS[t.kind].single); // bc5:editor-patch

// bc5:app-shell — a tab as stored in a workspace: everything but the runtime id.
function toWorkspaceTab(t: Tab): WorkspaceTab {
  const { id, ...rest } = t;
  void id;
  return rest;
}

export default function App() {
  const nextId = useRef(1);
  const [tabs, setTabs] = useState<Tab[]>([{ id: 0, kind: "home", left: "", right: "" }]);
  const [active, setActive] = useState(0);
  const [saved, setSaved] = useState<SavedSession[]>([]);
  const [saving, setSaving] = useState<Tab | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  /** Tabs whose session reported unsaved edits. */
  const dirtyTabs = useRef(new Set<number>());
  /** How each dirty tab saves its edits, for Save all when closing or quitting. */
  const savers = useRef(new Map<number, closeGuard.SaveTab>());
  /** What the close guard was last told: whether any tab is dirty. */
  const reportedUnsaved = useRef(false);
  const reportUnsaved = useCallback(() => {
    const any = dirtyTabs.current.size > 0;
    if (any === reportedUnsaved.current) return;
    reportedUnsaved.current = any;
    closeGuard.setUnsaved(any).catch(() => {});
  }, []);
  const markDirty = (id: number, dirty: boolean, save?: closeGuard.SaveTab) => {
    if (dirty) dirtyTabs.current.add(id);
    else dirtyTabs.current.delete(id);
    if (dirty && save) savers.current.set(id, save);
    else savers.current.delete(id);
    reportUnsaved();
  };
  const [closeAsk, setCloseAsk] = useState<closeGuard.CloseAsk | null>(null);
  const updater = useUpdater(setToast);
  const [settingsOpen, setSettingsOpen] = useState(false);
  /** Bumped to remount the sessions after one crashed while closing. */
  const [workareaEpoch, setWorkareaEpoch] = useState(0);
  const dbNames = useDbConnectionNames();

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

  // bc5:app-shell — workspaces, recent (auto-saved) sessions, help, startup restore.
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [recent, setRecent] = useState<RecentSession[]>([]);
  const [savingWorkspace, setSavingWorkspace] = useState(false);
  /** Help page shown in the side panel, or null when it's closed. */
  const [helpTopic, setHelpTopic] = useState<string | null>(null);
  /** Set once startup has restored/opened its tabs; until then the initial Home tab isn't recorded. */
  const startupDone = useRef(false);
  usePref("keys"); // tooltips show the current shortcuts
  const refreshWorkspaces = useCallback(
    () =>
      workspaceApi
        .list()
        .then(setWorkspaces)
        .catch((e) => setToast(errorMessage(e))),
    [],
  );
  const refreshRecent = useCallback(
    () =>
      workspaceApi
        .recent()
        .then(setRecent)
        .catch((e) => setToast(errorMessage(e))),
    [],
  );
  useEffect(() => {
    refreshWorkspaces();
    refreshRecent();
  }, [refreshWorkspaces, refreshRecent]);

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

  // automation:ui — one Automation tab; `prefill` (from Folder Sync's "Automate…") opens its editor.
  const [automationPrefill, setAutomationPrefill] = useState<AutomationPrefill | null>(null);
  const prefillSeq = useRef(0);
  const openAutomation = (prefill?: Partial<SyncTask>) => {
    if (prefill) setAutomationPrefill({ seq: ++prefillSeq.current, task: prefill });
    focusOr((t) => t.kind === "automation", () => openTab("automation"));
  };
  const openAutomationRef = useRef(openAutomation);
  openAutomationRef.current = openAutomation;
  // The tray menu's "Sync Automation…".
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    onOpenRequest(() => openAutomationRef.current())
      .then((u) => (cancelled ? u() : (unlisten = u)))
      .catch(() => {});
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);
  const clearAutomationPrefill = useCallback(() => setAutomationPrefill(null), []);
  // automation:ui end

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
    // bc5:reports-script — `extra` carries command-line titles / read-only sides.
    async (left: string, right: string, extra: Partial<Tab> = {}) => {
      if (isReviewArchivePath(left) && !right) return openTab("review", left, "", extra); // araxis:text-review
      // A missing side takes the other's kind, so one folder still opens a folder compare.
      const [lk, rk] = await Promise.all([api.pathKind(left), api.pathKind(right || left)]);
      if (lk === "directory" && rk === "directory") return openTab("folder", left, right, extra);
      // bc5:archives — archives, snapshots and svn: URLs (with each other or a folder) open as folders.
      const asFolder = (p: string, k: string) => k === "directory" || isContainerPath(p);
      if (asFolder(left, lk) && asFolder(right || left, rk)) return openTab("folder", left, right, extra);
      // bc5:converters — pairs matching an "always use" external tool open there instead.
      if (right && (await openExternalIfAlways(left, right))) {
        if (extra.waitToken) vcsApi.waitDone(extra.waitToken).catch(() => {}); // araxis:vcs — no tab to wait for
        return;
      }
      let kind = sessionForFiles(left, right || left);
      // `.db` is ambiguous (Thumbs.db, BerkeleyDB...): only SQLite files open as databases.
      if (kind === "hex" && right && extension(left) === "db" && extension(right) === "db") {
        if (await dbApi.isSqlite([left, right]).catch(() => false)) kind = "database";
      }
      openTab(kind, left, right, extra);
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

  // bc5:app-shell — "Auto-save sessions on close" (BC's default): a tab linked
  // to a saved session writes its paths and settings back; any other compare
  // is remembered under Recent sessions on Home.
  const autoSaveOnClose = (t: Tab) => {
    if (!getPrefs().autoSaveSessions || !isCompareKind(t.kind) || !(t.left || t.right)) return;
    const content = {
      kind: t.kind,
      left: t.left,
      right: t.right,
      base: t.base || undefined,
      output: t.output || undefined,
    };
    if (t.sessionId) {
      workspaceApi
        .autosaveSession(t.sessionId, { ...content, name: t.sessionName ?? "", settings: t.settings ?? {} })
        .then(refreshSaved)
        .catch((e) => setToast(errorMessage(e)));
    } else {
      workspaceApi
        .addRecent({ ...content, settings: t.settings ?? null })
        .then(refreshRecent)
        .catch((e) => setToast(errorMessage(e)));
    }
  };

  const closeTab = (id: number) => {
    if (dirtyTabs.current.has(id) && !window.confirm("This tab has unsaved changes. Close it and discard them?")) return;
    markDirty(id, false);
    const closing = tabs.find((t) => t.id === id); // bc5:app-shell
    if (closing) autoSaveOnClose(closing);
    if (closing?.waitToken) vcsApi.waitDone(closing.waitToken).catch((e) => setToast(errorMessage(e))); // araxis:vcs
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
  const openLaunch = useCallback(
    async (launch: LaunchOptions | string[]) => {
      // bc5:reports-script — switches: forced viewer (`/fv=`), tab titles, read-only sides.
      const o = toLaunchOptions(launch);
      const [left, right, base, output] = o.paths;
      // araxis:vcs — `--wait` launches report back when their tab closes.
      if (!left) {
        if (o.waitToken) vcsApi.waitDone(o.waitToken).catch(() => {});
        return;
      }
      const extra: Partial<Tab> = {};
      if (o.waitToken) extra.waitToken = o.waitToken;
      // araxis:vcs end
      if (o.titleLeft || o.titleRight) extra.labels = { left: o.titleLeft ?? undefined, right: o.titleRight ?? undefined };
      if (o.readOnlyLeft || o.readOnlyRight) extra.readOnly = { left: o.readOnlyLeft, right: o.readOnlyRight };
      if (base) Object.assign(extra, { base, output: output ?? o.mergeOutput ?? "" });
      if (o.viewer && o.viewer in SESSIONS) return openTab(o.viewer, left, right ?? "", extra);
      if (!base) return openAuto(left, right ?? "", extra);
      const kind = (await api.pathKind(base)) === "directory" ? "folderMerge" : "merge";
      openTab(kind, left, right ?? "", extra);
    },
    [openAuto, openTab],
  );

  // bc5:app-shell — replaces the open tabs with a workspace's. Saved-session
  // links pick up renames, and links to sessions deleted since are dropped.
  const loadTabs = async (wsTabs: WorkspaceTab[], activeIndex: number) => {
    const names = await api
      .listSessions()
      .then((all) => new Map(all.map((s) => [s.id, s.name])))
      .catch(() => null);
    const restored: Tab[] = wsTabs
      .filter((t) => t.kind in SESSIONS)
      .map((t) => {
        const linked = !t.sessionId || !names || names.has(t.sessionId);
        return {
          ...t,
          settings: t.settings ?? undefined,
          sessionId: linked ? t.sessionId : undefined,
          sessionName: linked ? ((t.sessionId && names?.get(t.sessionId)) ?? t.sessionName) : undefined,
          id: nextId.current++,
        };
      });
    if (!restored.length) return;
    dirtyTabs.current.clear();
    savers.current.clear();
    reportUnsaved();
    // araxis:vcs — replaced tabs close too, as far as `--wait` launches are concerned.
    tabs.forEach((t) => t.waitToken && vcsApi.waitDone(t.waitToken).catch(() => {}));
    // araxis:vcs end
    setTabs(restored);
    setActive(restored[Math.min(Math.max(0, activeIndex), restored.length - 1)].id);
  };

  const openWorkspace = async (w: Workspace) => {
    if (
      dirtyTabs.current.size &&
      !window.confirm(`Opening “${w.name}” closes the open tabs, and some have unsaved changes. Discard them?`)
    )
      return;
    tabs.forEach(autoSaveOnClose);
    await loadTabs(w.tabs, w.active);
  };

  // Startup: restore the previous run's tabs (if enabled), then open the
  // command line's compare in front of them. Runs once (StrictMode re-runs effects).
  const startupStarted = useRef(false);
  useEffect(() => {
    if (startupStarted.current) return;
    startupStarted.current = true;
    (async () => {
      const [prefs, last, launch] = await Promise.all([
        prefsReady,
        workspaceApi.last().catch(() => null),
        api.launchArgs().catch(() => [] as string[]),
      ]);
      if (prefs.restoreWorkspace && last?.tabs.length) await loadTabs(last.tabs, last.active);
      await openLaunch(launch).catch((e) => setToast(errorMessage(e)));
      startupDone.current = true;
      // Record the startup set once React has applied it.
      window.setTimeout(() => saveLast.current(), 500);
    })();
    // One-shot: later launches arrive through onLaunchPaths below.
  }, []);

  // Single instance: `pcompare a b` while this window is open lands here.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    workspaceApi
      .onLaunchPaths((paths) => openLaunch(paths).catch((e) => setToast(errorMessage(e))))
      .then((u) => (cancelled ? u() : (unlisten = u)))
      .catch(() => {});
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [openLaunch]);

  // The open tabs are recorded continuously (debounced), so the last set
  // survives closing the window, a crash or an update restart.
  const saveLast = useRef(() => {});
  saveLast.current = () => {
    if (!startupDone.current) return;
    const index = tabs.findIndex((t) => t.id === active);
    workspaceApi.setLast(tabs.map(toWorkspaceTab), Math.max(0, index)).catch(() => {});
  };
  useEffect(() => {
    const timer = window.setTimeout(() => saveLast.current(), 400);
    return () => window.clearTimeout(timer);
  }, [tabs, active]);
  useEffect(() => {
    const flush = () => saveLast.current();
    window.addEventListener("pagehide", flush);
    return () => window.removeEventListener("pagehide", flush);
  }, []);

  // Dropping two files/folders onto the window starts a comparison.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    getCurrentWebview()
      .onDragDropEvent((e) => {
        // araxis:folder — hovering marks the path field under the cursor; one path dropped on it fills it.
        const p = e.payload;
        if (p.type === "enter" || p.type === "over") highlightPathField(p.position);
        else highlightPathField(null);
        if (p.type === "drop" && p.paths.length === 1 && dropOnPathField(p.paths[0], p.position)) return;
        // araxis:folder end
        if (e.payload.type !== "drop") return;
        const [first, second] = e.payload.paths;
        if (second) return openAuto(first, second);
        if (first) dropOne.current(first);
      })
      .then((u) => (unlisten = u))
      .catch(() => {});
    return () => unlisten?.();
  }, [openAuto]);

  // Closing the window or quitting with unsaved edits: Rust stops it and asks here.
  useEffect(() => {
    const unlisten = closeGuard.onAsk((ask) => {
      if (dirtyTabs.current.size) setCloseAsk(ask);
      else void closeGuard.proceed(ask.quit);
    });
    return () => void unlisten.then((u) => u()).catch(() => {});
  }, []);
  const saveAllAndClose = async (quit: boolean) => {
    const left = await closeGuard.saveAll(dirtyTabs.current, savers.current, setActive, () => dirtyTabs.current);
    if (!left.length) {
      setCloseAsk(null);
      await closeGuard.proceed(quit);
      return;
    }
    setCloseAsk(null);
    setActive(left[0]);
    const names = tabs.filter((t) => left.includes(t.id)).map((t) => tabTitle(t, dbNames));
    setToast(`Not saved: ${names.join(", ")}. Save or close ${left.length === 1 ? "it" : "them"}, or choose Discard.`);
  };

  const activeTab = tabs.find((t) => t.id === active);

  /** One dropped path fills the empty side of the active compare, or opens on its own. */
  const dropOne = useRef((_path: string) => {});
  dropOne.current = (path: string) => {
    const t = activeTab;
    // araxis:text-review — a dropped .pcmpz opens as its own tab unless an empty archive tab is waiting for it.
    if (isReviewArchivePath(path) && !(t?.kind === "review" && !t.left))
      return void openAuto(path, "").catch((e) => setToast(errorMessage(e)));
    // araxis:text-review end
    // bc5:editor-patch — one-file sessions take the drop only while empty and unedited.
    if (t && isCompareKind(t.kind) && SESSIONS[t.kind].single) {
      if (!t.left && !dirtyTabs.current.has(t.id)) updateTab(t.id, { left: path });
      else openAuto(path, "").catch((e) => setToast(errorMessage(e)));
    } else if (t && isCompareKind(t.kind) && !SESSIONS[t.kind].threeWay && (!t.left || !t.right)) {
      updateTab(t.id, t.left ? { right: path } : { left: path });
    } else {
      openAuto(path, "").catch((e) => setToast(errorMessage(e)));
    }
  };
  const canSave = canSaveTab(activeTab);

  // bc5:app-shell — F1 / "?" toggles the help page for the view in front.
  const toggleHelp = () => {
    const id = topicFor(activeTab?.kind ?? "home").id;
    setHelpTopic((cur) => (cur === id ? null : id));
  };
  const cycleTab = (step: number) => {
    const i = tabs.findIndex((t) => t.id === active);
    setActive(tabs[(i + step + tabs.length) % tabs.length].id);
  };

  useEffect(() => {
    // bc5:app-shell — bindings come from keymap.ts (rebindable in Settings → Keyboard).
    const onKey = (e: KeyboardEvent) => {
      if (matches(e, "app.newTab")) openTab("home");
      else if (matches(e, "app.closeTab")) closeTab(active);
      else if (matches(e, "app.saveSession")) canSave && setSaving(activeTab!);
      else if (matches(e, "app.saveWorkspace")) setSavingWorkspace(true);
      else if (matches(e, "app.nextTab")) cycleTab(1);
      else if (matches(e, "app.prevTab")) cycleTab(-1);
      else if (matches(e, "app.settings")) setSettingsOpen(true);
      else if (matches(e, "app.help")) toggleHelp();
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

  // bc5:converters — pairs matching an "always use" external tool open there instead.
  const openFiles = (left: string, right: string) =>
    void openExternalIfAlways(left, right)
      .then((launched) => !launched && openTab(sessionForFiles(left, right), left, right))
      .catch((e) => setToast(errorMessage(e)));

  const renderSession = (t: Tab) => {
    const two = {
      left: t.left,
      right: t.right,
      settings: t.settings,
      onSettingsChange: (settings: object) => updateTab(t.id, { settings: { ...settings } }),
      onPathsChange: (left: string, right: string) => updateTab(t.id, { left, right }),
      // Block body: sessions report this from `useEffect(() => report(dirty))`, so a
      // returned Set/boolean would become the effect's "cleanup" and crash on close.
      onDirtyChange: (dirty: boolean, save?: closeGuard.SaveTab) => {
        markDirty(t.id, dirty, save);
      },
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
            onNew={(kind) =>
              kind === "profiles"
                ? openProfiles()
                : kind === "automation" // automation:ui
                  ? openAutomation()
                  : updateTab(t.id, { kind })
            }
            saved={saved}
            onOpenSaved={openSaved}
            onImported={(count) => {
              setToast(`Imported ${count} session${count === 1 ? "" : "s"}.`);
              refreshSaved();
            }}
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
            // bc5:app-shell — workspaces and recent (auto-saved) sessions
            workspaces={workspaces}
            onOpenWorkspace={(w) => openWorkspace(w).catch((e) => setToast(errorMessage(e)))}
            onRenameWorkspace={(w, name) =>
              workspaceApi
                .rename(w.id, name)
                .then(refreshWorkspaces)
                .catch((e) => setToast(errorMessage(e)))
            }
            onDeleteWorkspace={(w) =>
              workspaceApi
                .remove(w.id)
                .then(refreshWorkspaces)
                .catch((e) => setToast(errorMessage(e)))
            }
            recent={recent}
            onOpenRecent={(r) =>
              focusOr(
                (x) =>
                  !x.sessionId &&
                  x.kind === r.kind &&
                  x.left === r.left &&
                  x.right === r.right &&
                  (x.base ?? "") === (r.base ?? "") &&
                  (x.output ?? "") === (r.output ?? ""),
                () =>
                  openTab(r.kind, r.left, r.right, { base: r.base, output: r.output, settings: r.settings ?? {} }),
              )
            }
            onRemoveRecent={(r) =>
              workspaceApi
                .removeRecent(r.id)
                .then(refreshRecent)
                .catch((e) => setToast(errorMessage(e)))
            }
            onClearRecent={() =>
              workspaceApi
                .clearRecent()
                .then(refreshRecent)
                .catch((e) => setToast(errorMessage(e)))
            }
          />
        );
      case "profiles":
        return <Profiles />;
      // automation:ui
      case "automation":
        return (
          <AutomationPage
            prefill={automationPrefill}
            onPrefillUsed={clearAutomationPrefill}
            onToast={setToast}
            onHelp={() => setHelpTopic("automation")}
          />
        );
      // automation:ui end
      case "text":
        return (
          <TextCompare
            {...two}
            readOnlySides={t.readOnly} // bc5:reports-script — `/readonly`, `/lro`, `/rro`
            onOpenHex={() => updateTab(t.id, { kind: "hex" })}
            // bc5:editor-patch — the one chosen file, edited on its own in this tab
            onOpenEditor={(p) => updateTab(t.id, { kind: "edit", left: p, right: "", settings: undefined })}
            onOpenCompare={(l, r) => openTab("text", l, r)}
          />
        );
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
              // An orphan has one path: route by that file's type (the viewer shows the other side blank).
              const kind = left && right ? sessionForFiles(left, right) : sessionForFiles(left || right, left || right);
              // Text rules only mean something to a text compare.
              openTab(kind, left, right, kind === "text" ? { settings: { options: rules } } : {});
            }}
            onOpenHex={(left, right) => openTab("hex", left, right)}
            onOpenSync={(left, right, scope) => openTab("sync", left, right, scope ? { settings: { scope } } : {})} // bc5:sync-matrix
            onEditFile={(p) => openTab("edit", p)} // bc5:editor-patch
          />
        );
      case "sync":
        return <FolderSync {...two} onOpenFiles={openFiles} onAutomate={openAutomation /* automation:ui */} />;
      case "folderMerge":
        return (
          <FolderMerge
            {...three}
            onOpenFiles={openFiles}
            onOpenTextMerge={(p) => openTab("merge", p.left, p.right, { base: p.base, output: p.output })}
          />
        );
      case "hex":
        return <HexCompare {...two} />; // bc5:hex
      case "registry":
        return <RegistryCompare {...two} />;
      case "version":
        return <VersionCompare {...two} />;
      case "mp3":
        return <Mp3Compare {...two} />;
      case "database":
        return <DbCompare {...two} />;
      // bc5:editor-patch — Text Edit and Patch Viewer
      case "edit":
        return <TextEditor {...two} />;
      case "patch":
        return (
          <PatchViewer
            {...two}
            onOpenEditor={(p) => openTab("edit", p)}
            onOpenCompare={(left, memory) => openTab("text", left, "", { settings: { memory } })}
          />
        );
      case "review":
        return <ReviewArchiveView {...two} />; // araxis:text-review
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
              <span className="tab-title">{tabTitle(t, dbNames)}</span>
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
          <button
            className="tab-new"
            title={`New session (${shortcutLabel("app.newTab")})`}
            aria-label="New session"
            onClick={() => openTab("home")}
          >
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
            title={`Save session (${shortcutLabel("app.saveSession")})`}
            disabled={!canSave}
            onClick={() => canSave && setSaving(activeTab!)}
          >
            <Save size={14} /> Save session
          </button>
          {/* bc5:app-shell */}
          <button
            className="tab-action"
            title={`Save workspace: all open tabs (${shortcutLabel("app.saveWorkspace") || "no shortcut"})`}
            onClick={() => setSavingWorkspace(true)}
          >
            <Layers size={14} /> Workspace
          </button>
          <button className="tab-action" title="Connection profiles" onClick={openProfiles}>
            <Globe size={14} /> Profiles
          </button>
          {/* automation:ui */}
          <button
            className="tab-action"
            title="Sync Automation: scheduled and real-time folder syncs"
            onClick={() => openAutomation()}
          >
            <CalendarClock size={14} /> Automation
          </button>
          <button
            className="tab-action"
            title={`Settings: fonts, colors, keyboard and updates (${shortcutLabel("app.settings")})`}
            onClick={() => setSettingsOpen(true)}
          >
            <Settings size={14} /> Settings
          </button>
          {/* bc5:app-shell */}
          <button
            className={`tab-action help-toggle${helpTopic ? " active" : ""}`}
            title={`Help for this view (${shortcutLabel("app.help")})`}
            aria-label="Help"
            onClick={toggleHelp}
          >
            <CircleQuestionMark size={15} />
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

        {/* bc5:app-shell — the help panel docks to the right of the sessions. */}
        <div className="workarea">
          {/* A session that throws while being torn down (closing its tab) lands
              here; the tab list lives above, so remounting loses nothing. */}
          <ErrorBoundary
            key={workareaEpoch}
            fallback={(m) => (
              <CrashedView message={m} action="Reopen tabs" onAction={() => setWorkareaEpoch((n) => n + 1)} />
            )}
          >
            {tabs.map((t) => (
              // Inactive tabs stay mounted so their state and scroll position survive.
              <main key={t.id} className="tabpanel" hidden={t.id !== active}>
                <ErrorBoundary
                  fallback={(m) => <CrashedView message={m} action="Close tab" onAction={() => closeTab(t.id)} />}
                >
                  {renderSession(t)}
                </ErrorBoundary>
              </main>
            ))}
          </ErrorBoundary>
          {helpTopic && <HelpPanel topicId={helpTopic} onClose={() => setHelpTopic(null)} />}
        </div>

        {saving && canSaveTab(saving) && (
          <SaveSessionDialog
            kind={saving.kind}
            left={saving.left}
            right={saving.right}
            base={saving.base}
            output={saving.output}
            settings={saving.settings ?? {}}
            sessionId={saving.sessionId}
            defaultName={tabTitle(saving, dbNames)}
            onClose={() => setSaving(null)}
            onSaved={(s) => {
              updateTab(saving.id, { sessionId: s.id, sessionName: s.name });
              setToast(`Saved session “${s.name}”`);
              refreshSaved();
            }}
          />
        )}
        {closeAsk && (
          <CloseGuardDialog
            quit={closeAsk.quit}
            tabs={tabs
              .filter((t) => dirtyTabs.current.has(t.id))
              .map((t) => ({ id: t.id, title: tabTitle(t, dbNames), savable: savers.current.has(t.id) }))}
            onSaveAll={() => saveAllAndClose(closeAsk.quit)}
            onDiscard={() => {
              setCloseAsk(null);
              void closeGuard.proceed(closeAsk.quit);
            }}
            onCancel={() => setCloseAsk(null)}
          />
        )}
        {settingsOpen && <AppSettingsDialog updater={updater} onClose={() => setSettingsOpen(false)} />}
        {/* bc5:app-shell */}
        {savingWorkspace && (
          <SaveWorkspaceDialog
            tabs={tabs.map(toWorkspaceTab)}
            active={Math.max(0, tabs.findIndex((t) => t.id === active))}
            titles={tabs.map((t) => tabTitle(t, dbNames))}
            workspaces={workspaces}
            onClose={() => setSavingWorkspace(false)}
            onSaved={(w) => {
              setToast(`Saved workspace “${w.name}”`);
              refreshWorkspaces();
            }}
          />
        )}
        {/* bc5:remote-write — SSH servers' sign-in questions (one-time codes, passwords) */}
        <RemoteAuthPrompt />
        {toast && <div className="toast">{toast}</div>}
      </div>
    </ShellContext.Provider>
  );
}
