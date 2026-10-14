import { errorMessage } from "./api";
import { convertApi } from "./convertApi";
import type { MenuItems } from "./nativeMenu";

/**
 * Context-menu items opening a file pair in the matching third-party
 * comparison tools ("External tools" rules), or a link to set one up.
 */
export async function externalToolItems(
  left: string,
  right: string,
  onError: (message: string) => void,
  /** Opens the External tools settings (shown when no tool matches). */
  onConfigure: () => void,
): Promise<MenuItems> {
  const tools = await convertApi.toolsFor(left, right).catch(() => []);
  if (!tools.length) return [{ label: "Open with external tool…", onClick: onConfigure }];
  return tools.map((t) => ({
    label: `Open with ${t.name}`,
    onClick: () => void convertApi.launchTool(t.command, left, right).catch((e) => onError(errorMessage(e))),
  }));
}
