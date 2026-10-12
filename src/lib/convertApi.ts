// Typed bridge to src-tauri/src/convert.rs: "File formats" rules (documents
// converted to text before Text Compare: Word, PDF, RTF, OpenDocument, Delphi
// forms, HTML, JSON / XML pretty-print, or an external command) and
// "External tools" rules (pairs opened in a third-party diff utility).
import { invoke } from "@tauri-apps/api/core";

export type Converter = "none" | "word" | "pdf" | "rtf" | "odt" | "dfm" | "html" | "json" | "xml";

export const CONVERTERS: { id: Converter; label: string }[] = [
  { id: "none", label: "None (compare as is)" },
  { id: "word", label: "Microsoft Word (.docx, .doc)" },
  { id: "pdf", label: "Adobe PDF" },
  { id: "rtf", label: "Rich Text (.rtf)" },
  { id: "odt", label: "OpenDocument text (.odt)" },
  { id: "dfm", label: "Delphi form (binary → text)" },
  { id: "html", label: "HTML → plain text" },
  { id: "json", label: "JSON pretty-print (sorted keys)" },
  { id: "xml", label: "XML pretty-print" },
];

export interface FileFormat {
  name: string;
  /** `;`-separated masks, e.g. `*.htm;*.html`; a mask with a slash matches the whole path. */
  mask: string;
  enabled: boolean;
  converter: Converter;
  /** External conversion command (`conv.exe %s %t`); replaces `converter` when set. */
  command: string;
  readOnly: boolean;
  /** Grammar id forced for matching files; "" = by extension. */
  grammar: string;
}

export interface ExternalTool {
  name: string;
  mask: string;
  enabled: boolean;
  /** Command line with `%1` / `%2` for the left / right file. */
  command: string;
  /** Matching pairs always open in this tool instead of a built-in viewer. */
  always: boolean;
}

export interface ConvertConfig {
  formats: FileFormat[];
  externalTools: ExternalTool[];
}

/** What a file format rule did to a Text Compare side (`TextDocument.conversion`). */
export interface Conversion {
  rule: string;
  /** "PDF", "Word", "external command"... when the text was converted. */
  convertedFrom: string | null;
  readOnly: boolean;
  grammar: string | null;
}

export const convertApi = {
  loadConfig: () => invoke<ConvertConfig>("convert_config_load"),
  saveConfig: (config: ConvertConfig) => invoke<void>("convert_config_save", { config }),
  defaults: () => invoke<ConvertConfig>("convert_config_defaults"),
  /** Enabled external tools whose mask matches both sides. */
  toolsFor: (left: string, right: string) => invoke<ExternalTool[]>("convert_tools_for", { left, right }),
  launchTool: (command: string, left: string, right: string) =>
    invoke<void>("convert_launch_tool", { command, left, right }),
  /** A file's original (unconverted) text. */
  sourceText: (path: string) => invoke<string>("convert_source_text", { path }),
};

/**
 * Opens a pair of files in the first matching "always use" external tool.
 * Resolves `true` if a tool was started (the caller then skips its own
 * viewer); a failing lookup resolves `false` so the built-in viewer opens.
 */
export async function openExternalIfAlways(left: string, right: string): Promise<boolean> {
  let tools: ExternalTool[];
  try {
    tools = await convertApi.toolsFor(left, right);
  } catch {
    return false;
  }
  const tool = tools.find((t) => t.always);
  if (!tool) return false;
  await convertApi.launchTool(tool.command, left, right);
  return true;
}

export const emptyFormat = (): FileFormat => ({
  name: "New format",
  mask: "*.ext",
  enabled: true,
  converter: "none",
  command: "",
  readOnly: true,
  grammar: "",
});

export const emptyTool = (): ExternalTool => ({
  name: "New tool",
  mask: "*.*",
  enabled: true,
  command: '"C:\\Tools\\Diff.exe" %1 %2',
  always: false,
});
