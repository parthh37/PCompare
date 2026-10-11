import { open } from "@tauri-apps/plugin-dialog";
import { useEffect, useState } from "react";
import { useShell } from "../lib/shell";
import { RemoteBrowser } from "./RemoteBrowser";

interface Props {
  left: string;
  right: string;
  directory: boolean;
  onCompare: (left: string, right: string) => void;
}

/** The two path inputs across the top of every session, with browse + swap. */
export function PathBar({ left, right, directory, onCompare }: Props) {
  const shell = useShell();
  const [l, setL] = useState(left);
  const [r, setR] = useState(right);
  const [remoteSide, setRemoteSide] = useState<"left" | "right" | null>(null);
  useEffect(() => setL(left), [left]);
  useEffect(() => setR(right), [right]);

  const apply = (picked: string, isLeft: boolean) => {
    const other = isLeft ? r : l;
    (isLeft ? setL : setR)(picked);
    if (other) onCompare(isLeft ? picked : other, isLeft ? other : picked);
  };

  const browse = async (isLeft: boolean) => {
    const picked = await open({ directory, multiple: false, title: directory ? "Choose folder" : "Choose file" });
    if (typeof picked === "string") apply(picked, isLeft);
  };

  const submit = () => l && r && onCompare(l, r);

  return (
    <div className="pathbar">
      <div className="pathfield">
        <input
          value={l}
          placeholder={directory ? "Left folder… or remote://profile/path" : "Left file… or remote://profile/path"}
          onChange={(e) => setL(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && submit()}
          spellCheck={false}
        />
        <button className="icon-btn" title="Browse…" onClick={() => browse(true)}>
          📂
        </button>
        <button className="icon-btn" title="Browse remote (S3, SFTP, OneDrive…)" onClick={() => setRemoteSide("left")}>
          🌐
        </button>
      </div>
      <button className="icon-btn swap" title="Swap sides" onClick={() => l && r && onCompare(r, l)}>
        ⇄
      </button>
      <div className="pathfield">
        <input
          value={r}
          placeholder={directory ? "Right folder… or remote://profile/path" : "Right file… or remote://profile/path"}
          onChange={(e) => setR(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && submit()}
          spellCheck={false}
        />
        <button className="icon-btn" title="Browse…" onClick={() => browse(false)}>
          📂
        </button>
        <button className="icon-btn" title="Browse remote (S3, SFTP, OneDrive…)" onClick={() => setRemoteSide("right")}>
          🌐
        </button>
      </div>
      {remoteSide && (
        <RemoteBrowser
          directory={directory}
          initial={remoteSide === "left" ? l : r}
          onClose={() => setRemoteSide(null)}
          onManageProfiles={() => {
            setRemoteSide(null);
            shell.openProfiles();
          }}
          onPick={(p) => {
            apply(p, remoteSide === "left");
            setRemoteSide(null);
          }}
        />
      )}
    </div>
  );
}
