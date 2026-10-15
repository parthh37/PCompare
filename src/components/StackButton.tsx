import { ChevronDown } from "lucide-react";
import type { ButtonHTMLAttributes, MouseEvent, ReactNode } from "react";

type Props = { icon: ReactNode; label: ReactNode; active?: boolean } & ButtonHTMLAttributes<HTMLButtonElement>;

/** A viewer toolbar button: icon above a short label, flat until hovered, sunken while pressed. */
export function StackButton({ icon, label, active, className, ...rest }: Props) {
  return (
    <button {...rest} className={`stack-btn${active ? " active" : ""}${className ? ` ${className}` : ""}`} aria-pressed={active}>
      {icon}
      <span>{label}</span>
    </button>
  );
}

/**
 * A {@link StackButton} with a ▾ beside its label. With `onClick` the button
 * acts and only the ▾ opens the menu (a split button); without, the whole
 * button opens it. `onMenu` gets the point under the button to open it at.
 */
export function StackMenuButton({
  onMenu,
  onClick,
  label,
  ...rest
}: Omit<Props, "onClick"> & { onClick?: () => void; onMenu: (at: { clientX: number; clientY: number }) => void }) {
  const openBelow = (ev: MouseEvent<HTMLElement>) => {
    ev.stopPropagation();
    const box = (ev.currentTarget.closest(".stack-btn") ?? ev.currentTarget).getBoundingClientRect();
    onMenu({ clientX: box.left, clientY: box.bottom + 2 });
  };
  return (
    <StackButton
      {...rest}
      aria-haspopup="menu"
      onClick={onClick ? () => onClick() : openBelow}
      label={
        <>
          {label}
          <span className="stack-drop" onClick={onClick ? openBelow : undefined} title={onClick ? "More choices" : undefined}>
            <ChevronDown size={10} />
          </span>
        </>
      }
    />
  );
}
