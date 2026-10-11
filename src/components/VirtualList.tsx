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

export interface VirtualListHandle {
  /** Scrolls so `row` sits roughly a third of the way down the viewport. */
  scrollToRow: (row: number) => void;
}

interface Props {
  count: number;
  rowHeight: number;
  renderRow: (index: number) => ReactNode;
  onRangeChange?: (first: number, last: number) => void;
  onWheel?: (e: WheelEvent<HTMLDivElement>) => void;
  className?: string;
  overscan?: number;
}

export const VirtualList = forwardRef<VirtualListHandle, Props>(function VirtualList(
  { count, rowHeight, renderRow, onRangeChange, onWheel, className, overscan = 12 },
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

  useImperativeHandle(
    ref,
    () => ({
      scrollToRow(row: number) {
        const el = scroller.current;
        if (!el) return;
        el.scrollTop = Math.max(0, row * rowHeight - el.clientHeight / 3);
      },
    }),
    [rowHeight],
  );

  const first = Math.max(0, Math.floor(scrollTop / rowHeight) - overscan);
  const last = Math.min(count, Math.ceil((scrollTop + height) / rowHeight) + overscan);

  const rangeCb = useRef(onRangeChange);
  rangeCb.current = onRangeChange;
  useLayoutEffect(() => {
    rangeCb.current?.(first, last);
  }, [first, last]);

  const onScroll = useCallback(() => {
    if (scroller.current) setScrollTop(scroller.current.scrollTop);
  }, []);

  const items: ReactNode[] = [];
  for (let i = first; i < last; i++) {
    items.push(
      <div key={i} className="vrow" style={{ top: i * rowHeight, height: rowHeight }}>
        {renderRow(i)}
      </div>,
    );
  }

  return (
    <div ref={scroller} className={`vlist ${className ?? ""}`} onScroll={onScroll} onWheel={onWheel}>
      <div className="vlist-spacer" style={{ height: count * rowHeight }}>
        {items}
      </div>
    </div>
  );
});
