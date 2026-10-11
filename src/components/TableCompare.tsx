import type { CompareProps } from "../lib/sessionProps";
import { PathBar } from "./PathBar";

// Placeholder — replaced by the real Table Compare implementation.
export function TableCompare({ left, right, onPathsChange }: CompareProps) {
  return (
    <div className="session">
      <PathBar left={left} right={right} directory={false} onCompare={onPathsChange} />
      <div className="session-body">
        <div className="empty-state">Table Compare is being built.</div>
      </div>
    </div>
  );
}
