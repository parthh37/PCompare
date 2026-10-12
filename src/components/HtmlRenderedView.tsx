import { useEffect, useMemo, useRef, useState } from "react";
import { errorMessage } from "../lib/api";
import { convertApi } from "../lib/convertApi";
import { extension } from "../lib/sessions";
import "./HtmlRenderedView.css";

/** Files the "Rendered view" toggle is offered for. */
export const isHtmlPath = (path: string) => ["html", "htm", "xhtml"].includes(extension(path));

/** One side: its current (possibly edited) lines, or the file itself when its text was converted. */
export interface RenderedSide {
  path: string;
  lines: string[];
  /** The side shows converted text, so the original file is rendered instead. */
  converted: boolean;
}

// Nothing may load or run: no scripts, no network; inline styles and data: images only.
const CSP =
  "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:; media-src data:\">";

/** The page with the lockdown policy placed in its head. */
function lockDown(html: string): string {
  const head = /<head(\s[^>]*)?>/i.exec(html);
  if (head) return html.slice(0, head.index + head[0].length) + CSP + html.slice(head.index + head[0].length);
  const root = /<html(\s[^>]*)?>/i.exec(html);
  if (root) return html.slice(0, root.index + root[0].length) + `<head>${CSP}</head>` + html.slice(root.index + root[0].length);
  return CSP + html;
}

/**
 * Both sides of an HTML compare rendered side by side ("formatted HTML
 * view"). The frames are sandboxed without scripts, forms or navigation;
 * only same-origin access is granted so their scrolling can be linked.
 */
export function HtmlRenderedView({ left, right }: { left: RenderedSide; right: RenderedSide }) {
  const frames = [useRef<HTMLIFrameElement>(null), useRef<HTMLIFrameElement>(null)];
  const [linked, setLinked] = useState(true);
  const linkedRef = useRef(linked);
  linkedRef.current = linked;
  const [sources, setSources] = useState<(string | null)[]>([null, null]);
  const [error, setError] = useState<string | null>(null);

  // Converted sides render the original file; others render the text being compared.
  const leftText = useMemo(() => (left.converted ? null : left.lines.join("\n")), [left.converted, left.lines]);
  const rightText = useMemo(() => (right.converted ? null : right.lines.join("\n")), [right.converted, right.lines]);
  useEffect(() => {
    let stale = false;
    const sides = [left, right];
    Promise.all(sides.map((s) => (s.converted ? convertApi.sourceText(s.path) : Promise.resolve(null))))
      .then((texts) => !stale && setSources(texts))
      .catch((e) => !stale && setError(errorMessage(e)));
    return () => {
      stale = true;
    };
    // Re-read only when the file or its conversion changes.
  }, [left.path, right.path, left.converted, right.converted]);
  const docs = [leftText ?? sources[0], rightText ?? sources[1]].map((t) => (t === null ? null : lockDown(t)));

  /** Wires a loaded frame: linked scrolling and inert links. */
  const onLoad = (i: number) => {
    const win = frames[i].current?.contentWindow;
    const doc = frames[i].current?.contentDocument;
    if (!win || !doc) return;
    doc.addEventListener("click", (e) => {
      const a = (e.target as Element | null)?.closest?.("a[href]");
      // In-page anchors still jump; anything else would navigate the frame away.
      if (a && !a.getAttribute("href")?.startsWith("#")) e.preventDefault();
    });
    win.addEventListener("scroll", () => {
      if (!linkedRef.current) return;
      const other = frames[1 - i].current?.contentWindow;
      const src = doc.scrollingElement;
      const dst = other?.document.scrollingElement;
      if (!other || !src || !dst) return;
      const ratio = (h: Element) => Math.max(1, h.scrollHeight - h.clientHeight);
      const top = (src.scrollTop / ratio(src)) * ratio(dst);
      const left = src.scrollLeft;
      // Skip echoes of the scroll this handler caused on the other frame.
      if (Math.abs(dst.scrollTop - top) < 2 && Math.abs(dst.scrollLeft - left) < 2) return;
      other.scrollTo(left, top);
    });
  };

  return (
    <div className="hrv">
      <div className="hrv-bar">
        <label className="field check">
          <input type="checkbox" checked={linked} onChange={(e) => setLinked(e.target.checked)} />
          Link scrolling
        </label>
        <span className="muted">Rendered without scripts, network access or navigation.</span>
        {error && <span className="hrv-error">{error}</span>}
      </div>
      <div className="hrv-panes">
        {docs.map((doc, i) =>
          doc === null ? (
            <div key={i} className="hrv-pane muted">
              Loading…
            </div>
          ) : (
            <iframe
              key={i}
              ref={frames[i]}
              className="hrv-pane"
              title={i === 0 ? "Left page" : "Right page"}
              sandbox="allow-same-origin"
              referrerPolicy="no-referrer"
              srcDoc={doc}
              onLoad={() => onLoad(i)}
            />
          ),
        )}
      </div>
    </div>
  );
}
