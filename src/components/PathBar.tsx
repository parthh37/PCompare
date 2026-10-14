import type { DialogFilter } from "@tauri-apps/plugin-dialog";
import { ArrowLeftRight } from "lucide-react";
import { useEffect, useState } from "react";
import { rememberPath } from "../lib/recentPaths";
import { PathField } from "./PathField";

interface Props {
  left: string;
  right: string;
  directory: boolean;
  onCompare: (left: string, right: string) => void;
  filters?: DialogFilter[];
  /** Report a single chosen path too (the other side stays ""), e.g. to list one folder. */
  oneSided?: boolean;
}

/** The two path inputs across the top of every 2-way session, with browse + swap. */
export function PathBar({ left, right, directory, onCompare, filters, oneSided }: Props) {
  const [l, setL] = useState(left);
  const [r, setR] = useState(right);
  useEffect(() => setL(left), [left]);
  useEffect(() => setR(right), [right]);
  // The props are what the session actually compares (typed, picked, or opened from
  // Home/CLI), so they're what goes into the recent-path history.
  const kind = directory ? "folder" : "file";
  useEffect(() => void rememberPath(kind, left), [kind, left]);
  useEffect(() => void rememberPath(kind, right), [kind, right]);

  const apply = (picked: string, isLeft: boolean) => {
    const other = isLeft ? r : l;
    (isLeft ? setL : setR)(picked);
    if (other || oneSided) onCompare(isLeft ? picked : other, isLeft ? other : picked);
  };

  const submit = () => (oneSided ? l || r : l && r) && onCompare(l, r);
  const what = directory ? "folder" : "file";

  return (
    <div className="pathbar">
      <PathField
        value={l}
        onChange={setL}
        onSubmit={submit}
        onPick={(p) => apply(p, true)}
        directory={directory}
        filters={filters}
        placeholder={`Left ${what}… or remote://profile/path`}
      />
      <button className="icon-btn swap" title="Swap sides" onClick={() => (oneSided ? l || r : l && r) && onCompare(r, l)}>
        <ArrowLeftRight size={15} />
      </button>
      <PathField
        value={r}
        onChange={setR}
        onSubmit={submit}
        onPick={(p) => apply(p, false)}
        directory={directory}
        filters={filters}
        placeholder={`Right ${what}… or remote://profile/path`}
      />
    </div>
  );
}
