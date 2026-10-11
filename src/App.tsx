import { getCurrentWebview } from "@tauri-apps/api/webview";
import { useCallback, useEffect, useRef, useState } from "react";
import { FolderCompare } from "./components/FolderCompare";
import { HexCompare } from "./components/HexCompare";
import { Home } from "./components/Home";
import { TextCompare } from "./components/TextCompare";
import { api } from "./lib/api";
import { basename } from "./lib/format";

export type SessionKind = "home" | "text" | "folder" | "hex";

interface Tab {
  id: number;
  kind: SessionKind;
  left: string;
  right: string;
}

const KIND_LABEL: Record<SessionKind, string> = {
  home: "Home",
  text: "Text Compare",
  folder: "Folder Compare",
  hex: "Hex Compare",
};

const KIND_ICON: Record<SessionKind, string> = { home: "🏠", text: "📝", folder: "🗂️", hex: "🔢" };

function tabTitle(t: Tab): string {
  if (t.kind === "home" || !t.left || !t.right) return KIND_LABEL[t.kind];
  const l = basename(t.left);
  const r = basename(t.right);
  return l === r ? l : `${l} ↔ ${r}`;
}

export default function App() {
  const nextId = useRef(1);
  const [tabs, setTabs] = useState<Tab[]>([{ id: 0, kind: "home", left: "", right: "" }]);
  const [active, setActive] = useState(0);

  const openTab = useCallback((kind: SessionKind, left = "", right = "") => {
    const id = nextId.current++;
    setTabs((t) => [...t, { id, kind, left, right }]);
    setActive(id);
  }, []);

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

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const mod = e.ctrlKey || e.metaKey;
      if (mod && e.key.toLowerCase() === "t") openTab("home");
      else if (mod && e.key.toLowerCase() === "w") closeTab(active);
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  return (
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
      </nav>

      {tabs.map((t) => (
        // Inactive tabs stay mounted so their state and scroll position survive.
        <main key={t.id} className="tabpanel" hidden={t.id !== active}>
          {t.kind === "home" && <Home onNew={(kind) => updateTab(t.id, { kind })} />}
          {t.kind === "text" && (
            <TextCompare
              left={t.left}
              right={t.right}
              onPathsChange={(left, right) => updateTab(t.id, { left, right })}
              onOpenHex={() => updateTab(t.id, { kind: "hex" })}
            />
          )}
          {t.kind === "folder" && (
            <FolderCompare
              left={t.left}
              right={t.right}
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
    </div>
  );
}
