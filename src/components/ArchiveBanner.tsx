// tells the user when a Folder Compare side is an archive,
// snapshot or Subversion repository: what it is, whether it can be written
// to, and how contents are compared against a snapshot.
import { Binary, FileArchive } from "lucide-react";
import { useEffect, useState } from "react";
import type { ContentMethod } from "../lib/api";
import { archiveApi, containerLabel, isArchivePath, type SideInfo } from "../lib/archiveApi";
import { formatTime } from "../lib/format";
import "./Archives.css";

interface Props {
  left: string;
  right: string;
  content: ContentMethod;
  /** Compare two archive files byte by byte instead (Hex Compare). */
  onOpenHex: (left: string, right: string) => void;
}

function describe(side: string, info: SideInfo): string {
  if (!info.container) return "";
  if (info.snapshot) {
    const s = info.snapshot;
    return (
      `${side}: snapshot of ${s.source}, taken ${formatTime(s.createdMs)} — ` +
      `${s.summary.files.toLocaleString()} files, ${s.includeCrcs ? "with" : "without"} CRCs. File contents aren't stored.`
    );
  }
  const what = containerLabel(info.container);
  // git:/hg: sides (svnAvailable reports their client too)
  if (info.container.type === "git" || info.container.type === "hg") {
    const client = info.container.type;
    return info.svnAvailable === false
      ? `${side}: ${what} — the ${client} command-line client isn't installed, so it can't be listed.`
      : `${side}: ${what} (read-only).`;
  }
  if (info.container.type === "svn") {
    return info.svnAvailable === false
      ? `${side}: Subversion repository — the svn command-line client isn't installed, so it can't be listed.`
      : `${side}: Subversion repository (read-only).`;
  }
  return `${side}: ${what}, browsed in place${info.readOnly ? ` (read-only: ${info.readOnly})` : " — copies and deletes rewrite the archive"}.`;
}

export function ArchiveBanner({ left, right, content, onOpenHex }: Props) {
  const [infos, setInfos] = useState<[SideInfo | null, SideInfo | null]>([null, null]);
  useEffect(() => {
    let live = true;
    const load = (p: string) => (p ? archiveApi.sideInfo(p).catch(() => null) : Promise.resolve(null));
    Promise.all([load(left), load(right)]).then((r) => live && setInfos(r));
    return () => {
      live = false;
    };
  }, [left, right]);

  const [l, r] = infos;
  const lines = [l && describe("Left", l), r && describe("Right", r)].filter(Boolean) as string[];
  if (!lines.length) return null;
  const snaps = [l?.snapshot, r?.snapshot].filter(Boolean);
  let notice = "";
  if (snaps.length && content !== "off") {
    notice = snaps.every((s) => s!.includeCrcs)
      ? "Contents are compared by the CRC-32s stored in the snapshot."
      : "Content unavailable: the snapshot was saved without CRCs, so files are compared by size and date only.";
  }
  const bothArchives = isArchivePath(left) && isArchivePath(right);
  return (
    <div className="banner info archive-banner">
      <FileArchive size={14} aria-hidden />
      <div className="archive-banner-text">
        {lines.map((t) => (
          <div key={t}>{t}</div>
        ))}
        {notice && <div className="archive-banner-notice">{notice}</div>}
      </div>
      {bothArchives && (
        <button className="link" title="Compare the two archive files byte by byte" onClick={() => onOpenHex(left, right)}>
          <Binary size={13} /> Compare as files
        </button>
      )}
    </div>
  );
}
