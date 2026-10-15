// the comparison Statistics window for Text and Folder Compare.
import { Copy } from "lucide-react";
import { useMemo, useState } from "react";
import type { DiffRow, EntryStatus, FolderEntry, Hunk } from "../lib/api";
import { writeClipboardText } from "../lib/clipboard";
import { formatSize } from "../lib/format";
import { folderStats, textStats, type TextSideInput } from "../lib/reviewStats";
import { Modal } from "./Modal";
import "./Review.css";

export type StatsSource =
  | { kind: "text"; rows: DiffRow[]; hunks: Hunk[]; left: TextSideInput; right: TextSideInput }
  | { kind: "folder"; entries: FolderEntry[]; leftRoot: string; rightRoot: string };

/** One table row: a label and the left / right (or single, spanning) values. */
type Line = [label: string, left: string, right?: string, cls?: string];

const n = (v: number) => v.toLocaleString();
const pct = (v: number) => `${(v * 100).toFixed(v > 0 && v < 0.001 ? 2 : 1)}%`;

function textLines(s: Extract<StatsSource, { kind: "text" }>): { sides: [string, string]; groups: [string, Line[]][] } {
  const t = textStats(s.rows, s.hunks, s.left, s.right);
  return {
    sides: [t.left.label, t.right.label],
    groups: [
      [
        "Files",
        [
          ["Lines", n(t.left.lines), n(t.right.lines)],
          ["Size", formatSize(t.left.size), formatSize(t.right.size)],
          ["Encoding", t.left.encoding, t.right.encoding],
          ["Line endings", t.left.lineEnding, t.right.lineEnding],
          ["Characters changed", n(t.left.charsChanged), n(t.right.charsChanged)],
        ],
      ],
      [
        "Lines",
        [
          ["Identical", n(t.identical), undefined, "s-same-fg"],
          ["Changed", n(t.changed), undefined, "k-changed-fg"],
          ["Inserted (right only)", n(t.inserted), undefined, "k-rightOnly-fg"],
          ["Deleted (left only)", n(t.deleted), undefined, "k-leftOnly-fg"],
          ["Unimportant", n(t.unimportant), undefined, "k-unimportant-fg"],
          ["Difference blocks", n(t.blocks)],
          ["Similarity", pct(t.similarity)],
        ],
      ],
    ],
  };
}

const STATUS_ROWS: [EntryStatus, string, string][] = [
  ["same", "Same", "s-same-fg"],
  ["similar", "Similar (unimportant differences)", "s-similar-fg"],
  ["different", "Different", "s-different-fg"],
  ["leftOnly", "Left only", "s-leftOnly-fg"],
  ["rightOnly", "Right only", "s-rightOnly-fg"],
  ["typeMismatch", "File ↔ folder mismatch", "s-error-fg"],
  ["error", "Errors", "s-error-fg"],
];

function folderLines(s: Extract<StatsSource, { kind: "folder" }>): { sides: [string, string]; groups: [string, Line[]][] } {
  const f = folderStats(s.entries);
  const byStatus = (counts: Record<EntryStatus, number>, folders: boolean): Line[] =>
    STATUS_ROWS.filter(([st]) => counts[st] > 0 || (!folders && (st === "same" || st === "different"))).map(
      ([st, label, cls]) => [label, n(counts[st]), undefined, cls],
    );
  return {
    sides: [s.leftRoot || "(none)", s.rightRoot || "(none)"],
    groups: [
      [
        "Sides",
        [
          ["Files", n(f.left.files), n(f.right.files)],
          ["Folders", n(f.left.folders), n(f.right.folders)],
          ["Total size", formatSize(f.left.bytes), formatSize(f.right.bytes)],
          ["Size of differing files", formatSize(f.differingBytes.left), formatSize(f.differingBytes.right)],
          ["Orphan files", n(f.left.orphanFiles), n(f.right.orphanFiles)],
          ["Orphan folders", n(f.left.orphanFolders), n(f.right.orphanFolders)],
          ["Newer files", n(f.left.newer), n(f.right.newer)],
        ],
      ],
      [
        "Files by status",
        [...byStatus(f.files, false), ...(f.ignored ? ([["Ignored (counted as same)", n(f.ignored)]] as Line[]) : [])],
      ],
      ["Folders by status", byStatus(f.folders, true)],
    ],
  };
}

/** Statistics of the comparison in front, as a table; Copy puts it on the clipboard as text. */
export function StatsDialog({ source, onClose }: { source: StatsSource; onClose: () => void }) {
  const { sides, groups } = useMemo(() => (source.kind === "text" ? textLines(source) : folderLines(source)), [source]);
  const [copied, setCopied] = useState(false);

  const asText = () =>
    [`\t${sides[0]}\t${sides[1]}`]
      .concat(groups.flatMap(([title, lines]) => [title, ...lines.map(([l, a, b]) => `  ${l}\t${a}${b !== undefined ? `\t${b}` : ""}`)]))
      .join("\n");

  return (
    <Modal
      title={source.kind === "text" ? "Text comparison statistics" : "Folder comparison statistics"}
      onClose={onClose}
      wide
      footer={
        <>
          <button
            onClick={() =>
              writeClipboardText(asText())
                .then(() => setCopied(true))
                .catch(() => setCopied(false))
            }
          >
            <Copy size={14} /> {copied ? "Copied" : "Copy"}
          </button>
          <button className="primary" onClick={onClose}>
            Close
          </button>
        </>
      }
    >
      <table className="rv-stats">
        <thead>
          <tr>
            <th />
            <th title={sides[0]}>Left · {sides[0]}</th>
            <th title={sides[1]}>Right · {sides[1]}</th>
          </tr>
        </thead>
        {groups.map(([title, lines]) => (
          <tbody key={title}>
            <tr className="rv-stats-group">
              <th colSpan={3}>{title}</th>
            </tr>
            {lines.map(([label, a, b, cls]) => (
              <tr key={label}>
                <td>{label}</td>
                {b === undefined ? (
                  <td colSpan={2} className={`span${cls ? ` ${cls}` : ""}`}>
                    {a}
                  </td>
                ) : (
                  <>
                    <td className="num">{a}</td>
                    <td className="num">{b}</td>
                  </>
                )}
              </tr>
            ))}
          </tbody>
        ))}
      </table>
    </Modal>
  );
}
