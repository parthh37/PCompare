import { createContext, useContext } from "react";

/** App-level actions that deeply nested components can trigger. */
export interface Shell {
  openProfiles: () => void;
}

export const ShellContext = createContext<Shell>({ openProfiles: () => {} });
export const useShell = () => useContext(ShellContext);
