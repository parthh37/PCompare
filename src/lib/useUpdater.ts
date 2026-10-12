import { getVersion } from "@tauri-apps/api/app";
import { relaunch } from "@tauri-apps/plugin-process";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { useCallback, useEffect, useRef, useState } from "react";
import { errorMessage } from "./api";
import { setPrefs, usePref } from "./prefs";

/** Background re-check cadence for long-running sessions. */
const CHECK_EVERY_MS = 6 * 60 * 60 * 1000;
/** Don't compete with the first comparison for startup I/O. */
const FIRST_CHECK_DELAY_MS = 8_000;

export type UpdatePhase =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "available"; update: Update }
  | { kind: "downloading"; update: Update; done: number; total: number | null }
  | { kind: "error"; message: string };

/**
 * Over-the-air updates from GitHub Releases (see plugins.updater in
 * tauri.conf.json). Packages are verified against the embedded public key
 * before installing, so a tampered download is rejected.
 */
export function useUpdater(notify: (msg: string) => void) {
  const [phase, setPhase] = useState<UpdatePhase>({ kind: "idle" });
  const [version, setVersion] = useState("");
  const [dismissed, setDismissed] = useState<string | null>(null);
  /** App preference (migrated from the old `pcompare.autoUpdate` localStorage key by prefs.ts). */
  const autoCheck = usePref("autoUpdate");
  /** When the last check reached the server, successfully or not. */
  const [checkedAt, setCheckedAt] = useState<Date | null>(null);
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
        setCheckedAt(new Date());
        if (update) {
          setPhase({ kind: "available", update });
          if (manual) setDismissed(null);
        } else {
          setPhase({ kind: "idle" });
          if (manual) notify(`You're on the latest version${version ? ` (${version})` : ""}.`);
        }
      } catch (e) {
        if (manual) setCheckedAt(new Date());
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
    if (!import.meta.env.PROD || !autoCheck) return;
    const first = window.setTimeout(() => runCheck(false), FIRST_CHECK_DELAY_MS);
    const every = window.setInterval(() => runCheck(false), CHECK_EVERY_MS);
    return () => {
      window.clearTimeout(first);
      window.clearInterval(every);
    };
  }, [runCheck, autoCheck]);

  const setAutoCheck = useCallback((on: boolean) => setPrefs({ autoUpdate: on }), []);

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
    autoCheck,
    setAutoCheck,
    checkedAt,
    checkNow: () => runCheck(true),
    install,
    dismiss: () => phase.kind === "available" && setDismissed(phase.update.version),
  };
}

export type Updater = ReturnType<typeof useUpdater>;
