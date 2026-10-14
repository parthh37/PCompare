// Command-line launch options (mirrors `cli::LaunchOptions` in
// src-tauri/src/cli.rs): what `pcompare [switches] <paths…>` asks the window
// to open, at startup (`launch_args`) or from a second launch.
import type { SessionKind } from "./sessions";

export interface LaunchOptions {
  /** `left right [base [output]]`. */
  paths: string[];
  /** Forced session kind (`/fv=Hex Compare`, `--viewer hex`). */
  viewer: SessionKind | null;
  /** Tab labels (`/title1=`, `/title2=`). */
  titleLeft: string | null;
  titleRight: string | null;
  /** Merge base / output labels (`/title3=`, `/title4=`). */
  titleBase?: string | null;
  titleOutput?: string | null;
  /** `/filters=`, `/expandall`, `/savetarget=`. */
  filters?: string | null;
  /** `/filters=` split into Folder Compare's glob lists (cli.rs `NameFilter`). */
  nameFilter?: NameFilter | null;
  expandAll?: boolean;
  saveTarget?: string | null;
  /** `/readonly`, `/lro`, `/rro`. */
  readOnlyLeft: boolean;
  readOnlyRight: boolean;
  /** `/mergeoutput=` (already folded into `paths[3]` for 3-way merges). */
  mergeOutput: string | null;
  /** A single argument naming a saved session / workspace (BC "Named Session"). */
  sessionId?: string | null;
  workspaceId?: string | null;
  /** araxis:vcs — a `--wait` launch: report back under this token when the tab closes. */
  waitToken?: string | null;
}

export interface NameFilter {
  includeFiles: string[];
  excludeFiles: string[];
  excludeFolders: string[];
}

/**
 * Folder Compare settings for `/filters=` (its fields are comma-separated
 * globs). Every list is set, so an empty one clears that field's default.
 */
export function folderFilterSettings(f: NameFilter): Record<string, string> {
  const join = (globs: string[]) => globs.join(", ");
  return {
    includeFiles: join(f.includeFiles),
    excludeFiles: join(f.excludeFiles),
    excludeFolders: join(f.excludeFolders),
  };
}

/** Accepts the old payload shape (a bare path list) too. */
export function toLaunchOptions(raw: LaunchOptions | string[] | null | undefined): LaunchOptions {
  if (Array.isArray(raw) || !raw) {
    return {
      paths: raw ?? [],
      viewer: null,
      titleLeft: null,
      titleRight: null,
      readOnlyLeft: false,
      readOnlyRight: false,
      mergeOutput: null,
    };
  }
  return raw;
}
