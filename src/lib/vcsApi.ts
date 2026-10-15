// source-control integration (mirrors src-tauri/src/vcs.rs and
// pcompare-archive's vcs module): git:/hg: sides, `--wait` tabs, git setup.
import { invoke } from "@tauri-apps/api/core";

export type VcsKind = "git" | "hg";

export interface Revision {
  id: string;
  short: string;
  summary: string;
  author: string;
  timeMs: number;
  /** Branches, tags and bookmarks pointing at it. */
  refs: string[];
}

export interface RepoInfo {
  kind: VcsKind | null;
  gitAvailable: boolean;
  hgAvailable: boolean;
}

/** A commit some lines were last changed in (`git blame`). */
export interface BlameCommit {
  id: string;
  short: string;
  author: string;
  timeMs: number;
  summary: string;
  /** The file's repository-relative path in this commit (differs after a rename). */
  path: string;
  /** The parent and the file's path there; null when this commit added the file. */
  previous: { id: string; path: string } | null;
}

export interface Blame {
  /** The repository's working-tree root. */
  root: string;
  commits: BlameCommit[];
  /** Per line (0-based): index into `commits`, null = not committed yet. */
  lines: (number | null)[];
}

export interface GitSetting {
  key: string;
  value: string;
  /** The setting as a `git config --global` command. */
  command: string;
  /** What git has for the key now. */
  current: string | null;
}

export interface GitSetup {
  gitAvailable: boolean;
  settings: GitSetting[];
}

export interface Snippet {
  tool: string;
  target: string;
  text: string;
}

export const vcsApi = {
  waitDone: (token: string) => invoke<void>("vcs_wait_done", { token }),
  gitSetupPreview: (makeDefault: boolean) => invoke<GitSetup>("vcs_git_setup_preview", { makeDefault }),
  gitSetupApply: (makeDefault: boolean) => invoke<GitSetup>("vcs_git_setup_apply", { makeDefault }),
  snippets: () => invoke<Snippet[]>("vcs_snippets"),
  detect: (path: string) => invoke<RepoInfo>("vcs_detect", { path }),
  revisions: (kind: VcsKind, path: string, limit?: number) => invoke<Revision[]>("vcs_revisions", { kind, path, limit }),
  /**
   * Who last changed each line of a local working-tree file or a `git:` side. `text`: the
   * file's unsaved contents, so edited lines show as not committed and line numbers match.
   */
  blame: (path: string, text: string | null) => invoke<Blame>("vcs_blame", { path, text }),
};

/** A `git:` path to one file at one commit (`path` is repository-relative). */
export const gitFileAt = (root: string, rev: string, path: string) => `git:${root}@${rev}/${path}`;

export const VCS_NAMES: Record<VcsKind, string> = { git: "Git", hg: "Mercurial" };

/** `git:` / `hg:` paths: a working-tree file or folder at a revision. */
export const vcsKindOf = (path: string): VcsKind | null => {
  const m = /^(git|hg):/i.exec(path);
  return m ? (m[1].toLowerCase() as VcsKind) : null;
};

/** `git:<path>@<rev>` (the default revision is left out). */
export function vcsPath(kind: VcsKind, location: string, rev: string): string {
  const r = rev.trim();
  const plain = !r || (kind === "git" ? r === "HEAD" : r === ".");
  return `${kind}:${location}${plain ? "" : `@${r}`}`;
}

/** The working-tree path and revision of a `git:`/`hg:` path typed earlier (best effort). */
export function splitVcsPath(path: string): { kind: VcsKind; location: string; rev: string } | null {
  const kind = vcsKindOf(path);
  if (!kind) return null;
  const body = path.slice(kind.length + 1);
  const at = body.search(/@(?!\{)/);
  return at > 0 ? { kind, location: body.slice(0, at), rev: body.slice(at + 1) } : { kind, location: body, rev: "" };
}
