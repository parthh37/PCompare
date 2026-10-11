import type { CompareProps } from "../lib/sessionProps";
import { PathBar } from "./PathBar";

// Placeholder — replaced by the real Picture Compare implementation.
export function PictureCompare({ left, right, onPathsChange }: CompareProps) {
  return (
    <div className="session">
      <PathBar left={left} right={right} directory={false} onCompare={onPathsChange} />
      <div className="session-body">
        <div className="empty-state">Picture Compare is being built.</div>
      </div>
    </div>
  );
}
