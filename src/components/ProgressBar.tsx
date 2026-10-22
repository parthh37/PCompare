import type { ReactNode } from "react";
import "./ProgressBar.css";

type Props = {
  /** Work done so far. Leave out (or null) while the total isn't known: the bar then runs indeterminate. */
  value?: number | null;
  /** The whole amount of work; 0, null or left out means unknown. */
  max?: number | null;
  /** "slim" (3 px) for status bars and overlays; "normal" (6 px) for progress cards. */
  size?: "slim" | "normal";
  /** Paused or waiting to retry: the fill turns amber and stops moving. */
  tone?: "accent" | "warn";
  /** Read out by screen readers. */
  label: string;
  className?: string;
};

/**
 * The one progress bar every long-running activity shows. With both `value`
 * and `max` it fills in proportion; without a known total a short stripe
 * slides along it, so a scan that can't know its size still visibly runs.
 */
export function ProgressBar({ value, max, size = "normal", tone = "accent", label, className }: Props) {
  const known = max != null && max > 0 && value != null;
  const fraction = known ? Math.min(1, Math.max(0, value / max)) : 0;
  return (
    <div
      role="progressbar"
      aria-label={label}
      aria-valuemin={known ? 0 : undefined}
      aria-valuemax={known ? max : undefined}
      aria-valuenow={known ? Math.min(value, max) : undefined}
      className={`pbar pbar-${size}${known ? "" : " pbar-indeterminate"}${tone === "warn" ? " pbar-warn" : ""}${
        className ? ` ${className}` : ""
      }`}
    >
      <div className="pbar-fill" style={known ? { width: `${fraction * 100}%` } : undefined} />
    </div>
  );
}

/** "Doing X…" with a small running bar, for status bars and inline waits. */
export function BusyStatus({ label, className }: { label: string; className?: string }) {
  return (
    <span className={`live-progress${className ? ` ${className}` : ""}`}>
      {label}
      <ProgressBar size="slim" label={label} />
    </span>
  );
}

/**
 * Covers a viewer while it loads or compares: what it's doing, a progress
 * bar, and optionally a line under it (a count, a Cancel button).
 */
export function BusyOverlay({
  label,
  value,
  max,
  children,
}: {
  label: ReactNode;
  value?: number | null;
  max?: number | null;
  children?: ReactNode;
}) {
  return (
    <div className="overlay busy-overlay" aria-live="polite">
      <div className="busy-card">
        <span>{label}</span>
        <ProgressBar value={value} max={max} size="slim" label={typeof label === "string" ? label : "Working"} />
        {children}
      </div>
    </div>
  );
}
