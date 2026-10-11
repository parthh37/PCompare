import { getCurrentWebview } from "@tauri-apps/api/webview";
import { useCallback, useEffect, useRef, useState } from "react";
import { FolderCompare } from "./components/FolderCompare";
import { HexCompare } from "./components/HexCompare";
import { Home } from "./components/Home";
import { Profiles } from "./components/Profiles";
import { SaveSessionDialog } from "./components/SaveSessionDialog";
import { TextCompare } from "./components/TextCompare";
import { api, errorMessage, type SavedKind, type SavedSession } from "./lib/api";
import { basename } from "./lib/format";
import { ShellContext } from "./lib/shell";

export type SessionKind = "home" | "text" | "folder" | "hex" | "profiles";

interface Tab {
  id: number;
  kind: SessionKind;
  left: string;
  right: string;
  /** View settings reported by the session, persisted when saved. */
  settings?: Record<string, unknown>;
  /** Set once the tab is backed by a saved session. */
  sessionId?: string;
  sessionName?: string;
}

const KIND_LABEL: Record<SessionKind, string> = {
  home: "Home",
  text: "Text Compare",
  folder: "Folder Compare",
  hex: "Hex Compare",
  profiles: "Connection Profiles",
};

const KIND_ICON: Record<SessionKind, string> = { home: "🏠", text: "📝", folder: "🗂️", hex: "🔢", profiles: "🌐" };

const isSavable = (k: SessionKind): k is SavedKind => k === "text" || k === "folder" || k === "hex";

function tabTitle(t: Tab): string {
  if (t.sessionName) return t.sessionName;
  if (!isSavable(t.kind) || !t.left || !t.right) return KIND_LABEL[t.kind];
  const l = basename(t.left);
  const r = basename(t.right);
  return l === r ? l : `${l} ↔ ${r}`;
}

export default function App() {
  const nextId = useRef(1);
  const [tabs, setTabs] = useState<Tab[]>([{ id: 0, kind: "home", left: "", right: "" }]);
  const [active, setActive] = useState(0);
  const [saved, setSaved] = useState<SavedSession[]>([]);
  const [saving, setSaving] = useState<Tab | null>(null);
  const [toast, setToast] = useState<string | null>(null);

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
      () => openTab(s.kind, s.left, s.right, { settings: s.settings ?? {}, sessionId: s.id, sessionName: s.name }),
    );

  /** Picks the session type from what the two paths are. */
  const openAuto = useCallback(
    async (left: string, right: string) => {
      const [lk, rk] = await Promise.all([api.pathKind(left), api.pathKind(right)]);
      openTab(lk === "directory" && rk === "directory" ? "folder" : "text", left, right);
    },
    [openTab],
  );

  const updateTab = (id: number, patch: Partial<Tab>) =>
    setTabs((ts) => ts.map((t) => (t.id === id ? { ...t, ...patch } : t)));

  const closeTab = (id: number) =>
    setTabs((ts) => {
      const idx = ts.findIndex((t) => t.id === id);
      const rest = ts.filter((t) => t.id !== id);
      const remaining = rest.length ? rest : [{ id: nextId.current++, kind: "home" as const, left: "", right: "" }];
      if (id === active) setActive(remaining[Math.max(0, idx - 1)].id);
      return remaining;
    });

  // `pcompare.exe <left> <right>` from the shell or a git difftool.
  useEffect(() => {
    api
      .launchArgs()
      .then((args) => {
        if (args.length >= 2) openAuto(args[0], args[1]);
      })
      .catch(() => {});
  }, [openAuto]);

  // Dropping two files/folders onto the window starts a comparison.
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    getCurrentWebview()
      .onDragDropEvent((e) => {
        if (e.payload.type === "drop" && e.payload.paths.length >= 2) {
          openAuto(e.payload.paths[0], e.payload.paths[1]);
        }
      })
      .then((u) => (unlisten = u))
      .catch(() => {});
    return () => unlisten?.();
  }, [openAuto]);

  const activeTab = tabs.find((t) => t.id === active);
  const canSave = !!activeTab && isSavable(activeTab.kind) && !!activeTab.left && !!activeTab.right;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const mod = e.ctrlKey || e.metaKey;
      if (mod && e.key.toLowerCase() === "t") openTab("home");
      else if (mod && e.key.toLowerCase() === "w") closeTab(active);
      else if (mod && e.key.toLowerCase() === "s") canSave && setSaving(activeTab!);
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

  return (
    <ShellContext.Provider value={{ openProfiles }}>
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
            <span>{KIND_ICON[t.kind]}</span>
            <span className="tab-title">{tabTitle(t)}</span>
            <button
              className="tab-close"
              onClick={(e) => {
                e.stopPropagation();
                closeTab(t.id);
              }}
            >
              ×
            </button>
          </div>
        ))}
        <button className="tab-new" title="New session (Ctrl+T)" onClick={() => openTab("home")}>
          +
        </button>
        <span className="spacer" />
        <button
          className="tab-action"
          title="Save session (Ctrl+S)"
          disabled={!canSave}
          onClick={() => canSave && setSaving(activeTab!)}
        >
          💾 Save
        </button>
        <button className="tab-action" title="Connection profiles" onClick={openProfiles}>
          🌐 Profiles
        </button>
      </nav>

      {tabs.map((t) => (
        // Inactive tabs stay mounted so their state and scroll position survive.
        <main key={t.id} className="tabpanel" hidden={t.id !== active}>
          {t.kind === "home" && (
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
          )}
          {t.kind === "profiles" && <Profiles />}
          {t.kind === "text" && (
            <TextCompare
              left={t.left}
              right={t.right}
              settings={t.settings}
              onSettingsChange={(settings) => updateTab(t.id, { settings: { ...settings } })}
              onPathsChange={(left, right) => updateTab(t.id, { left, right })}
              onOpenHex={() => updateTab(t.id, { kind: "hex" })}
            />
          )}
          {t.kind === "folder" && (
            <FolderCompare
              left={t.left}
              right={t.right}
              settings={t.settings}
              onSettingsChange={(settings) => updateTab(t.id, { settings: { ...settings } })}
              onPathsChange={(left, right) => updateTab(t.id, { left, right })}
              onOpenFiles={(left, right) => openTab("text", left, right)}
            />
          )}
          {t.kind === "hex" && (
            <HexCompare
              left={t.left}
              right={t.right}
              onPathsChange={(left, right) => updateTab(t.id, { left, right })}
            />
          )}
        </main>
      ))}

      {saving && isSavable(saving.kind) && (
        <SaveSessionDialog
          kind={saving.kind}
          left={saving.left}
          right={saving.right}
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
      {toast && <div className="toast">{toast}</div>}
    </div>
    </ShellContext.Provider>
  );
}
