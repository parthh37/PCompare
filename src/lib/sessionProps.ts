// Prop contracts between the shell (App.tsx) and each session component.
// Settings are an opaque, JSON-serializable object that the session owns and
// the shell persists verbatim when a session is saved.

export interface CompareProps<S = Record<string, unknown>> {
  left: string;
  right: string;
  onPathsChange: (left: string, right: string) => void;
  /** Settings restored from a saved session (may be partial or stale). */
  settings?: Partial<S>;
  /** Report the current settings whenever they change. */
  onSettingsChange?: (s: S) => void;
  /** Report whether the session holds unsaved edits, so closing it can confirm. */
  onDirtyChange?: (dirty: boolean) => void;
}

export interface MergePaths {
  left: string;
  base: string;
  right: string;
  /** Where the merged result is written; may be empty until the user picks one. */
  output: string;
}

export interface MergeProps<S = Record<string, unknown>> extends MergePaths {
  onPathsChange: (paths: MergePaths) => void;
  settings?: Partial<S>;
  onSettingsChange?: (s: S) => void;
  onDirtyChange?: (dirty: boolean) => void;
}
