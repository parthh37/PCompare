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

export interface VarListHandle {
  /** Scrolls so `row` sits roughly a third of the way down the viewport. */
  scrollToRow: (row: number) => void;
  /**
   * Brings `row` fully into view: by the least scroll, or with `jump` by
   * placing it like `scrollToRow` when it is not visible at all.
   */
  ensureVisible: (row: number, jump?: boolean) => void;
}

interface Props {
  count: number;
  /** Height of every row when `offsets` is not given. */
  rowHeight: number;
  /** Prefix sums of row heights (`count + 1` entries); enables variable heights. */
  offsets?: Float64Array | null;
  renderRow: (index: number) => ReactNode;
  /** Drawn inside the scrolled area on top of the rows (e.g. an inline editor). */
  overlay?: ReactNode;
  onWheel?: (e: WheelEvent<HTMLDivElement>) => void;
  className?: string;
  overscan?: number;
}

/**
 * A virtualized list like `VirtualList`, but rows may have different heights
 * (word-wrapped text) and an overlay can float in the scrolled content.
 */
export const VarList = forwardRef<VarListHandle, Props>(function VarList(
  { count, rowHeight, offsets, renderRow, overlay, onWheel, className, overscan = 8 },
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

  const top = useCallback((i: number) => (offsets ? offsets[Math.min(i, count)] : i * rowHeight), [offsets, rowHeight, count]);
  /** The row containing pixel `y`. */
  const rowAt = useCallback(
    (y: number) => {
      if (!offsets) return Math.floor(y / rowHeight);
      let lo = 0;
      let hi = count;
      while (lo < hi) {
        const mid = (lo + hi + 1) >> 1;
        if (offsets[mid] <= y) lo = mid;
        else hi = mid - 1;
      }
      return lo;
    },
    [offsets, rowHeight, count],
  );

  useImperativeHandle(
    ref,
    () => ({
      scrollToRow(row: number) {
        const el = scroller.current;
        if (!el || row < 0) return;
        el.scrollTop = Math.max(0, top(row) - el.clientHeight / 3);
      },
      ensureVisible(row: number, jump = false) {
        const el = scroller.current;
        if (!el || row < 0) return;
        const t = top(row);
        const b = top(row + 1);
        const offscreen = b <= el.scrollTop || t >= el.scrollTop + el.clientHeight;
        if (jump && offscreen) el.scrollTop = Math.max(0, t - el.clientHeight / 3);
        else if (t < el.scrollTop) el.scrollTop = t;
        else if (b > el.scrollTop + el.clientHeight) el.scrollTop = b - el.clientHeight;
      },
    }),
    [top],
  );

  const first = Math.max(0, rowAt(scrollTop) - overscan);
  const last = Math.min(count, rowAt(scrollTop + height) + 1 + overscan);

  const onScroll = useCallback(() => {
    if (scroller.current) setScrollTop(scroller.current.scrollTop);
  }, []);

  const items: ReactNode[] = [];
  for (let i = first; i < last; i++) {
    const t = top(i);
    items.push(
      <div key={i} className="vrow" style={{ top: t, height: top(i + 1) - t }}>
        {renderRow(i)}
      </div>,
    );
  }

  return (
    <div ref={scroller} className={`vlist ${className ?? ""}`} onScroll={onScroll} onWheel={onWheel}>
      <div className="vlist-spacer" style={{ height: top(count) }}>
        {items}
        {overlay}
      </div>
    </div>
  );
});
