import type { DialogFilter } from "@tauri-apps/plugin-dialog";
import { Image, Music } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { errorMessage } from "../lib/api";
import { basename, formatSize } from "../lib/format";
import { metaApi, type AudioCompareResult, type AudioSummary, type PictureInfo } from "../lib/metaApi";
import type { CompareProps } from "../lib/sessionProps";
import { PathBar } from "./PathBar";
import { PropertyCompare, type PropertyFilter } from "./PropertyCompare";
import "./Mp3Compare.css";

/** What a saved MP3 session remembers besides its paths. */
export interface Mp3Settings {
  filter: PropertyFilter;
}

const FILE_FILTERS: DialogFilter[] = [
  {
    name: "Audio files",
    extensions: ["mp3", "flac", "m4a", "m4b", "mp4", "aac", "ogg", "oga", "opus", "spx", "wav", "aiff", "aif", "ape", "wv", "mpc"],
  },
  { name: "All files", extensions: ["*"] },
];

function formatDuration(ms: number): string {
  const total = Math.round(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = String(total % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${s}` : `${m}:${s}`;
}

function Headline({ s }: { s: AudioSummary }) {
  const title = [s.title, s.artist].filter(Boolean).join(" — ");
  return (
    <div title={title || undefined}>
      <Music size={13} className="muted" />
      {title ? <span className="headline">{title}</span> : <span className="muted">Untitled</span>}
      <span className="muted">
        · {formatDuration(s.durationMs)}
        {s.bitrateKbps ? ` · ${s.bitrateKbps} kbps` : ""} · {s.format}
      </span>
    </div>
  );
}

function Covers({ pictures }: { pictures: PictureInfo[] }) {
  if (pictures.length === 0) return <div className="mp3-covers-side muted">No embedded pictures</div>;
  return (
    <div className="mp3-covers-side">
      {pictures.map((p, i) => (
        <figure key={i} className="mp3-cover">
          {p.dataUrl ? (
            <img src={p.dataUrl} alt={p.kind} />
          ) : (
            <div className="mp3-cover-placeholder" title="Too large or unsupported format for a preview">
              <Image size={22} />
            </div>
          )}
          <figcaption>
            {p.kind}
            <span className="muted">
              {p.width && p.height ? ` · ${p.width}×${p.height}` : ""} · {formatSize(p.size)}
            </span>
          </figcaption>
        </figure>
      ))}
    </div>
  );
}

export function Mp3Compare({ left, right, onPathsChange, settings, onSettingsChange }: CompareProps<Mp3Settings>) {
  const [filter, setFilter] = useState<PropertyFilter>(settings?.filter ?? "all");
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => void reportSettings.current?.({ filter }), [filter]);

  const [result, setResult] = useState<AudioCompareResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const run = useCallback(async () => {
    if (!left || !right) return;
    setLoading(true);
    setError(null);
    try {
      setResult(await metaApi.compareAudio(left, right));
    } catch (e) {
      setError(errorMessage(e));
      setResult(null);
    } finally {
      setLoading(false);
    }
  }, [left, right]);

  useEffect(() => {
    run();
  }, [run]);

  const hasPictures = !!result && result.leftPictures.length + result.rightPictures.length > 0;
  return (
    <PropertyCompare
      result={result}
      loading={loading}
      loadingText="Reading tags…"
      error={error}
      filter={filter}
      onFilterChange={setFilter}
      onRefresh={run}
      pathBar={<PathBar left={left} right={right} directory={false} onCompare={onPathsChange} filters={FILE_FILTERS} />}
      header={
        result && (
          <>
            <div className="meta-info">
              <Headline s={result.left} />
              <Headline s={result.right} />
            </div>
            {hasPictures && (
              <div className="mp3-covers">
                <Covers pictures={result.leftPictures} />
                <Covers pictures={result.rightPictures} />
              </div>
            )}
          </>
        )
      }
      leftTitle={basename(left) || "Left"}
      rightTitle={basename(right) || "Right"}
      emptyText="Choose two audio files (MP3, FLAC, M4A, Ogg, WAV…) to compare their tags."
    />
  );
}
