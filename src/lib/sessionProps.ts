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
  /** bc5:reports-script — sides opened read-only from the command line (`/ro`, `/lro`, `/rro`). */
  readOnlySides?: ReadOnlySides;
}

export interface ReadOnlySides {
  left: boolean;
  right: boolean;
}

/** Shown where an edit is refused because of `/ro`, `/lro` or `/rro`. */
export const LAUNCH_READ_ONLY_HINT = "Opened read-only from the command line (/ro, /lro, /rro)";

/** Command-line labels for the panes (`/title1=` … `/title4=`), shown instead of the paths. */
export interface PaneTitles {
  left?: string;
  right?: string;
  base?: string;
  output?: string;
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
  /** bc5:reports-script — `/ro`, `/lro`, `/rro`: the merge never writes over a read-only side. */
  readOnlySides?: ReadOnlySides;
  /** bc5:reports-script — `/title1=` … `/title4=`. */
  titles?: PaneTitles;
}
