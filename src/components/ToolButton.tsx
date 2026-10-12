import type { ReactNode } from "react";
import { ToolIcon, type ToolIconName } from "./icons";

interface Props {
  icon: ToolIconName;
  label: ReactNode;
  onClick: () => void;
  title?: string;
  /** Pressed state, for filters and toggles. */
  active?: boolean;
  disabled?: boolean;
}

/** A Beyond Compare–style toolbar button: a large colour icon above a short label. */
export function ToolButton({ icon, label, onClick, title, active, disabled }: Props) {
  return (
    <button
      className={`tool-btn${active ? " active" : ""}`}
      onClick={onClick}
      title={title}
      disabled={disabled}
      aria-pressed={active}
    >
      <ToolIcon name={icon} />
      <span>{label}</span>
    </button>
  );
}
