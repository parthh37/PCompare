import { CaseSensitive, ChevronDown, ChevronUp, Regex, Replace, ReplaceAll, Search, WholeWord, X } from "lucide-react";
import { useEffect, useRef } from "react";
import type { FindOptions } from "../lib/find";
import "./FindBar.css";

export type FindScope = "left" | "right" | "both";

interface Props {
  query: string;
  onQuery: (q: string) => void;
  replacement: string;
  onReplacement: (r: string) => void;
  options: FindOptions;
  onOptions: (o: FindOptions) => void;
  /** bc5:editor-patch — optional: one-pane views (Text Edit) have no sides to pick. */
  scope?: FindScope;
  onScope?: (s: FindScope) => void;
  /** Show the replace row (Ctrl+H). */
  replaceMode: boolean;
  onReplaceMode: (on: boolean) => void;
  count: number;
  /** 0-based index of the current match, or -1. */
  current: number;
  /** Invalid regex message. */
  error: string | null;
  /** Why replacing is unavailable (read-only sides, diff catching up), or null. */
  replaceBlocked: string | null;
  /** Bumped whenever Ctrl+F/Ctrl+H is pressed so the query gets focus again. */
  focusSignal: number;
  onNext: () => void;
  onPrev: () => void;
  onReplace: () => void;
  onReplaceAll: () => void;
  onClose: () => void;
}

/** The Find / Replace strip shown above the Text Compare panes (Ctrl+F, Ctrl+H). */
export function FindBar(p: Props) {
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    input.current?.focus();
    input.current?.select();
  }, [p.focusSignal]);

  const toggle = (key: keyof FindOptions, label: string, Icon: typeof Regex) => (
    <button
      className={`icon-btn fb-toggle${p.options[key] ? " active" : ""}`}
      title={label}
      aria-pressed={p.options[key]}
      onClick={() => p.onOptions({ ...p.options, [key]: !p.options[key] })}
    >
      <Icon size={15} />
    </button>
  );

  const status = p.error
    ? "Invalid regex"
    : !p.query
      ? ""
      : p.count === 0
        ? "No results"
        : `${p.current >= 0 ? p.current + 1 : "?"} of ${p.count.toLocaleString()}`;

  return (
    <div className="findbar">
      <div className="fb-row">
        <Search size={14} className="fb-icon" />
        <input
          ref={input}
          className={`fb-input${p.error || (p.query && !p.count) ? " fb-miss" : ""}`}
          value={p.query}
          spellCheck={false}
          placeholder="Find"
          title={p.error ?? undefined}
          onChange={(e) => p.onQuery(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") (e.shiftKey ? p.onPrev : p.onNext)();
            else if (e.key === "Escape") p.onClose();
            else return;
            e.preventDefault();
          }}
        />
        {toggle("matchCase", "Match case", CaseSensitive)}
        {toggle("wholeWord", "Whole word", WholeWord)}
        {toggle("regex", "Regular expression", Regex)}
        {p.onScope && ( // bc5:editor-patch
          <div className="seg-control fb-scope" title="Which side to search">
            {(["left", "both", "right"] as FindScope[]).map((s) => (
              <button key={s} className={p.scope === s ? "active" : ""} onClick={() => p.onScope?.(s)}>
                {s === "left" ? "Left" : s === "right" ? "Right" : "Both"}
              </button>
            ))}
          </div>
        )}
        <span className={`fb-status${p.error ? " error" : ""}`}>{status}</span>
        <button className="icon-btn" onClick={p.onPrev} disabled={!p.count} title="Previous match (Shift+F3)">
          <ChevronUp size={15} />
        </button>
        <button className="icon-btn" onClick={p.onNext} disabled={!p.count} title="Next match (F3)">
          <ChevronDown size={15} />
        </button>
        <button
          className={`icon-btn${p.replaceMode ? " active" : ""}`}
          onClick={() => p.onReplaceMode(!p.replaceMode)}
          title="Toggle replace (Ctrl+H)"
        >
          <Replace size={15} />
        </button>
        <button className="icon-btn" onClick={p.onClose} title="Close (Esc)">
          <X size={15} />
        </button>
      </div>
      {p.replaceMode && (
        <div className="fb-row">
          <Replace size={14} className="fb-icon" />
          <input
            className="fb-input"
            value={p.replacement}
            spellCheck={false}
            placeholder={p.options.regex ? "Replace with ($1, $& …)" : "Replace with"}
            onChange={(e) => p.onReplacement(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") p.onReplace();
              else if (e.key === "Escape") p.onClose();
              else return;
              e.preventDefault();
            }}
          />
          <button onClick={p.onReplace} disabled={!p.count || !!p.replaceBlocked} title={p.replaceBlocked ?? "Replace the current match"}>
            <Replace size={14} /> Replace
          </button>
          <button
            onClick={p.onReplaceAll}
            disabled={!p.count || !!p.replaceBlocked}
            title={p.replaceBlocked ?? "Replace every match in the searched, editable sides (one undo step)"}
          >
            <ReplaceAll size={14} /> Replace all
          </button>
        </div>
      )}
    </div>
  );
}
