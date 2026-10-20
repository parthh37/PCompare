// The backup choices (keep them or not, for how long, how much, where),
// shared by the one-time question, the tab bar's Backups panel and
// Settings → Backups.
import { open } from "@tauri-apps/plugin-dialog";
import { useEffect, useState } from "react";
import { backupsApi, keepLabel, KEEP_OPTIONS, SIZE_OPTIONS, type BackupsInfo } from "../lib/backupsApi";
import type { BackupPrefs } from "../lib/prefs";
import "./Backups.css";

interface Props {
  value: BackupPrefs;
  onChange: (patch: Partial<BackupPrefs>) => void;
  /** Shows the on/off checkbox (the one-time question asks with its buttons instead). */
  toggle?: boolean;
  /** Offers the size limit too (Settings; the quick panel stays short). */
  sizeLimit?: boolean;
}

export function BackupSettingsFields({ value, onChange, toggle, sizeLimit }: Props) {
  const [info, setInfo] = useState<BackupsInfo | null>(null);
  // The custom folder as typed; saved on blur / Enter so half a path never becomes the setting.
  const [draft, setDraft] = useState(value.folder);
  // Remembers the custom folder while "Default folder" is picked, so switching back is one click.
  const [lastCustom, setLastCustom] = useState(value.folder);
  useEffect(() => {
    setDraft(value.folder);
    if (value.folder) setLastCustom(value.folder);
  }, [value.folder]);
  useEffect(() => {
    let live = true;
    backupsApi
      .info()
      .then((i) => live && setInfo(i))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, []);

  const custom = value.folder !== "";
  const setFolder = (folder: string) => onChange({ folder: folder.trim() });
  const browse = async () => {
    const picked = await open({
      directory: true,
      multiple: false,
      title: "Folder for backups",
      defaultPath: value.folder || lastCustom || info?.defaultFolder,
    });
    if (typeof picked === "string" && picked) setFolder(picked);
  };

  return (
    <div className="bkset">
      {toggle && (
        <label className="field check">
          <input
            type="checkbox"
            checked={value.enabled}
            onChange={(e) => onChange({ enabled: e.target.checked, asked: true })}
          />
          Keep a backup of every file PCompare replaces or removes
        </label>
      )}
      <fieldset className="bkset-body" disabled={toggle && !value.enabled}>
        <div className="field-row">
          <label className="field">
            <span>Keep backups for</span>
            <select value={value.keepDays} onChange={(e) => onChange({ keepDays: Number(e.target.value) })}>
              {KEEP_OPTIONS.map((o) => (
                <option key={o.days} value={o.days}>
                  {o.label}
                </option>
              ))}
              {!KEEP_OPTIONS.some((o) => o.days === value.keepDays) && (
                <option value={value.keepDays}>{keepLabel(value.keepDays)}</option>
              )}
            </select>
          </label>
          {sizeLimit && (
            <label className="field">
              <span>Size limit</span>
              <select value={value.maxSizeMb} onChange={(e) => onChange({ maxSizeMb: Number(e.target.value) })}>
                {SIZE_OPTIONS.map((o) => (
                  <option key={o.mb} value={o.mb}>
                    {o.label}
                  </option>
                ))}
                {!SIZE_OPTIONS.some((o) => o.mb === value.maxSizeMb) && (
                  <option value={value.maxSizeMb}>{value.maxSizeMb} MB</option>
                )}
              </select>
            </label>
          )}
        </div>
        <small className="muted bkset-hint">
          {value.keepDays > 0
            ? `Older backups are removed automatically${sizeLimit && value.maxSizeMb > 0 ? ", and the oldest go first when the folder passes the size limit" : ""}.`
            : sizeLimit && value.maxSizeMb > 0
              ? "Backups stay until the folder passes the size limit; then the oldest go first."
              : "Backups stay until you delete them on the Backups tab."}
        </small>
        <div className="field bkset-where">
          <span>Save backups in</span>
          <label className="field radio">
            <input type="radio" checked={!custom} onChange={() => setFolder("")} />
            <span>
              The default folder
              <small className="muted bkset-path" title={info?.defaultFolder}>
                {info?.defaultFolder ?? "…"}
              </small>
            </span>
          </label>
          <label className="field radio">
            <input
              type="radio"
              checked={custom}
              onChange={() => (lastCustom ? setFolder(lastCustom) : void browse())}
            />
            <span>A folder I choose</span>
          </label>
          {custom && (
            <div className="input-row bkset-folder">
              <input
                value={draft}
                spellCheck={false}
                aria-label="Backup folder"
                onChange={(e) => setDraft(e.target.value)}
                onBlur={() => draft.trim() !== value.folder && setFolder(draft)}
                onKeyDown={(e) => e.key === "Enter" && setFolder(draft)}
              />
              <button onClick={() => void browse()}>Browse…</button>
            </div>
          )}
          {info && info.folder !== (value.folder || info.defaultFolder) && (
            <small className="muted bkset-hint">Backups made before this change stay in {info.folder}.</small>
          )}
        </div>
      </fieldset>
    </div>
  );
}
