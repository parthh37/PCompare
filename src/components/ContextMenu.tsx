import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { cleanEntries, nativeMenusAvailable, popupNativeMenu, SEPARATOR, type MenuEntry, type MenuItems } from "../lib/nativeMenu";

interface Point {
  x: number;
  y: number;
}

/**
 * A right-click menu for `target`, drawn by the OS. `build` describes the
 * items and runs after the render that follows `open`, so selection or
 * caret changes made in the same event are already applied. Without a
 * Tauri runtime the same items show as an HTML menu (`element`).
 */
export function useContextMenu<T>(build: (target: T) => MenuItems | Promise<MenuItems>) {
  const [request, setRequest] = useState<(Point & { target: T }) | null>(null);
  const [html, setHtml] = useState<(Point & { entries: MenuEntry[] }) | null>(null);
  const buildRef = useRef(build);
  useLayoutEffect(() => {
    buildRef.current = build;
  });

  useEffect(() => {
    if (!request) return;
    let live = true;
    const at = { x: request.x, y: request.y };
    (async () => cleanEntries(await buildRef.current(request.target)))()
      .then((entries) => {
        if (!live) return;
        setRequest(null);
        if (!entries.length) return;
        if (!nativeMenusAvailable) return setHtml({ ...at, entries });
        popupNativeMenu(entries, at).catch(() => setHtml({ ...at, entries }));
      })
      .catch(() => live && setRequest(null));
    return () => {
      live = false;
    };
  }, [request]);

  return {
    /** Opens the menu at the mouse position of `e` (or any point in the window). */
    open: (at: { clientX: number; clientY: number }, target: T) => {
      setHtml(null);
      setRequest({ x: at.clientX, y: at.clientY, target });
    },
    /** True while the HTML fallback is up (a native menu tracks itself). */
    isOpen: !!request || !!html,
    element: html ? <HtmlMenu {...html} onClose={() => setHtml(null)} /> : null,
  };
}

/** The HTML stand-in for a native menu. */
function HtmlMenu({ x, y, entries, onClose }: Point & { entries: MenuEntry[]; onClose: () => void }) {
  const ref = useRef<HTMLDivElement>(null);

  // Keep it on screen: shift left at the right edge, open upwards near the bottom.
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const { width, height } = el.getBoundingClientRect();
    el.style.left = `${Math.max(4, Math.min(x, window.innerWidth - width - 4))}px`;
    el.style.top = `${y + height > window.innerHeight - 4 ? Math.max(4, y - height) : y}px`;
  }, [x, y]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("mousedown", onClose);
    window.addEventListener("blur", onClose);
    window.addEventListener("resize", onClose);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onClose);
      window.removeEventListener("blur", onClose);
      window.removeEventListener("resize", onClose);
      window.removeEventListener("keydown", onKey);
    };
  }, [onClose]);

  const render = (list: MenuEntry[], prefix = ""): ReactNode[] =>
    list.map((e, n) => {
      const i = prefix + n;
      if (e === SEPARATOR) return <hr key={i} />;
      if ("heading" in e) {
        return (
          <div key={i} className="ctx-label">
            {e.heading}
          </div>
        );
      }
      if ("submenu" in e) {
        return [
          <div key={i} className="ctx-label">
            {e.label}
          </div>,
          ...render(
            cleanEntries(e.submenu).map((s) => (e.disabled && s !== SEPARATOR && "onClick" in s ? { ...s, disabled: true } : s)),
            `${i}.`,
          ),
        ];
      }
      return (
        <button
          key={i}
          role={e.checked === undefined ? "menuitem" : "menuitemcheckbox"}
          aria-checked={e.checked}
          className={e.danger ? "danger-text" : undefined}
          disabled={e.disabled}
          onClick={() => {
            onClose();
            e.onClick();
          }}
        >
          {e.checked && "✓ "}
          {e.label}
          {e.shortcut && <span className="ctx-key">{e.shortcut}</span>}
        </button>
      );
    });

  return (
    <div className="ctx-menu" ref={ref} style={{ left: x, top: y }} role="menu" onMouseDown={(e) => e.stopPropagation()}>
      {render(entries)}
    </div>
  );
}
