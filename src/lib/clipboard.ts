// System clipboard text through tauri-plugin-clipboard-manager (WebView
// clipboard access prompts or is blocked on some platforms), falling back to
// the browser API outside Tauri.
import { readText, writeText } from "@tauri-apps/plugin-clipboard-manager";

export async function readClipboardText(): Promise<string> {
  try {
    return await readText();
  } catch (e) {
    if (navigator.clipboard?.readText) return navigator.clipboard.readText();
    throw e;
  }
}

export async function writeClipboardText(text: string): Promise<void> {
  try {
    await writeText(text);
  } catch (e) {
    if (navigator.clipboard?.writeText) return navigator.clipboard.writeText(text);
    throw e;
  }
}
