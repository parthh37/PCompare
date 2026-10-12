import type { DialogFilter } from "@tauri-apps/plugin-dialog";
import { ArrowLeftRight } from "lucide-react";
import { useEffect, useState } from "react";
import { PathField } from "./PathField";

interface Props {
  left: string;
  right: string;
  directory: boolean;
  onCompare: (left: string, right: string) => void;
  filters?: DialogFilter[];
}

/** The two path inputs across the top of every 2-way session, with browse + swap. */
export function PathBar({ left, right, directory, onCompare, filters }: Props) {
  const [l, setL] = useState(left);
  const [r, setR] = useState(right);
  useEffect(() => setL(left), [left]);
  useEffect(() => setR(right), [right]);

  const apply = (picked: string, isLeft: boolean) => {
    const other = isLeft ? r : l;
    (isLeft ? setL : setR)(picked);
    if (other) onCompare(isLeft ? picked : other, isLeft ? other : picked);
  };

  const submit = () => l && r && onCompare(l, r);
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
      <button className="icon-btn swap" title="Swap sides" onClick={() => l && r && onCompare(r, l)}>
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
