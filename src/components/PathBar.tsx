import { open } from "@tauri-apps/plugin-dialog";
import { useEffect, useState } from "react";

interface Props {
  left: string;
  right: string;
  directory: boolean;
  onCompare: (left: string, right: string) => void;
}

/** The two path inputs across the top of every session, with browse + swap. */
export function PathBar({ left, right, directory, onCompare }: Props) {
  const [l, setL] = useState(left);
  const [r, setR] = useState(right);
  useEffect(() => setL(left), [left]);
  useEffect(() => setR(right), [right]);

  const browse = async (set: (v: string) => void, other: string, isLeft: boolean) => {
    const picked = await open({ directory, multiple: false, title: directory ? "Choose folder" : "Choose file" });
    if (typeof picked === "string") {
      set(picked);
      if (other) onCompare(isLeft ? picked : other, isLeft ? other : picked);
    }
  };

  const submit = () => l && r && onCompare(l, r);

  return (
    <div className="pathbar">
      <div className="pathfield">
        <input
          value={l}
          placeholder={directory ? "Left folder…" : "Left file…"}
          onChange={(e) => setL(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && submit()}
          spellCheck={false}
        />
        <button className="icon-btn" title="Browse…" onClick={() => browse(setL, r, true)}>
          📂
        </button>
      </div>
      <button className="icon-btn swap" title="Swap sides" onClick={() => l && r && onCompare(r, l)}>
        ⇄
      </button>
      <div className="pathfield">
        <input
          value={r}
          placeholder={directory ? "Right folder…" : "Right file…"}
          onChange={(e) => setR(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && submit()}
          spellCheck={false}
        />
        <button className="icon-btn" title="Browse…" onClick={() => browse(setR, l, false)}>
          📂
        </button>
      </div>
    </div>
  );
}
