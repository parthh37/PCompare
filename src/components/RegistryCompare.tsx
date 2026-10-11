import type { CompareProps } from "../lib/sessionProps";
import { PathBar } from "./PathBar";

// Placeholder — replaced by the real Registry Compare implementation.
export function RegistryCompare({ left, right, onPathsChange }: CompareProps) {
  return (
    <div className="session">
      <PathBar left={left} right={right} directory={false} onCompare={onPathsChange} />
      <div className="session-body">
        <div className="empty-state">Registry Compare is being built.</div>
      </div>
    </div>
  );
}
