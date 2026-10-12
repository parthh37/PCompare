// Remote write operations and SSH sign-in prompts (src-tauri/src/remote.rs).
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface PromptField {
  prompt: string;
  /** Whether the answer may be shown while typed (false for passwords). */
  echo: boolean;
}

/** A question an SSH server asked while signing in (keyboard-interactive). */
export interface AuthPrompt {
  /** Connection profile name. */
  profile: string;
  /** `user@host`. */
  target: string;
  name: string;
  instructions: string;
  fields: PromptField[];
}

export interface AuthPromptEvent {
  id: number;
  prompt: AuthPrompt;
}

export const remoteApi = {
  mkdir: (path: string) => invoke<void>("remote_mkdir", { path }),
  /** Permanent: remote folders have no Recycle Bin. */
  remove: (path: string) => invoke<void>("remote_delete", { path }),
  rename: (path: string, newName: string) => invoke<void>("remote_rename", { path, newName }),
  canRename: (path: string) => invoke<boolean>("remote_can_rename", { path }),
  /** One answer per field, or `null` to cancel the sign-in. */
  answerAuth: (id: number, answers: string[] | null) => invoke<void>("remote_auth_answer", { id, answers }),
  onAuthPrompt: (cb: (e: AuthPromptEvent) => void): Promise<UnlistenFn> =>
    listen<AuthPromptEvent>("remote-auth-prompt", (e) => cb(e.payload)),
};
