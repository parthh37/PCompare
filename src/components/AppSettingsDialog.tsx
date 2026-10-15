import { CircleArrowUp, Keyboard, Palette, RotateCcw, Settings, Type } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import {
  bindingsWith,
  COMMANDS,
  eventBinding,
  findConflicts,
  formatBinding,
  normalizeBinding,
  type CommandDef,
} from "../lib/keymap";
import {
  DEFAULT_PREFS,
  EDITOR_FONT_SIZES,
  UI_FONT_SIZES,
  usePrefs,
  type DiffColorKey,
  type Prefs,
  type ThemePref,
} from "../lib/prefs";
import { SESSIONS } from "../lib/sessions";
import { cssVar, DIFF_COLOR_VARS } from "../lib/theme";
import type { Updater } from "../lib/useUpdater";
import "./AppShell.css";
import { Modal } from "./Modal";
import { GitBranch } from "lucide-react"; // araxis:vcs
import { VcsSetupPage } from "./VcsSetup"; // araxis:vcs
import { explorerApi, type MenuStatus } from "../lib/explorerApi";
import { errorMessage } from "../lib/api";

interface Props {
  updater: Updater;
  onClose: () => void;
}

type Page = "general" | "appearance" | "fonts" | "keyboard" | "vcs"; // araxis:vcs (vcs)

const PAGES: { id: Page; label: string; icon: typeof Settings }[] = [
  { id: "general", label: "General", icon: Settings },
  { id: "appearance", label: "Colors", icon: Palette },
  { id: "fonts", label: "Fonts", icon: Type },
  { id: "keyboard", label: "Keyboard", icon: Keyboard },
  { id: "vcs", label: "Source control", icon: GitBranch }, // araxis:vcs
];

function updateStatus({ phase, checkedAt }: Updater): string {
  const at = checkedAt
    ? ` Last checked ${checkedAt.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}.`
    : "";
  switch (phase.kind) {
    case "checking":
      return "Checking for updates…";
    case "available":
      return `Version ${phase.update.version} is available.`;
    case "downloading":
      return `Downloading version ${phase.update.version}…`;
    case "error":
      return `Update check failed: ${phase.message}`;
    case "idle":
      return checkedAt ? `You're on the latest version.${at}` : "Not checked yet this session.";
  }
}

// ---------- general ----------

function GeneralPage({ updater, prefs, set }: { updater: Updater; prefs: Prefs; set: SetPrefs }) {
  const { phase } = updater;
  const busy = phase.kind === "checking" || phase.kind === "downloading";
  return (
    <div className="settings-grid single">
      <fieldset>
        <legend>Startup and sessions</legend>
        <label className="field check">
          <input
            type="checkbox"
            checked={prefs.restoreWorkspace}
            onChange={(e) => set({ restoreWorkspace: e.target.checked })}
          />
          Restore last workspace on startup
        </label>
        <small className="muted settings-hint">
          The open tabs are always remembered when PCompare closes; comparisons given on the command line still open.
        </small>
        <label className="field check">
          <input
            type="checkbox"
            checked={prefs.autoSaveSessions}
            onChange={(e) => set({ autoSaveSessions: e.target.checked })}
          />
          Auto-save sessions on close
        </label>
        <small className="muted settings-hint">
          Closing a tab updates its saved session; unsaved comparisons go to Recent sessions on Home.
        </small>
      </fieldset>
      <ExplorerMenuSettings />
      <fieldset>
        <legend>Updates</legend>
        <div className="update-status">
          <span>
            <span>
              PCompare <b>{updater.version || "—"}</b>
            </span>
            <small className="muted">{updateStatus(updater)}</small>
          </span>
          {phase.kind === "available" ? (
            <button className="primary" onClick={updater.install}>
              Install &amp; restart
            </button>
          ) : (
            <button disabled={busy} onClick={updater.checkNow}>
              <CircleArrowUp size={14} /> {phase.kind === "checking" ? "Checking…" : "Check for updates"}
            </button>
          )}
        </div>
        <label className="field check">
          <input
            type="checkbox"
            checked={updater.autoCheck}
            onChange={(e) => updater.setAutoCheck(e.target.checked)}
          />
          Check for updates automatically (every hour)
        </label>
        <label className="field check">
          <input
            type="checkbox"
            checked={updater.reminders}
            disabled={!updater.autoCheck}
            onChange={(e) => updater.setReminders(e.target.checked)}
          />
          Remind me with a pop-up whenever a new version is available
        </label>
      </fieldset>
    </div>
  );
}

/** Windows only: the "Select Left Side" / "Compare with PCompare" right-click menu. */
function ExplorerMenuSettings() {
  const [status, setStatus] = useState<MenuStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    explorerApi
      .status()
      .then(setStatus)
      .catch(() => setStatus({ state: "unsupported" }));
  }, []);
  if (!status || status.state === "unsupported") return null;

  const apply = (enabled: boolean) => {
    setBusy(true);
    setError(null);
    explorerApi
      .set(enabled)
      .then(setStatus)
      .catch((e) => setError(errorMessage(e)))
      .finally(() => setBusy(false));
  };
  const on = status.state !== "disabled";
  return (
    <fieldset>
      <legend>Windows Explorer</legend>
      <label className="field check">
        <input type="checkbox" checked={on} disabled={busy} onChange={(e) => apply(e.target.checked)} />
        Add PCompare to the right-click menu
      </label>
      <small className="muted settings-hint">
        Right-click a file or folder → <b>Select Left Side for Compare</b>, then another → <b>Compare to</b>; or select
        two folders or files (three for a merge) → <b>Compare with PCompare</b>. On Windows 11 these are under{" "}
        <b>Show more options</b> (or Shift+right-click).
      </small>
      {status.state === "stale" && (
        <small className="muted settings-hint">
          The menu currently opens another copy of PCompare ({status.exe}).
        </small>
      )}
      {(status.state === "stale" || status.state === "missing") && (
        <div className="settings-actions">
          <button disabled={busy} onClick={() => apply(true)}>
            Repair menu
          </button>
        </div>
      )}
      {error && <small className="settings-hint" style={{ color: "var(--danger-text)" }}>{error}</small>}
    </fieldset>
  );
}

// ---------- colours ----------

const COLOR_ROWS: { key: DiffColorKey; label: string }[] = [
  { key: "changed", label: "Changed lines" },
  { key: "inserted", label: "Inserted lines (right only)" },
  { key: "deleted", label: "Deleted lines (left only)" },
  { key: "intralineDeleted", label: "Intra-line highlight, left" },
  { key: "intralineInserted", label: "Intra-line highlight, right" },
  { key: "unimportant", label: "Unimportant text" },
];

interface Rgba {
  r: number;
  g: number;
  b: number;
  a: number;
}

/** Parses the colour forms used in styles.css and written by this dialog. */
function parseColor(text: string): Rgba | null {
  const s = text.trim();
  const hex = /^#([0-9a-f]{3,8})$/i.exec(s)?.[1];
  if (hex) {
    const full = hex.length <= 4 ? [...hex].map((c) => c + c).join("") : hex;
    const n = (i: number) => parseInt(full.slice(i, i + 2), 16);
    return { r: n(0), g: n(2), b: n(4), a: full.length === 8 ? n(6) / 255 : 1 };
  }
  const fn = /^rgba?\(([^)]+)\)$/i.exec(s)?.[1];
  if (fn) {
    const [r, g, b, a = "1"] = fn.split(/[\s,/]+/).filter(Boolean);
    return { r: +r, g: +g, b: +b, a: a.endsWith("%") ? parseFloat(a) / 100 : +a };
  }
  return null;
}

const toHex = ({ r, g, b }: Rgba) => "#" + [r, g, b].map((v) => Math.round(v).toString(16).padStart(2, "0")).join("");
const toCss = ({ r, g, b, a }: Rgba) =>
  a >= 1 ? toHex({ r, g, b, a }) : `rgba(${r}, ${g}, ${b}, ${Math.round(a * 100) / 100})`;

function ColorRow({ label, colorKey, prefs, set }: { label: string; colorKey: DiffColorKey; prefs: Prefs; set: SetPrefs }) {
  const override = prefs.diffColors[colorKey];
  // The effective colour: the override, or the theme's default from styles.css.
  const value = parseColor(override ?? cssVar(DIFF_COLOR_VARS[colorKey])) ?? { r: 128, g: 128, b: 128, a: 1 };
  const update = (patch: Partial<Rgba>) =>
    set((p) => ({ diffColors: { ...p.diffColors, [colorKey]: toCss({ ...value, ...patch }) } }));
  return (
    <div className="color-row">
      <span className="color-swatch" style={{ background: toCss(value) }} />
      <span className="color-label">
        {label}
        {override && <small className="muted"> (custom)</small>}
      </span>
      <input
        type="color"
        value={toHex(value)}
        onChange={(e) => {
          const c = parseColor(e.target.value);
          if (c) update({ r: c.r, g: c.g, b: c.b });
        }}
        aria-label={`${label} colour`}
      />
      <input
        type="range"
        min={0}
        max={100}
        value={Math.round(value.a * 100)}
        onChange={(e) => update({ a: +e.target.value / 100 })}
        title={`Opacity ${Math.round(value.a * 100)}%`}
        aria-label={`${label} opacity`}
      />
      <button
        className="icon-btn"
        disabled={!override}
        title="Reset to the theme colour"
        aria-label="Reset colour"
        onClick={() =>
          set((p) => {
            const next = { ...p.diffColors };
            delete next[colorKey];
            return { diffColors: next };
          })
        }
      >
        <RotateCcw size={12} />
      </button>
    </div>
  );
}

/** A few diff rows drawn with the live palette. */
function DiffPreview() {
  return (
    <div className="diff-preview">
      <div className="dp-row" style={{ background: "var(--chg-bg)" }}>
        <span>
          total = <mark style={{ background: "var(--del-mark)" }}>price</mark> * qty;
        </span>
        <span>
          total = <mark style={{ background: "var(--add-mark)" }}>unitPrice</mark> * qty;
        </span>
      </div>
      <div className="dp-row">
        <span style={{ background: "var(--del-bg)" }}>log(&quot;removed&quot;);</span>
        <span className="dp-absent" />
      </div>
      <div className="dp-row">
        <span className="dp-absent" />
        <span style={{ background: "var(--add-bg)" }}>validate(order);</span>
      </div>
      <div className="dp-row" style={{ color: "var(--unimportant)" }}>
        <span>// $Id: 41 $</span>
        <span>// $Id: 42 $</span>
      </div>
    </div>
  );
}

function AppearancePage({ prefs, set }: { prefs: Prefs; set: SetPrefs }) {
  const themes: { id: ThemePref; label: string; hint: string }[] = [
    { id: "dark", label: "Dark", hint: "" },
    { id: "light", label: "Light", hint: "" },
    { id: "system", label: "System", hint: "Follows the operating system's light or dark setting" },
  ];
  return (
    <div className="settings-grid single">
      <fieldset>
        <legend>Theme</legend>
        <div className="theme-choices">
          {themes.map((t) => (
            <label key={t.id} className={`theme-choice${prefs.theme === t.id ? " active" : ""}`} title={t.hint}>
              <input
                type="radio"
                name="theme"
                checked={prefs.theme === t.id}
                onChange={() => set({ theme: t.id })}
              />
              <span className={`theme-sample ${t.id}`} />
              {t.label}
            </label>
          ))}
        </div>
      </fieldset>
      <fieldset>
        <legend>Difference colours</legend>
        {COLOR_ROWS.map((r) => (
          <ColorRow key={r.key} label={r.label} colorKey={r.key} prefs={prefs} set={set} />
        ))}
        <DiffPreview />
        <div className="settings-actions">
          <button disabled={!Object.keys(prefs.diffColors).length} onClick={() => set({ diffColors: {} })}>
            <RotateCcw size={13} /> Reset all colours
          </button>
        </div>
      </fieldset>
    </div>
  );
}

// ---------- fonts ----------

const UI_FONTS = ["Segoe UI", "Segoe UI Variable", "Inter", "Arial", "Helvetica Neue", "Tahoma", "Verdana", "Ubuntu"];
const MONO_FONTS = [
  "Cascadia Mono",
  "Cascadia Code",
  "Consolas",
  "JetBrains Mono",
  "Fira Code",
  "Source Code Pro",
  "SF Mono",
  "Menlo",
  "Courier New",
];

function sizes({ min, max }: { min: number; max: number }) {
  const out: number[] = [];
  for (let s = min; s <= max; s += 0.5) out.push(s);
  return out;
}

function FontField({
  label,
  list,
  font,
  size,
  range,
  onFont,
  onSize,
  sample,
  mono,
}: {
  label: string;
  list: string[];
  font: string;
  size: number;
  range: { min: number; max: number };
  onFont: (f: string) => void;
  onSize: (s: number) => void;
  sample: string;
  mono?: boolean;
}) {
  const id = `fonts-${label.replace(/\W/g, "")}`;
  const [draft, setDraft] = useState(font);
  useEffect(() => setDraft(font), [font]);
  return (
    <fieldset>
      <legend>{label}</legend>
      <div className="field-row">
        <label className="field">
          <span>Font</span>
          <input
            list={id}
            value={draft}
            placeholder="Default"
            onChange={(e) => setDraft(e.target.value)}
            onBlur={() => onFont(draft)}
            onKeyDown={(e) => e.key === "Enter" && onFont(draft)}
            spellCheck={false}
          />
          <datalist id={id}>
            {list.map((f) => (
              <option key={f} value={f} />
            ))}
          </datalist>
        </label>
        <label className="field font-size">
          <span>Size</span>
          <select value={size} onChange={(e) => onSize(+e.target.value)}>
            {sizes(range).map((s) => (
              <option key={s} value={s}>
                {s}px
              </option>
            ))}
          </select>
        </label>
      </div>
      <div
        className="font-sample"
        style={{ fontFamily: mono ? "var(--mono)" : "var(--ui)", fontSize: mono ? "var(--editor-font-size)" : "var(--ui-font-size)" }}
      >
        {sample}
      </div>
    </fieldset>
  );
}

function FontsPage({ prefs, set }: { prefs: Prefs; set: SetPrefs }) {
  const changed =
    prefs.uiFont !== DEFAULT_PREFS.uiFont ||
    prefs.uiFontSize !== DEFAULT_PREFS.uiFontSize ||
    prefs.editorFont !== DEFAULT_PREFS.editorFont ||
    prefs.editorFontSize !== DEFAULT_PREFS.editorFontSize;
  return (
    <div className="settings-grid single">
      <FontField
        label="Interface"
        list={UI_FONTS}
        font={prefs.uiFont}
        size={prefs.uiFontSize}
        range={UI_FONT_SIZES}
        onFont={(uiFont) => set({ uiFont })}
        onSize={(uiFontSize) => set({ uiFontSize })}
        sample="Folder Compare · 12 differences · Synchronize folders"
      />
      <FontField
        label="Editor (text, merge, hex and table views)"
        list={MONO_FONTS}
        font={prefs.editorFont}
        size={prefs.editorFontSize}
        range={EDITOR_FONT_SIZES}
        onFont={(editorFont) => set({ editorFont })}
        onSize={(editorFontSize) => set({ editorFontSize })}
        sample={"if (left != right) { return 0x1F; } // Il1 O0"}
        mono
      />
      <div className="settings-actions">
        <button
          disabled={!changed}
          onClick={() =>
            set({
              uiFont: DEFAULT_PREFS.uiFont,
              uiFontSize: DEFAULT_PREFS.uiFontSize,
              editorFont: DEFAULT_PREFS.editorFont,
              editorFontSize: DEFAULT_PREFS.editorFontSize,
            })
          }
        >
          <RotateCcw size={13} /> Default fonts
        </button>
      </div>
    </div>
  );
}

// ---------- keyboard ----------

/** Heading for a command's scope in the shortcut list. */
const scopeLabel = (c: CommandDef) =>
  c.scope.map((s) => (s === "global" ? "Application" : SESSIONS[s].label)).join(" / ");

function KeyboardPage({ prefs, set }: { prefs: Prefs; set: SetPrefs }) {
  const [recording, setRecording] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const bindings = useMemo(() => bindingsWith(prefs.keys), [prefs.keys]);
  const conflicts = useMemo(() => findConflicts(bindings), [bindings]);

  const assign = (id: string, binding: string) =>
    set((p) => {
      const keys = { ...p.keys };
      const def = COMMANDS.find((c) => c.id === id)!;
      if (normalizeBinding(binding) === normalizeBinding(def.binding)) delete keys[id];
      else keys[id] = binding;
      return { keys };
    });

  // While recording, the next key combination becomes the shortcut. Capture
  // phase on window, so neither the app nor a session reacts to it.
  useEffect(() => {
    if (!recording) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") return; // the dialog closes; recording ends with it
      e.preventDefault();
      e.stopPropagation();
      if (e.key === "Backspace" || e.key === "Delete") {
        assign(recording, "");
        setRecording(null);
        return;
      }
      const b = eventBinding(e);
      if (!b) return; // modifiers alone: keep waiting for the key
      assign(recording, b);
      setRecording(null);
    };
    const cancel = () => setRecording(null);
    window.addEventListener("keydown", onKey, true);
    window.addEventListener("mousedown", cancel);
    return () => {
      window.removeEventListener("keydown", onKey, true);
      window.removeEventListener("mousedown", cancel);
    };
    // `assign` only depends on `set`, which is stable.
  }, [recording]);

  const q = filter.trim().toLowerCase();
  const shown = COMMANDS.filter(
    (c) =>
      !q ||
      c.label.toLowerCase().includes(q) ||
      scopeLabel(c).toLowerCase().includes(q) ||
      (bindings.get(c.id) ?? "").toLowerCase().includes(q),
  );
  const groups = new Map<string, CommandDef[]>();
  for (const c of shown) groups.set(scopeLabel(c), [...(groups.get(scopeLabel(c)) ?? []), c]);
  const label = (id: string) => {
    const c = COMMANDS.find((x) => x.id === id)!;
    return `${c.label} (${scopeLabel(c)})`;
  };

  return (
    <div className="keymap">
      <div className="keymap-top">
        <input placeholder="Filter commands or keys…" value={filter} onChange={(e) => setFilter(e.target.value)} />
        <span className="spacer" />
        {conflicts.size > 0 && <span className="keymap-warn">{conflicts.size} conflicting shortcuts</span>}
        <button disabled={!Object.keys(prefs.keys).length} onClick={() => set({ keys: {} })}>
          <RotateCcw size={13} /> Reset all
        </button>
      </div>
      <small className="muted settings-hint">
        Click a shortcut, then press the new keys. Backspace removes it; Esc closes the dialog.
      </small>
      <div className="keymap-list">
        {[...groups].map(([group, cmds]) => (
          <div key={group}>
            <div className="keymap-group">{group}</div>
            {cmds.map((c) => {
              const b = bindings.get(c.id) ?? "";
              const clash = conflicts.get(c.id);
              const custom = c.id in prefs.keys && !c.fixed;
              return (
                <div key={c.id} className={`keymap-row${clash ? " conflict" : ""}`}>
                  <span className="keymap-label">
                    {c.label}
                    {clash && <small>Also used by {clash.map(label).join(", ")}</small>}
                  </span>
                  {c.fixed ? (
                    <span className="keymap-key fixed" title="This view handles the key itself; it can't be changed yet">
                      {formatBinding(b) || "—"} <small className="badge">built-in</small>
                    </span>
                  ) : (
                    <button
                      className={`keymap-key${recording === c.id ? " recording" : ""}`}
                      onMouseDown={(e) => e.stopPropagation()}
                      onClick={() => setRecording(recording === c.id ? null : c.id)}
                    >
                      {recording === c.id ? "Press keys…" : formatBinding(b) || "None"}
                    </button>
                  )}
                  <button
                    className="icon-btn"
                    disabled={!custom}
                    title={`Reset to ${formatBinding(normalizeBinding(c.binding)) || "none"}`}
                    aria-label="Reset shortcut"
                    onClick={() => assign(c.id, c.binding)}
                  >
                    <RotateCcw size={12} />
                  </button>
                </div>
              );
            })}
          </div>
        ))}
      </div>
    </div>
  );
}

// ---------- dialog ----------

type SetPrefs = ReturnType<typeof usePrefs>[1];

/** Application-wide preferences (per-session options live in each session's own settings). */
export function AppSettingsDialog({ updater, onClose }: Props) {
  const [prefs, set] = usePrefs();
  const [page, setPage] = useState<Page>("general");

  return (
    <Modal title="Settings" onClose={onClose} wide footer={<button onClick={onClose}>Close</button>}>
      <div className="app-settings">
        <nav className="app-settings-nav">
          {PAGES.map(({ id, label, icon: Icon }) => (
            <button key={id} className={page === id ? "active" : undefined} onClick={() => setPage(id)}>
              <Icon size={14} /> {label}
            </button>
          ))}
        </nav>
        <div className="app-settings-page">
          {page === "general" && <GeneralPage updater={updater} prefs={prefs} set={set} />}
          {page === "appearance" && <AppearancePage prefs={prefs} set={set} />}
          {page === "fonts" && <FontsPage prefs={prefs} set={set} />}
          {page === "keyboard" && <KeyboardPage prefs={prefs} set={set} />}
          {page === "vcs" && <VcsSetupPage /> /* araxis:vcs */}
        </div>
      </div>
    </Modal>
  );
}
