import { SESSIONS, type SessionKind } from "../lib/sessions";

interface Props {
  kind: SessionKind;
  /** Icon size in px; the tile is drawn around it. */
  size?: number;
  /** `tile` draws a tinted rounded square behind the glyph (cards, lists). */
  tile?: boolean;
}

/** The icon for a session type, tinted with that session's accent colour. */
export function SessionIcon({ kind, size = 14, tile = false }: Props) {
  const { icon: Icon, color } = SESSIONS[kind];
  const glyph = <Icon size={size} color={color} strokeWidth={tile ? 1.75 : 2} aria-hidden />;
  if (!tile) return glyph;
  return (
    <span
      className="session-tile"
      style={{ background: `${color}1f`, borderColor: `${color}40`, width: size * 2, height: size * 2 }}
    >
      {glyph}
    </span>
  );
}
