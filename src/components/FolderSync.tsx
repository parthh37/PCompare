import type { CompareProps } from "../lib/sessionProps";
import { PathBar } from "./PathBar";

// Placeholder — replaced by the real Folder Sync implementation.
export function FolderSync({ left, right, onPathsChange }: CompareProps) {
  return (
    <div className="session">
      <PathBar left={left} right={right} directory={true} onCompare={onPathsChange} />
      <div className="session-body">
        <div className="empty-state">Folder Sync is being built.</div>
      </div>
    </div>
  );
}
