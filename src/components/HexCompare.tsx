import { ArrowDown } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { api, errorMessage, type HexChunk, type HexSummary } from "../lib/api";
import { formatSize, hex } from "../lib/format";
import { PathBar } from "./PathBar";
import { VirtualList, type VirtualListHandle } from "./VirtualList";

const BYTES_PER_ROW = 16;
const CHUNK_BYTES = 4096;
const ROW_HEIGHT = 20;

interface Props {
  left: string;
  right: string;
  onPathsChange: (left: string, right: string) => void;
}

const printable = (b: number | undefined) =>
  b === undefined ? " " : b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : "·";

export function HexCompare({ left, right, onPathsChange }: Props) {
  const [summary, setSummary] = useState<HexSummary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [cursor, setCursor] = useState<number | null>(null);
  const [, bump] = useState(0);
  const chunks = useRef(new Map<number, HexChunk>());
  const pending = useRef(new Set<number>());
  const list = useRef<VirtualListHandle>(null);

  useEffect(() => {
    chunks.current.clear();
    pending.current.clear();
    setSummary(null);
    setCursor(null);
    setError(null);
    if (!left || !right) return;
    api
      .hexSummary(left, right)
      .then((s) => {
        setSummary(s);
        if (s.firstDifference != null) setCursor(s.firstDifference);
      })
      .catch((e) => setError(errorMessage(e)));
  }, [left, right]);

  const ensureRows = useCallback(
    (first: number, last: number) => {
      const firstChunk = Math.floor((first * BYTES_PER_ROW) / CHUNK_BYTES);
      const lastChunk = Math.floor((last * BYTES_PER_ROW) / CHUNK_BYTES);
      for (let c = firstChunk; c <= lastChunk; c++) {
        if (chunks.current.has(c) || pending.current.has(c)) continue;
        pending.current.add(c);
        api
          .hexChunk(left, right, c * CHUNK_BYTES, CHUNK_BYTES)
          .then((chunk) => {
            chunks.current.set(c, chunk);
            bump((n) => n + 1);
          })
          .catch((e) => setError(errorMessage(e)))
          .finally(() => pending.current.delete(c));
      }
    },
    [left, right],
  );

  useEffect(() => {
    if (cursor != null) list.current?.scrollToRow(Math.floor(cursor / BYTES_PER_ROW));
  }, [cursor]);

  const nextDiff = async () => {
    try {
      const from = cursor == null ? 0 : cursor + 1;
      const next = await api.hexNextDifference(left, right, from);
      if (next != null) setCursor(next);
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  const size = summary ? Math.max(summary.leftSize, summary.rightSize) : 0;
  const rowCount = Math.ceil(size / BYTES_PER_ROW);

  const renderRow = (row: number) => {
    const offset = row * BYTES_PER_ROW;
    const chunk = chunks.current.get(Math.floor(offset / CHUNK_BYTES));
    if (!chunk) return <div className="hrow loading">{hex(offset, 8)}</div>;
    const base = offset - chunk.offset;
    const l = chunk.left.slice(base, base + BYTES_PER_ROW);
    const r = chunk.right.slice(base, base + BYTES_PER_ROW);
    const cells = (bytes: number[], other: number[]) =>
      Array.from({ length: BYTES_PER_ROW }, (_, i) => {
        const b = bytes[i];
        const cls = b === undefined ? "hb absent" : b !== other[i] ? "hb diff" : "hb";
        const isCursor = cursor === offset + i ? " cursor" : "";
        return (
          <span key={i} className={cls + isCursor}>
            {b === undefined ? "  " : hex(b, 2)}
          </span>
        );
      });
    const ascii = (bytes: number[], other: number[]) =>
      Array.from({ length: BYTES_PER_ROW }, (_, i) => (
        <span key={i} className={bytes[i] !== other[i] ? "ha diff" : "ha"}>
          {printable(bytes[i])}
        </span>
      ));
    const rowDiffers = l.length !== r.length || l.some((b, i) => b !== r[i]);
    return (
      <div className={`hrow${rowDiffers ? " differs" : ""}`}>
        <span className="hoff">{hex(offset, 8)}</span>
        <span className="hbytes">{cells(l, r)}</span>
        <span className="hascii">{ascii(l, r)}</span>
        <span className="mid" />
        <span className="hbytes">{cells(r, l)}</span>
        <span className="hascii">{ascii(r, l)}</span>
      </div>
    );
  };

  return (
    <div className="session">
      <div className="toolbar">
        <button onClick={nextDiff} disabled={!summary || summary.identical}>
          <ArrowDown size={14} /> Next difference
        </button>
        <span className="spacer" />
      </div>
      <PathBar left={left} right={right} directory={false} onCompare={onPathsChange} />
      <div className="session-body">
        {error && <div className="banner error">{error}</div>}
        {summary && (
          <VirtualList
            ref={list}
            className="hex-list"
            count={rowCount}
            rowHeight={ROW_HEIGHT}
            renderRow={renderRow}
            onRangeChange={ensureRows}
          />
        )}
      </div>
      <div className="statusbar">
        {summary &&
          (summary.identical ? (
            <span className="ok">Binary identical · {formatSize(summary.leftSize)}</span>
          ) : (
            <>
              <span className="s-different-fg">{summary.differingBytes.toLocaleString()} differing bytes</span>
              <span>Left {formatSize(summary.leftSize)}</span>
              <span>Right {formatSize(summary.rightSize)}</span>
              {cursor != null && <span>Offset 0x{hex(cursor, 8)}</span>}
            </>
          ))}
      </div>
    </div>
  );
}
