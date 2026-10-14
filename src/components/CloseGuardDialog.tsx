import { useState } from "react";
import { Modal } from "./Modal";

interface Props {
  quit: boolean;
  /** Title of each tab with unsaved edits, and whether it can be saved from here. */
  tabs: { id: number; title: string; savable: boolean }[];
  onSaveAll: () => Promise<void>;
  onDiscard: () => void;
  onCancel: () => void;
}

/** Asked when closing the window or quitting would lose unsaved edits. */
export function CloseGuardDialog({ quit, tabs, onSaveAll, onDiscard, onCancel }: Props) {
  const [busy, setBusy] = useState(false);
  const what = quit ? "quitting" : "closing";
  const saveAll = () => {
    setBusy(true);
    onSaveAll().finally(() => setBusy(false));
  };
  return (
    <Modal
      title={quit ? "Quit PCompare" : "Close PCompare"}
      onClose={() => !busy && onCancel()}
      footer={
        <>
          <button className="primary" onClick={saveAll} disabled={busy} autoFocus>
            Save all
          </button>
          <button onClick={onDiscard} disabled={busy}>
            Discard
          </button>
          <button onClick={onCancel} disabled={busy}>
            Cancel
          </button>
        </>
      }
    >
      <p>
        {tabs.length === 1 ? "A tab has" : `${tabs.length} tabs have`} unsaved changes. Save them before {what}?
      </p>
      <ul className="close-guard-tabs">
        {tabs.map((t) => (
          <li key={t.id}>
            {t.title}
            {!t.savable && <span className="muted"> (save it in its tab)</span>}
          </li>
        ))}
      </ul>
    </Modal>
  );
}
