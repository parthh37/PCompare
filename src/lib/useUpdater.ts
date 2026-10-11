import { getVersion } from "@tauri-apps/api/app";
import { relaunch } from "@tauri-apps/plugin-process";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { useCallback, useEffect, useRef, useState } from "react";
import { errorMessage } from "./api";

/** Background re-check cadence for long-running sessions. */
const CHECK_EVERY_MS = 6 * 60 * 60 * 1000;
/** Don't compete with the first comparison for startup I/O. */
const FIRST_CHECK_DELAY_MS = 8_000;
const AUTO_CHECK_KEY = "pcompare.autoUpdate";

export type UpdatePhase =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "available"; update: Update }
  | { kind: "downloading"; update: Update; done: number; total: number | null }
  | { kind: "error"; message: string };

export function autoUpdateEnabled(): boolean {
  return localStorage.getItem(AUTO_CHECK_KEY) !== "off";
}

export function setAutoUpdateEnabled(on: boolean) {
  localStorage.setItem(AUTO_CHECK_KEY, on ? "on" : "off");
}

/**
 * Over-the-air updates from GitHub Releases (see plugins.updater in
 * tauri.conf.json). Packages are verified against the embedded public key
 * before installing, so a tampered download is rejected.
 */
export function useUpdater(notify: (msg: string) => void) {
  const [phase, setPhase] = useState<UpdatePhase>({ kind: "idle" });
  const [version, setVersion] = useState("");
  const [dismissed, setDismissed] = useState<string | null>(null);
  const busy = useRef(false);

  useEffect(() => {
    getVersion().then(setVersion).catch(() => {});
  }, []);

  const runCheck = useCallback(
    async (manual: boolean) => {
      if (busy.current) return;
      busy.current = true;
      if (manual) setPhase({ kind: "checking" });
      try {
        const update = await check();
        if (update) {
          setPhase({ kind: "available", update });
          if (manual) setDismissed(null);
        } else {
          setPhase({ kind: "idle" });
          if (manual) notify(`You're on the latest version${version ? ` (${version})` : ""}.`);
        }
      } catch (e) {
        // Background failures (offline, no release yet) stay quiet.
        setPhase(manual ? { kind: "error", message: errorMessage(e) } : { kind: "idle" });
        if (manual) notify(`Update check failed: ${errorMessage(e)}`);
      } finally {
        busy.current = false;
      }
    },
    [notify, version],
  );

  useEffect(() => {
    // Dev builds have no release to compare against.
    if (!import.meta.env.PROD || !autoUpdateEnabled()) return;
    const first = window.setTimeout(() => runCheck(false), FIRST_CHECK_DELAY_MS);
    const every = window.setInterval(() => runCheck(false), CHECK_EVERY_MS);
    return () => {
      window.clearTimeout(first);
      window.clearInterval(every);
    };
  }, [runCheck]);

  const install = useCallback(async () => {
    if (phase.kind !== "available") return;
    const { update } = phase;
    let done = 0;
    let total: number | null = null;
    setPhase({ kind: "downloading", update, done, total });
    try {
      await update.downloadAndInstall((event) => {
        if (event.event === "Started") total = event.data.contentLength ?? null;
        else if (event.event === "Progress") done += event.data.chunkLength;
        setPhase({ kind: "downloading", update, done, total });
      });
      // On Windows the installer exits the app itself; elsewhere restart into the new version.
      await relaunch();
    } catch (e) {
      setPhase({ kind: "available", update });
      notify(`Update failed: ${errorMessage(e)}`);
    }
  }, [phase, notify]);

  const visible =
    (phase.kind === "available" && dismissed !== phase.update.version) || phase.kind === "downloading";

  return {
    phase,
    version,
    bannerVisible: visible,
    checkNow: () => runCheck(true),
    install,
    dismiss: () => phase.kind === "available" && setDismissed(phase.update.version),
  };
}
