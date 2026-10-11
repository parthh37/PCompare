import { useEffect, useRef, useState } from "react";
import { api, isRemote } from "./api";

/** Remote stores can't push change notifications, so they are polled. */
export const REMOTE_POLL_MS = 30_000;

export type AutoRefreshState = "off" | "watching" | "polling" | "unavailable";

interface Options {
  enabled: boolean;
  paths: string[];
  /** Globs for paths whose churn should not trigger a refresh (e.g. `.git`). */
  exclude?: string[];
  onChange: () => void;
}

/**
 * Calls `onChange` whenever either side changes on disk: local paths through
 * the OS file watcher (debounced in Rust), remote paths by polling.
 */
export function useAutoRefresh({ enabled, paths, exclude = [], onChange }: Options): AutoRefreshState {
  const callback = useRef(onChange);
  callback.current = onChange;
  const [state, setState] = useState<AutoRefreshState>("off");
  const pathKey = paths.join("\0");
  const excludeKey = exclude.join("\0");

  useEffect(() => {
    if (!enabled || paths.some((p) => !p)) {
      setState("off");
      return;
    }
    const id = crypto.randomUUID();
    let disposed = false;
    let unlisten: (() => void) | undefined;
    const remote = paths.some(isRemote);
    const timer = remote ? window.setInterval(() => callback.current(), REMOTE_POLL_MS) : undefined;

    api
      .onFsChanged((changed) => changed === id && callback.current())
      .then((u) => (disposed ? u() : (unlisten = u)));
    api
      .watchPaths(id, paths, exclude)
      .then((watched) => {
        if (disposed) return;
        setState(watched > 0 ? "watching" : remote ? "polling" : "unavailable");
      })
      .catch(() => !disposed && setState(remote ? "polling" : "unavailable"));

    return () => {
      disposed = true;
      unlisten?.();
      window.clearInterval(timer);
      api.unwatchPaths(id).catch(() => {});
    };
    // pathKey/excludeKey stand in for the arrays so identity churn doesn't re-watch.
  }, [enabled, pathKey, excludeKey]);

  return state;
}

export function autoRefreshTitle(state: AutoRefreshState): string {
  switch (state) {
    case "watching":
      return "Re-compares automatically when files change on disk";
    case "polling":
      return `Remote side is re-checked every ${REMOTE_POLL_MS / 1000}s`;
    case "unavailable":
      return "These paths can't be watched";
    case "off":
      return "Re-compare automatically when files change";
  }
}
