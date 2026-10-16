// What's on disk under a half-typed path, for the dropdown under every path
// field (Rust `complete_path`). Only absolute local paths are listed; the
// field offers its recent-path history for anything else.
import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";

export interface CompletionEntry {
  name: string;
  /** `dir` + `name`: what the field holds once this entry is picked. */
  path: string;
  isDir: boolean;
}

export interface PathCompletion {
  /** The folder listed, as typed and ending with its separator (`C:\Users\`). */
  dir: string;
  /** Folders first, then by name. */
  entries: CompletionEntry[];
  /** Matches left out past the limit. */
  more: number;
}

const LIMIT = 200;

/** A drive, UNC or `/` path: worth asking the disk (Rust's `split_typed` has the last word). */
const looksLocal = (text: string) => /^\s*"?([a-z]:|[\\/])/i.test(text);

/** The folder part of typed text, up to its last separator. */
const dirOf = (text: string) => text.replace(/[^\\/]*$/, "");

/**
 * The disk's entries under `text`, kept current as it's typed. `null` when
 * there's nothing to list (not a local path, a folder that can't be read);
 * `undefined` while a folder's first listing is on its way. `folders` asks
 * for subfolders plus the archives and snapshots that open as folders.
 */
export function usePathCompletion(text: string, folders: boolean): PathCompletion | null | undefined {
  const local = looksLocal(text);
  const [got, setGot] = useState<{ text: string; folders: boolean; result: PathCompletion | null }>();
  // A new folder is listed at once; more letters in the same one wait for a
  // pause, narrowing the listing already here in the meantime.
  const sameDir = got !== undefined && dirOf(got.text) === dirOf(text);
  const fresh = got?.text === text && got.folders === folders;
  useEffect(() => {
    if (!local || fresh) return;
    let live = true;
    const timer = setTimeout(
      () =>
        invoke<PathCompletion | null>("complete_path", { text, folders, limit: LIMIT })
          .catch(() => null)
          .then((result) => void (live && setGot({ text, folders, result }))),
      sameDir ? 80 : 0,
    );
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [text, folders, local, fresh, sameDir]);
  if (!local) return null;
  if (!got || got.folders !== folders) return undefined;
  return fresh ? got.result : narrow(got.result, text);
}

/** An older listing of the same folder, filtered to what's typed now. */
function narrow(c: PathCompletion | null, text: string): PathCompletion | undefined {
  const partial = c && text.startsWith(c.dir) ? text.slice(c.dir.length) : null;
  if (!c || partial === null || /[\\/]/.test(partial)) return undefined;
  const want = partial.toLowerCase();
  return { ...c, entries: c.entries.filter((e) => e.name.toLowerCase().startsWith(want)) };
}
