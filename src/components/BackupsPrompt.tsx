// The one-time question, asked when the first comparison opens: should
// PCompare keep a backup of every file it replaces? Either answer is final
// (`backups.asked`); the tab bar's Backups button and Settings change it later.
import { ArchiveRestore } from "lucide-react";
import { useState } from "react";
import { setBackupPrefs } from "../lib/backupsApi";
import { getPrefs, type BackupPrefs } from "../lib/prefs";
import { BackupSettingsFields } from "./BackupSettingsFields";
import { Modal } from "./Modal";

export function BackupsPrompt({ onDone }: { onDone: () => void }) {
  const [draft, setDraft] = useState<BackupPrefs>(() => getPrefs().backups);
  const answer = (enabled: boolean) => {
    // "Not now" keeps the other choices untouched for when backups are turned on later.
    setBackupPrefs(enabled ? { ...draft, enabled, asked: true } : { asked: true });
    onDone();
  };
  return (
    <Modal
      title="Keep backups of your files?"
      onClose={() => answer(false)}
      footer={
        <>
          <button onClick={() => answer(false)}>Not now</button>
          <button className="primary" autoFocus onClick={() => answer(true)}>
            Keep backups
          </button>
        </>
      }
    >
      <div className="bkset-prompt">
        <ArchiveRestore size={28} className="bkset-prompt-icon" />
        <p>
          When you copy files left → right or right → left, save an edited side, or run a sync or merge, PCompare
          can first keep the version it replaces. Any of them can be put back from the <b>Backups</b> tab.
        </p>
      </div>
      <BackupSettingsFields value={draft} onChange={(patch) => setDraft((d) => ({ ...d, ...patch }))} />
      <small className="muted">
        You can change this any time with the Backups button in the tab bar, or in Settings → Backups.
      </small>
    </Modal>
  );
}
