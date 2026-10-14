import { X } from "lucide-react";
import { useEffect, useRef, type ReactNode } from "react";

interface Props {
  title: string;
  onClose: () => void;
  children: ReactNode;
  footer?: ReactNode;
  wide?: boolean;
  /** automation:ui — extra class on the dialog box (custom sizes). */
  className?: string;
}

// automation:ui — open dialogs, innermost last: Escape closes only the innermost one
// (a dialog opened from another one, e.g. "Manage channels…" in the task editor).
const openModals: object[] = [];

/** A centered dialog over a dimmed backdrop; Escape or a backdrop click closes it. */
export function Modal({ title, onClose, children, footer, wide, className }: Props) {
  const self = useRef({});
  useEffect(() => {
    const me = self.current;
    openModals.push(me);
    return () => void openModals.splice(openModals.indexOf(me), 1);
  }, []);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && openModals[openModals.length - 1] === self.current) {
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  return (
    <div className="modal-backdrop" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className={`modal${wide ? " wide" : ""}${className ? ` ${className}` : ""}`} role="dialog" aria-label={title}>
        <header className="modal-head">
          <span>{title}</span>
          <button className="tab-close" onClick={onClose} aria-label="Close">
            <X size={14} />
          </button>
        </header>
        <div className="modal-body">{children}</div>
        {footer && <footer className="modal-foot">{footer}</footer>}
      </div>
    </div>
  );
}
