// Picture Compare side tools: per-side transform controls, the pixel loupe
// and the "replaced colours" rule editor. Styles live in PictureCompare.css.
import {
  ArrowLeftRight,
  ArrowRight,
  ArrowUpDown,
  Pin,
  PinOff,
  Pipette,
  Plus,
  RotateCcw,
  RotateCw,
  Trash,
  X,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import {
  IDENTITY_TRANSFORM,
  type ColorRule,
  type PictureTransform,
  type PixelMatch,
  type PixelProbe,
  type Rgba,
  type ScaleMode,
} from "../lib/pictureApi";
import { Modal } from "./Modal";

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

// ---------- colours ----------

const hex2 = (n: number) => n.toString(16).padStart(2, "0");

/** `#rrggbb`, or `#rrggbbaa` when not fully opaque. */
export function rgbaHex(c: Rgba): string {
  return `#${hex2(c[0])}${hex2(c[1])}${hex2(c[2])}${c[3] === 255 ? "" : hex2(c[3])}`;
}

/** Parses `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa` (the `#` is optional). */
export function parseHexColor(text: string): Rgba | null {
  let h = text.trim().replace(/^#/, "");
  if (!/^[0-9a-f]+$/i.test(h) || ![3, 4, 6, 8].includes(h.length)) return null;
  if (h.length <= 4) h = [...h].map((c) => c + c).join("");
  if (h.length === 6) h += "ff";
  const n = (i: number) => parseInt(h.slice(i, i + 2), 16);
  return [n(0), n(2), n(4), n(6)];
}

const cssColor = (c: Rgba) => `rgba(${c[0]}, ${c[1]}, ${c[2]}, ${c[3] / 255})`;

/** Colour swatch over a checkerboard, so transparency shows. */
function Swatch({ color }: { color: Rgba | null }) {
  return (
    <span className="pic-swatch" title={color ? rgbaHex(color) : "No pixel"}>
      <span style={{ background: color ? cssColor(color) : undefined }} className={color ? "" : "none"} />
    </span>
  );
}

/** A colour picker plus a hex field for one RGBA colour. */
function ColorField({ value, onChange, label }: { value: Rgba; onChange: (c: Rgba) => void; label: string }) {
  const [text, setText] = useState(rgbaHex(value));
  useEffect(() => setText(rgbaHex(value)), [value]);
  const parsed = parseHexColor(text);
  const commit = () => (parsed ? onChange(parsed) : setText(rgbaHex(value)));
  return (
    <span className="pic-colorfield">
      <input
        type="color"
        aria-label={`${label} colour`}
        value={rgbaHex([value[0], value[1], value[2], 255])}
        onChange={(e) => {
          const c = parseHexColor(e.target.value);
          if (c) onChange([c[0], c[1], c[2], value[3]]);
        }}
      />
      <input
        className={`pic-hex${parsed ? "" : " invalid"}`}
        aria-label={`${label} hex`}
        value={text}
        spellCheck={false}
        onChange={(e) => setText(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => e.key === "Enter" && commit()}
        title="#rrggbb or #rrggbbaa"
      />
    </span>
  );
}

// ---------- transforms ----------

const SCALE_PRESETS = [25, 50, 75, 150, 200, 300, 400];

export function isIdentity(t: PictureTransform): boolean {
  return (
    t.rotate === 0 &&
    !t.flipHorizontal &&
    !t.flipVertical &&
    (t.scale === "none" || (t.scale === "percent" && t.scalePercent === 100))
  );
}

/** Coerces a saved transform into a valid one. */
export function restoreTransform(t: Partial<PictureTransform> | undefined): PictureTransform {
  const rotate = [0, 90, 180, 270].includes(t?.rotate as number) ? (t!.rotate as PictureTransform["rotate"]) : 0;
  const scale: ScaleMode = t?.scale === "matchOther" || t?.scale === "percent" ? t.scale : "none";
  const pct = typeof t?.scalePercent === "number" && Number.isFinite(t.scalePercent) ? t.scalePercent : 100;
  return {
    rotate,
    flipHorizontal: t?.flipHorizontal === true,
    flipVertical: t?.flipVertical === true,
    scale,
    scalePercent: clamp(Math.round(pct), 1, 1000),
  };
}

/** Rotates what is displayed by `deg` clockwise. Rotation is applied before
 * the flips, so with a single flip active the stored rotation turns the other way. */
function rotateBy(t: PictureTransform, deg: number): PictureTransform {
  const mirrored = t.flipHorizontal !== t.flipVertical;
  const rotate = ((((t.rotate + (mirrored ? -deg : deg)) % 360) + 360) % 360) as PictureTransform["rotate"];
  return { ...t, rotate };
}

/** Rotate / flip / scale buttons for one side. */
export function TransformControls({
  side,
  value,
  onChange,
}: {
  side: "left" | "right";
  value: PictureTransform;
  onChange: (t: PictureTransform) => void;
}) {
  const other = side === "left" ? "right" : "left";
  const scaleKey = value.scale === "percent" ? `p${value.scalePercent}` : value.scale;
  const presets =
    SCALE_PRESETS.includes(value.scalePercent) || value.scale !== "percent"
      ? SCALE_PRESETS
      : [...SCALE_PRESETS, value.scalePercent].sort((a, b) => a - b);
  const [custom, setCustom] = useState(String(value.scalePercent));
  useEffect(() => setCustom(String(value.scalePercent)), [value.scalePercent]);
  const commitCustom = () => {
    const n = Number(custom);
    if (Number.isFinite(n) && n >= 1)
      onChange({ ...value, scale: "percent", scalePercent: clamp(Math.round(n), 1, 1000) });
    else setCustom(String(value.scalePercent));
  };
  const toggle = (on: boolean) => (on ? " active-toggle" : "");
  return (
    <span className="pic-transform">
      <button
        className="icon-btn"
        onClick={() => onChange(rotateBy(value, -90))}
        title={`Rotate the ${side} image 90° counter-clockwise`}
      >
        <RotateCcw size={13} />
      </button>
      <button
        className="icon-btn"
        onClick={() => onChange(rotateBy(value, 90))}
        title={`Rotate the ${side} image 90° clockwise`}
      >
        <RotateCw size={13} />
      </button>
      <button
        className={`icon-btn${toggle(value.flipHorizontal)}`}
        onClick={() => onChange({ ...value, flipHorizontal: !value.flipHorizontal })}
        title={`Flip the ${side} image horizontally`}
      >
        <ArrowLeftRight size={13} />
      </button>
      <button
        className={`icon-btn${toggle(value.flipVertical)}`}
        onClick={() => onChange({ ...value, flipVertical: !value.flipVertical })}
        title={`Flip the ${side} image vertically`}
      >
        <ArrowUpDown size={13} />
      </button>
      <select
        className={value.scale === "none" ? "" : "active"}
        value={scaleKey}
        title={`Scale the ${side} image before comparing`}
        onChange={(e) => {
          const v = e.target.value;
          if (v === "none" || v === "matchOther") onChange({ ...value, scale: v });
          else if (v === "custom") onChange({ ...value, scale: "percent" });
          else onChange({ ...value, scale: "percent", scalePercent: Number(v.slice(1)) });
        }}
      >
        <option value="none">Original size</option>
        <option value="matchOther">Match {other} size</option>
        {presets.map((p) => (
          <option key={p} value={`p${p}`}>
            {p}%
          </option>
        ))}
        {value.scale === "percent" && !presets.includes(value.scalePercent) && (
          <option value={scaleKey}>{value.scalePercent}%</option>
        )}
        <option value="custom">Custom %…</option>
      </select>
      {value.scale === "percent" && (
        <input
          type="number"
          className="pic-num"
          min={1}
          max={1000}
          value={custom}
          aria-label="Scale percent"
          onChange={(e) => setCustom(e.target.value)}
          onBlur={commitCustom}
          onKeyDown={(e) => e.key === "Enter" && commitCustom()}
        />
      )}
      {!isIdentity(value) && (
        <button className="icon-btn" onClick={() => onChange(IDENTITY_TRANSFORM)} title={`Undo all ${side} transforms`}>
          <X size={13} />
        </button>
      )}
    </span>
  );
}

// ---------- loupe ----------

/** Screen pixels per loupe cell. */
const CELL = 11;
const MATCH_LABEL: Record<PixelMatch, string> = { same: "Same", replaced: "Replaced colour", different: "Different" };

function drawGrid(canvas: HTMLCanvasElement, cells: (Rgba | null)[], matches: (PixelMatch | null)[], size: number) {
  const px = size * CELL;
  canvas.width = px;
  canvas.height = px;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  const styles = getComputedStyle(canvas);
  const checkerA = styles.getPropertyValue("--checker-a").trim() || "#2a2f3a";
  const checkerB = styles.getPropertyValue("--checker-b").trim() || "#3a414e";
  const empty = styles.getPropertyValue("--bg").trim() || "#14171d";
  const half = Math.ceil(CELL / 2);
  cells.forEach((c, i) => {
    const x = (i % size) * CELL;
    const y = Math.floor(i / size) * CELL;
    if (!c) {
      ctx.fillStyle = empty;
      ctx.fillRect(x, y, CELL, CELL);
      ctx.strokeStyle = checkerB;
      ctx.beginPath();
      ctx.moveTo(x, y + CELL);
      ctx.lineTo(x + CELL, y);
      ctx.stroke();
      return;
    }
    if (c[3] < 255) {
      ctx.fillStyle = checkerA;
      ctx.fillRect(x, y, CELL, CELL);
      ctx.fillStyle = checkerB;
      ctx.fillRect(x, y, half, half);
      ctx.fillRect(x + half, y + half, CELL - half, CELL - half);
    }
    ctx.fillStyle = cssColor(c);
    ctx.fillRect(x, y, CELL, CELL);
    if (matches[i] === "different") {
      ctx.strokeStyle = "rgb(255, 32, 96)";
      ctx.lineWidth = 1;
      ctx.strokeRect(x + 1.5, y + 1.5, CELL - 3, CELL - 3);
    }
  });
  // Centre cell: dark and light rings so it shows on any colour.
  const c = Math.floor(size / 2) * CELL;
  ctx.lineWidth = 1;
  ctx.strokeStyle = "#000";
  ctx.strokeRect(c - 0.5, c - 0.5, CELL + 1, CELL + 1);
  ctx.strokeStyle = "#fff";
  ctx.strokeRect(c - 1.5, c - 1.5, CELL + 3, CELL + 3);
}

function LoupeGrid({ cells, matches, size }: { cells: (Rgba | null)[]; matches: (PixelMatch | null)[]; size: number }) {
  const ref = useRef<HTMLCanvasElement>(null);
  useEffect(() => {
    if (ref.current) drawGrid(ref.current, cells, matches, size);
  }, [cells, matches, size]);
  return <canvas ref={ref} className="pic-loupe-grid" style={{ width: size * CELL, height: size * CELL }} />;
}

function PixelValue({ label, c }: { label: string; c: Rgba | null }) {
  return (
    <div className="pic-loupe-value">
      <span className="pic-loupe-side">{label}</span>
      <Swatch color={c} />
      {c ? (
        <>
          <span className="mono">
            R {c[0]} G {c[1]} B {c[2]} A {c[3]}
          </span>
          <span className="mono muted">{rgbaHex(c)}</span>
        </>
      ) : (
        <span className="muted">outside the image</span>
      )}
    </div>
  );
}

/** Centre pixels of a probe, for colour rules. */
export function probeCenter(p: PixelProbe | null): { left: Rgba | null; right: Rgba | null } {
  if (!p) return { left: null, right: null };
  const i = Math.floor((p.size * p.size) / 2);
  return { left: p.left[i], right: p.right[i] };
}

/** Magnified pixels around the cursor on both sides, with the centre pixel's RGBA values. */
export function Loupe({
  probe,
  pinned,
  onTogglePin,
  onAddRule,
  onClose,
}: {
  probe: PixelProbe | null;
  pinned: boolean;
  onTogglePin: () => void;
  onAddRule: (left: Rgba, right: Rgba) => void;
  onClose: () => void;
}) {
  const center = probeCenter(probe);
  const match = probe ? probe.matches[Math.floor((probe.size * probe.size) / 2)] : null;
  return (
    <div className="pic-loupe" onPointerDown={(e) => e.stopPropagation()}>
      <div className="pic-loupe-head">
        <span>{probe ? `Pixel ${probe.x}, ${probe.y}` : "Loupe"}</span>
        {match && <span className={`pic-loupe-match ${match}`}>{MATCH_LABEL[match]}</span>}
        <span className="spacer" />
        <button
          className={`icon-btn${pinned ? " active-toggle" : ""}`}
          onClick={onTogglePin}
          disabled={!probe && !pinned}
          title={pinned ? "Follow the mouse again" : "Keep showing this pixel (or Alt+click a pixel)"}
        >
          {pinned ? <PinOff size={13} /> : <Pin size={13} />}
        </button>
        <button
          className="icon-btn"
          disabled={!center.left || !center.right || match === "same"}
          onClick={() => center.left && center.right && onAddRule(center.left, center.right)}
          title="Ignore this colour replacement: treat the left colour turning into the right colour as equal"
        >
          <Pipette size={13} />
        </button>
        <button className="icon-btn" onClick={onClose} title="Hide the loupe">
          <X size={13} />
        </button>
      </div>
      {probe ? (
        <>
          <div className="pic-loupe-grids">
            <LoupeGrid cells={probe.left} matches={probe.matches} size={probe.size} />
            <LoupeGrid cells={probe.right} matches={probe.matches} size={probe.size} />
          </div>
          <PixelValue label="Left" c={center.left} />
          <PixelValue label="Right" c={center.right} />
        </>
      ) : (
        <div className="pic-loupe-empty muted">Move the mouse over the image. Alt+click pins a pixel.</div>
      )}
    </div>
  );
}

// ---------- replaced colours ----------

/** A colour rule as the session stores it; disabled rules are kept but not applied. */
export interface ReplacedColor extends ColorRule {
  enabled: boolean;
}

export function restoreRules(v: unknown): ReplacedColor[] {
  if (!Array.isArray(v)) return [];
  const color = (c: unknown): Rgba | null =>
    Array.isArray(c) && c.length === 4 && c.every((n) => typeof n === "number")
      ? (c.map((n) => clamp(Math.round(n), 0, 255)) as Rgba)
      : null;
  return v.flatMap((r) => {
    const left = color(r?.left);
    const right = color(r?.right);
    if (!left || !right) return [];
    const tolerance = typeof r.tolerance === "number" ? clamp(Math.round(r.tolerance), 0, 255) : 0;
    return [{ left, right, tolerance, enabled: r.enabled !== false }];
  });
}

/** Lists and edits the colour-replacement rules. */
export function ReplacedColorsDialog({
  rules,
  onChange,
  loupe,
  onClose,
}: {
  rules: ReplacedColor[];
  onChange: (rules: ReplacedColor[]) => void;
  /** Centre pixels of the loupe, offered as a quick way to fill in a rule. */
  loupe: { left: Rgba | null; right: Rgba | null };
  onClose: () => void;
}) {
  const update = (i: number, patch: Partial<ReplacedColor>) =>
    onChange(rules.map((r, j) => (i === j ? { ...r, ...patch } : r)));
  const fromLoupe = loupe.left && loupe.right ? { left: loupe.left, right: loupe.right } : null;
  return (
    <Modal
      title="Ignore replaced colours"
      onClose={onClose}
      wide
      footer={
        <>
          <button
            onClick={() =>
              onChange([
                ...rules,
                { left: [255, 255, 255, 255], right: [255, 255, 255, 255], tolerance: 0, enabled: true },
              ])
            }
          >
            <Plus size={14} /> Add rule
          </button>
          <button
            disabled={!fromLoupe}
            onClick={() => fromLoupe && onChange([...rules, { ...fromLoupe, tolerance: 0, enabled: true }])}
            title="Add a rule from the loupe's centre pixels"
          >
            <Pipette size={14} /> Add from loupe
          </button>
          <span className="spacer" />
          <button className="primary" onClick={onClose}>
            Done
          </button>
        </>
      }
    >
      <p className="muted pic-rules-help">
        A left pixel matching the left colour that faces a right pixel matching the right colour counts as equal. The
        tolerance is the largest per-channel distance from each rule colour (0–255). Use the loupe's pipette, or
        Alt+click a pixel, to pick colours from the images.
      </p>
      {rules.length === 0 ? (
        <div className="empty-state">No rules yet.</div>
      ) : (
        <div className="pic-rules">
          {rules.map((r, i) => (
            <div key={i} className={`pic-rule${r.enabled ? "" : " disabled"}`}>
              <input
                type="checkbox"
                checked={r.enabled}
                onChange={(e) => update(i, { enabled: e.target.checked })}
                title="Apply this rule"
              />
              <ColorField label="Left" value={r.left} onChange={(left) => update(i, { left })} />
              <ArrowRight size={14} className="muted" />
              <ColorField label="Right" value={r.right} onChange={(right) => update(i, { right })} />
              <label title="Largest per-channel distance from the rule colours still matching">
                ±
                <input
                  type="number"
                  className="pic-num"
                  min={0}
                  max={255}
                  value={r.tolerance}
                  onChange={(e) => {
                    const v = Number(e.target.value);
                    if (Number.isFinite(v)) update(i, { tolerance: clamp(Math.round(v), 0, 255) });
                  }}
                />
              </label>
              <button
                className="icon-btn"
                disabled={!fromLoupe}
                onClick={() => fromLoupe && update(i, fromLoupe)}
                title="Use the loupe's centre pixels"
              >
                <Pipette size={13} />
              </button>
              <button
                className="icon-btn"
                onClick={() => onChange(rules.filter((_, j) => j !== i))}
                title="Remove rule"
              >
                <Trash size={13} />
              </button>
            </div>
          ))}
        </div>
      )}
    </Modal>
  );
}
