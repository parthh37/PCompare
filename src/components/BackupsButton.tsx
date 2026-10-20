// The tab bar's Backups switch: shows at a glance whether replaced files are
// being kept, and opens a small panel to turn that on or off, pick how long
// they stay and where, or go to the Backups tab.
import { ShieldCheck, ShieldOff } from "lucide-react";
import { useEffect, useRef, useState, type CSSProperties } from "react";
import { keepLabel, setBackupPrefs } from "../lib/backupsApi";
import { usePrefs } from "../lib/prefs";
import { BackupSettingsFields } from "./BackupSettingsFields";

interface Props {
  onOpenBackups: () => void;
  onOpenSettings: () => void;
}

export function BackupsButton({ onOpenBackups, onOpenSettings }: Props) {
  const [prefs] = usePrefs();
  const b = prefs.backups;
  const button = useRef<HTMLButtonElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  // Fixed under the button: the tab strip scrolls sideways and would clip it.
  const [at, setAt] = useState<CSSProperties | null>(null);

  const toggle = () => {
    const r = button.current?.getBoundingClientRect();
    setAt((cur) => (cur || !r ? null : { top: r.bottom + 4, right: Math.max(8, window.innerWidth - r.right) }));
  };

  useEffect(() => {
    if (!at) return;
    const away = (e: MouseEvent) => {
      const t = e.target as Node;
      if (!panel.current?.contains(t) && !button.current?.contains(t)) setAt(null);
    };
    const key = (e: KeyboardEvent) => {
      if (e.key === "Escape") setAt(null);
    };
    const close = () => setAt(null);
    window.addEventListener("mousedown", away, true);
    window.addEventListener("keydown", key, true);
    window.addEventListener("resize", close);
    return () => {
      window.removeEventListener("mousedown", away, true);
      window.removeEventListener("keydown", key, true);
      window.removeEventListener("resize", close);
    };
  }, [at]);

  const go = (action: () => void) => () => {
    setAt(null);
    action();
  };

  return (
    <>
      <button
        ref={button}
        className={`tab-action bkset-btn${b.enabled ? " on" : ""}`}
        title={
          b.enabled
            ? `Backups are on: files PCompare replaces are kept for ${keepLabel(b.keepDays).toLowerCase()}`
            : "Backups are off: replaced files aren't kept"
        }
        aria-expanded={!!at}
        aria-haspopup="dialog"
        onClick={toggle}
      >
        {b.enabled ? <ShieldCheck size={14} /> : <ShieldOff size={14} />} Backups
      </button>
      {at && (
        <div ref={panel} className="bkset-panel" style={at} role="dialog" aria-label="Backups">
          <BackupSettingsFields toggle value={b} onChange={setBackupPrefs} />
          <div className="bkset-panel-foot">
            <button onClick={go(onOpenSettings)}>More settings…</button>
            <button className="primary" onClick={go(onOpenBackups)}>
              Open Backups
            </button>
          </div>
        </div>
      )}
    </>
  );
}
