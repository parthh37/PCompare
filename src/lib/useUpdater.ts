import { getVersion } from "@tauri-apps/api/app";
import { getCurrentWindow, UserAttentionType } from "@tauri-apps/api/window";
import { relaunch } from "@tauri-apps/plugin-process";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { useCallback, useEffect, useRef, useState } from "react";
import { errorMessage } from "./api";
import { setPrefs, usePref } from "./prefs";

/**
 * Background re-check cadence. Each check that still finds a newer version
 * pops the update dialog up again (unless reminders are off), so "Later"
 * means "remind me in an hour".
 */
export const CHECK_EVERY_MS = 60 * 60 * 1000;
/** Don't compete with the first comparison for startup I/O. */
const FIRST_CHECK_DELAY_MS = 8_000;

export type UpdatePhase =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "available"; update: Update }
  | { kind: "downloading"; update: Update; done: number; total: number | null }
  | { kind: "error"; message: string };

/** Flashes the taskbar button / bounces the Dock icon when PCompare isn't the active app. */
async function pingUser() {
  try {
    const win = getCurrentWindow();
    if (!(await win.isFocused())) await win.requestUserAttention(UserAttentionType.Informational);
  } catch {
    // Attention requests are a nicety; the dialog still shows.
  }
}

/**
 * Over-the-air updates from GitHub Releases (see plugins.updater in
 * tauri.conf.json). Packages are verified against the embedded public key
 * before installing, so a tampered download is rejected.
 */
export function useUpdater(notify: (msg: string) => void) {
  const [phase, setPhase] = useState<UpdatePhase>({ kind: "idle" });
  const [version, setVersion] = useState("");
  /** The update dialog is up (opened by a check that found a new version). */
  const [promptOpen, setPromptOpen] = useState(false);
  /** App preference (migrated from the old `pcompare.autoUpdate` localStorage key by prefs.ts). */
  const autoCheck = usePref("autoUpdate");
  const reminders = usePref("updateReminders");
  /** When the last check reached the server, successfully or not. */
  const [checkedAt, setCheckedAt] = useState<Date | null>(null);
  const busy = useRef(false);
  const remindersRef = useRef(reminders);
  remindersRef.current = reminders;

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
          // Keep an install that is already downloading going.
          setPhase((p) => (p.kind === "downloading" ? p : { kind: "available", update }));
          // A manual check always shows the dialog; background ones only while reminders are on.
          if (manual || remindersRef.current) {
            setPromptOpen(true);
            if (!manual) void pingUser();
          }
        } else {
          setPhase({ kind: "idle" });
          setPromptOpen(false);
          if (manual) notify(`You're on the latest version${version ? ` (${version})` : ""}.`);
        }
      } catch (e) {
        if (manual) setCheckedAt(new Date());
        // Background failures (offline, no release yet) stay quiet.
        setPhase((p) =>
          manual ? { kind: "error", message: errorMessage(e) } : p.kind === "checking" ? { kind: "idle" } : p,
        );
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
  const setReminders = useCallback((on: boolean) => setPrefs({ updateReminders: on }), []);

  const install = useCallback(async () => {
    if (phase.kind !== "available") return;
    const { update } = phase;
    let done = 0;
    let total: number | null = null;
    setPromptOpen(true);
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

  /** "Later": hide the dialog until the next hourly check. */
  const remindLater = useCallback(() => setPromptOpen(false), []);

  /** "Don't remind me": no more pop-ups; Settings → Updates and the tab-bar badge still offer the install. */
  const disableReminders = useCallback(() => {
    setPrefs({ updateReminders: false });
    setPromptOpen(false);
    notify("Update reminders are off. Turn them back on in Settings → Updates.");
  }, [notify]);

  return {
    phase,
    version,
    /** The update dialog should be on screen. */
    promptVisible: (promptOpen && phase.kind === "available") || phase.kind === "downloading",
    /** A newer version is known but its dialog is closed (shown as a tab-bar badge). */
    pending: phase.kind === "available" && !promptOpen,
    autoCheck,
    setAutoCheck,
    reminders,
    setReminders,
    checkedAt,
    checkNow: () => runCheck(true),
    install,
    remindLater,
    disableReminders,
    /** Reopens the dialog (the tab-bar badge). */
    showPrompt: () => setPromptOpen(true),
  };
}

export type Updater = ReturnType<typeof useUpdater>;
