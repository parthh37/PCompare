import type { DialogFilter } from "@tauri-apps/plugin-dialog";
import { Cpu } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { errorMessage } from "../lib/api";
import { basename } from "../lib/format";
import { metaApi, type VersionCompareResult, type VersionSummary } from "../lib/metaApi";
import type { CompareProps } from "../lib/sessionProps";
import { PathBar } from "./PathBar";
import { PropertyCompare, type PropertyFilter } from "./PropertyCompare";

/** What a saved version session remembers besides its paths. */
export interface VersionSettings {
  filter: PropertyFilter;
}

const FILE_FILTERS: DialogFilter[] = [
  { name: "Executables", extensions: ["exe", "dll", "sys", "ocx", "cpl", "scr", "drv", "efi", "mui"] },
  { name: "All files", extensions: ["*"] },
];

function Headline({ s, other }: { s: VersionSummary; other: VersionSummary }) {
  if (!s.fileVersion && !s.productVersion) {
    return (
      <div className="muted">
        <Cpu size={13} /> No version resource{s.architecture ? ` · ${s.architecture}` : ""}
      </div>
    );
  }
  const differs = s.fileVersion !== other.fileVersion;
  return (
    <div title={s.description ?? undefined}>
      <Cpu size={13} className="muted" />
      <span className={`headline${differs ? " differs" : ""}`}>{s.fileVersion ?? s.productVersion}</span>
      {s.productVersion && s.productVersion !== s.fileVersion && (
        <span className="muted">product {s.productVersion}</span>
      )}
      {s.architecture && <span className="muted">· {s.architecture}</span>}
      {s.description && <span className="muted">· {s.description}</span>}
    </div>
  );
}

export function VersionCompare({ left, right, onPathsChange, settings, onSettingsChange }: CompareProps<VersionSettings>) {
  const [filter, setFilter] = useState<PropertyFilter>(settings?.filter ?? "all");
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(() => reportSettings.current?.({ filter }), [filter]);

  const [result, setResult] = useState<VersionCompareResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const run = useCallback(async () => {
    if (!left || !right) return;
    setLoading(true);
    setError(null);
    try {
      setResult(await metaApi.compareVersions(left, right));
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

  return (
    <PropertyCompare
      result={result}
      loading={loading}
      loadingText="Reading version information…"
      error={error}
      filter={filter}
      onFilterChange={setFilter}
      onRefresh={run}
      pathBar={<PathBar left={left} right={right} directory={false} onCompare={onPathsChange} filters={FILE_FILTERS} />}
      header={
        result && (
          <div className="meta-info">
            <Headline s={result.left} other={result.right} />
            <Headline s={result.right} other={result.left} />
          </div>
        )
      }
      leftTitle={basename(left) || "Left"}
      rightTitle={basename(right) || "Right"}
      emptyText="Choose two executables (.exe, .dll, .sys…) to compare their version information."
    />
  );
}
