import { createContext, useContext } from "react";

/** App-level actions that deeply nested components can trigger. */
export interface Shell {
  openProfiles: () => void;
  /** Path picked with "Select Left Side for Compare" (here or in Explorer). */
  leftSelection: string | null;
  selectLeft: (path: string | null) => void;
  /** Compares `path` against the selected left side, then clears the selection. */
  compareToLeft: (path: string) => void;
}

export const ShellContext = createContext<Shell>({
  openProfiles: () => {},
  leftSelection: null,
  selectLeft: () => {},
  compareToLeft: () => {},
});
export const useShell = () => useContext(ShellContext);
