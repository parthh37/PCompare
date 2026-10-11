import type { CompareProps } from "../lib/sessionProps";
import { PathBar } from "./PathBar";

// Placeholder — replaced by the real Version Compare implementation.
export function VersionCompare({ left, right, onPathsChange }: CompareProps) {
  return (
    <div className="session">
      <PathBar left={left} right={right} directory={false} onCompare={onPathsChange} />
      <div className="session-body">
        <div className="empty-state">Version Compare is being built.</div>
      </div>
    </div>
  );
}
