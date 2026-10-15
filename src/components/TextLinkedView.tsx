// araxis:text-view — Text Compare without vertical padding: each side shows only
// its own lines, both panes scroll together so corresponding blocks meet at a
// focus line, and the middle gutter links every difference's left lines to
// its right lines with a curved band (plus dashed lines for sync points).
import {
  forwardRef,
  useCallback,
  useImperativeHandle,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
  type WheelEvent,
} from "react";
import { blockAt, sideOffsetAt, type LinkedLayout } from "../lib/textLinks";
import type { Side } from "../lib/textEdit";
import type { VarListHandle } from "./VarList";

interface Props {
  layout: LinkedLayout;
  rowHeight: number;
  /** Width of the middle gutter (px). */
  midWidth: number;
  /** One side's row for display item `d` (a line of that side, or a gap). */
  renderSide: (side: Side, d: number) => ReactNode;
  /** The current difference: its band is emphasised (the copy arrows live in each pane's gutter). */
  currentHunk: number;
  onPickHunk: (hunk: number) => void;
  /** Sync points as display indices of their left and right lines. */
  syncLinks: { left: number; right: number }[];
  /** Floats over item `index` of `side` (the inline line editor). */
  overlay?: { side: Side; index: number; node: ReactNode } | null;
  onWheel?: (e: WheelEvent<HTMLDivElement>) => void;
  className?: string;
}

const SIDES: Side[] = ["left", "right"];

export const TextLinkedView = forwardRef<VarListHandle, Props>(function TextLinkedView(
  { layout, rowHeight: RH, midWidth: W, renderSide, currentHunk, onPickHunk, syncLinks, overlay, onWheel, className },
  ref,
) {
  const scroller = useRef<HTMLDivElement>(null);
  const [scrollTop, setScrollTop] = useState(0);
  const [height, setHeight] = useState(600);

  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setHeight(el.clientHeight));
    ro.observe(el);
    setHeight(el.clientHeight);
    return () => ro.disconnect();
  }, []);

  const blocks = layout.u.length - 1;
  const total = (blocks > 0 ? layout.u[blocks] : 0) * RH;
  const range = Math.max(0, total - height);

  /**
   * Each pane's top (px) for a scroll position. The focus line slides from
   * the top of the viewport to the bottom as the scroll goes from start to
   * end, so both files start and end flush; in between, the lines at the
   * focus line correspond.
   */
  const tops = useCallback(
    (st: number) => {
      const p = range > 0 ? Math.min(1, Math.max(0, st / range)) : 0;
      const fy = p * height;
      const u = (st + fy) / RH;
      return {
        left: sideOffsetAt(layout, "left", u) * RH - fy,
        right: sideOffsetAt(layout, "right", u) * RH - fy,
      };
    },
    [range, height, layout, RH],
  );

  /** Scroll position that puts padded position `u` (px) on the focus line. */
  const scrollFor = useCallback((u: number) => (total > 0 ? Math.max(0, Math.min(range, (u * range) / total)) : 0), [range, total]);

  useImperativeHandle(
    ref,
    () => ({
      scrollToRow(d: number) {
        const el = scroller.current;
        if (!el || d < 0) return;
        el.scrollTop = scrollFor(d * RH + RH / 2);
      },
      ensureVisible(d: number, jump = false) {
        const el = scroller.current;
        if (!el || d < 0) return;
        const shown = SIDES.filter((s) => layout.pos[s][d] >= 0);
        const outside = (st: number) => {
          const t = tops(st);
          for (const s of shown) {
            const y = layout.pos[s][d] * RH - t[s];
            if (y < 0) return y;
            if (y + RH > height) return y + RH - height;
          }
          return 0;
        };
        let st = el.scrollTop;
        if (jump && outside(st) !== 0) {
          el.scrollTop = scrollFor(d * RH + RH / 2);
          return;
        }
        // Nudge by the overshoot; a few rounds settle it (the panes don't move 1:1 with the scroll).
        for (let i = 0; i < 4; i++) {
          const off = outside(st);
          if (!off) break;
          st = Math.max(0, Math.min(range, st + off));
        }
        if (outside(st)) st = scrollFor(d * RH + RH / 2);
        el.scrollTop = st;
      },
    }),
    [layout, tops, scrollFor, RH, height, range],
  );

  const onScroll = useCallback(() => {
    if (scroller.current) setScrollTop(scroller.current.scrollTop);
  }, []);

  const st = Math.min(scrollTop, range);
  const top = tops(st);

  const pane = (side: Side) => {
    const items = layout.items[side];
    const t = top[side];
    const first = Math.max(0, Math.floor(t / RH));
    const last = Math.min(items.length, Math.ceil((t + height) / RH) + 1);
    const out: ReactNode[] = [];
    for (let i = first; i < last; i++) {
      out.push(
        <div key={items[i]} className="vrow" style={{ top: i * RH - t, height: RH }}>
          {renderSide(side, items[i])}
        </div>,
      );
    }
    if (overlay && overlay.side === side) {
      out.push(
        <div key="overlay" className="vrow tc-editor-layer" style={{ top: overlay.index * RH - t, height: RH }}>
          {overlay.node}
        </div>,
      );
    }
    return <div className={`tc-pane tc-pane-${side}`}>{out}</div>;
  };

  // Bands for the blocks on screen on either side.
  const bands: ReactNode[] = [];
  if (blocks > 0) {
    const from = Math.min(blockAt(layout.l, top.left / RH), blockAt(layout.r, top.right / RH));
    const to = Math.max(blockAt(layout.l, (top.left + height) / RH), blockAt(layout.r, (top.right + height) / RH));
    const h2 = W / 2;
    for (let b = from; b <= to && b < blocks; b++) {
      const kind = layout.kind[b];
      if (!kind) continue;
      const l0 = layout.l[b] * RH - top.left;
      const l1 = layout.l[b + 1] * RH - top.left;
      const r0 = layout.r[b] * RH - top.right;
      const r1 = layout.r[b + 1] * RH - top.right;
      const h = layout.hunk[b];
      const current = h >= 0 && h === currentHunk;
      bands.push(
        <path
          key={b}
          className={`tc-band k-${kind}${current ? " current" : ""}`}
          d={`M0 ${l0} C${h2} ${l0} ${h2} ${r0} ${W} ${r0} L${W} ${r1} C${h2} ${r1} ${h2} ${l1} 0 ${l1} Z`}
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => h >= 0 && onPickHunk(h)}
        />,
      );
    }
  }
  const links = syncLinks.flatMap((s, i) => {
    const li = layout.pos.left[s.left];
    const ri = layout.pos.right[s.right];
    if (li < 0 || ri < 0) return [];
    const yl = li * RH + RH / 2 - top.left;
    const yr = ri * RH + RH / 2 - top.right;
    if ((yl < 0 && yr < 0) || (yl > height && yr > height)) return [];
    return [<line key={`s${i}`} className="tc-sync-link" x1={0} y1={yl} x2={W} y2={yr} />];
  });

  return (
    <div ref={scroller} className={`vlist ${className ?? ""}`} onScroll={onScroll} onWheel={onWheel}>
      <div className="tc-linked-view" style={{ height }}>
        {pane("left")}
        <div className="tc-links">
          <svg width={W} height={height}>
            {bands}
            {links}
          </svg>
        </div>
        {pane("right")}
      </div>
      <div style={{ height: range }} />
    </div>
  );
});
