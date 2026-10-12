import { Component, type ReactNode } from "react";
import { errorMessage } from "../lib/api";

interface Props {
  children: ReactNode;
  /** What to show instead of the crashed subtree. */
  fallback: (message: string) => ReactNode;
}

/**
 * Keeps one crashing view from taking the whole window down: without a
 * boundary React unmounts everything on an uncaught error, leaving a blank
 * window that only a reload brings back.
 */
export class ErrorBoundary extends Component<Props, { message: string | null }> {
  state = { message: null as string | null };

  static getDerivedStateFromError(e: unknown) {
    return { message: errorMessage(e) || "Unknown error" };
  }

  componentDidCatch(e: unknown) {
    console.error(e);
  }

  render() {
    return this.state.message === null ? this.props.children : this.props.fallback(this.state.message);
  }
}

/** Shown in place of a crashed view, with a way out that doesn't need a reload. */
export function CrashedView({ message, action, onAction }: { message: string; action: string; onAction: () => void }) {
  return (
    <div className="empty-state crashed-view">
      <p>This view stopped working.</p>
      <pre>{message}</pre>
      <button className="primary" onClick={onAction}>
        {action}
      </button>
    </div>
  );
}
