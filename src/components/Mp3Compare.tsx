import type { CompareProps } from "../lib/sessionProps";
import { PathBar } from "./PathBar";

// Placeholder — replaced by the real MP3 Compare implementation.
export function Mp3Compare({ left, right, onPathsChange }: CompareProps) {
  return (
    <div className="session">
      <PathBar left={left} right={right} directory={false} onCompare={onPathsChange} />
      <div className="session-body">
        <div className="empty-state">MP3 Compare is being built.</div>
      </div>
    </div>
  );
}
