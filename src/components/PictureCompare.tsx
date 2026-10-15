import {
  Columns2,
  Diff,
  Layers,
  Link2,
  Link2Off,
  LocateFixed,
  Maximize,
  RefreshCw,
  Replace,
  ScanEye,
  SquareSplitHorizontal,
  ZoomIn,
  ZoomOut,
  type LucideIcon,
} from "lucide-react";
import { embedInactive } from "../lib/embeddedPane";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type PointerEvent as ReactPointerEvent,
  type ReactNode,
} from "react";
import { errorMessage } from "../lib/api";
import { formatSize } from "../lib/format";
import {
  IDENTITY_TRANSFORM,
  pictureApi,
  type DiffOptions,
  type PairTransform,
  type PictureComparison,
  type PictureInfo,
  type PictureSources,
  type PictureTransform,
  type PixelProbe,
  type Rgba,
} from "../lib/pictureApi";
import type { CompareProps } from "../lib/sessionProps";
import { PathBar } from "./PathBar";
import {
  Loupe,
  probeCenter,
  ReplacedColorsDialog,
  restoreRules,
  restoreTransform,
  TransformControls,
  type ReplacedColor,
} from "./PictureTools";
import "./PictureCompare.css";

type ViewMode = "side" | "difference" | "overlay" | "swipe";
type Zoom = number | "fit";

/** What a saved picture session remembers besides its paths. */
export interface PictureSettings {
  mode: ViewMode;
  /** Tolerance for every channel while `linkChannels` is on, 0–255. */
  tolerance: number;
  /** Separate R, G, B, A tolerances, used while `linkChannels` is off. */
  channelTolerance: Rgba;
  linkChannels: boolean;
  /** Paint the difference mask over the images. */
  highlight: boolean;
  /** Opacity of the right image in overlay mode, 0–1. */
  opacity: number;
  /** Rotation / flips / scaling of each side, applied before comparing. */
  transforms: PairTransform;
  /** "Ignore replaced colours" rules. */
  replacedColors: ReplacedColor[];
  /** Show the pixel loupe. */
  loupe: boolean;
}

const MODES: { id: ViewMode; label: string; icon: LucideIcon }[] = [
  { id: "side", label: "Side by side", icon: Columns2 },
  { id: "difference", label: "Difference", icon: Diff },
  { id: "overlay", label: "Overlay", icon: Layers },
  { id: "swipe", label: "Swipe", icon: SquareSplitHorizontal },
];

const IMAGE_FILTERS = [
  {
    name: "Images",
    extensions: ["png", "jpg", "jpeg", "gif", "bmp", "ico", "tif", "tiff", "webp", "tga", "pbm", "pgm", "ppm", "pnm", "pam", "qoi"],
  },
  { name: "All files", extensions: ["*"] },
];

const ZOOM_STEPS = [0.02, 0.05, 0.1, 0.125, 0.25, 1 / 3, 0.5, 2 / 3, 1, 1.5, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64];
const MIN_ZOOM = ZOOM_STEPS[0];
const MAX_ZOOM = ZOOM_STEPS[ZOOM_STEPS.length - 1];
/** Largest zoom "Jump to difference" will pick, so a single pixel keeps some context. */
const JUMP_MAX_ZOOM = 16;
/** Space around the canvas inside each pane (matches `.pic-stage` padding). */
const PAD = 12;
const TOLERANCE_DEBOUNCE_MS = 250;
/** The loupe shows (2·r+1)² pixels: 11 × 11. */
const LOUPE_RADIUS = 5;
const CHANNELS = ["R", "G", "B", "A"] as const;

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

function restore(s: Partial<PictureSettings> | undefined): PictureSettings {
  const num = (v: unknown, fallback: number) => (typeof v === "number" && Number.isFinite(v) ? v : fallback);
  const tolerance = clamp(Math.round(num(s?.tolerance, 0)), 0, 255);
  const channels = Array.isArray(s?.channelTolerance) && s.channelTolerance.length === 4 ? s.channelTolerance : null;
  return {
    mode: MODES.some((m) => m.id === s?.mode) ? s!.mode! : "side",
    tolerance,
    channelTolerance: channels
      ? (channels.map((v) => clamp(Math.round(num(v, tolerance)), 0, 255)) as Rgba)
      : [tolerance, tolerance, tolerance, tolerance],
    linkChannels: typeof s?.linkChannels === "boolean" ? s.linkChannels : true,
    highlight: typeof s?.highlight === "boolean" ? s.highlight : true,
    opacity: clamp(num(s?.opacity, 0.5), 0, 1),
    transforms: { left: restoreTransform(s?.transforms?.left), right: restoreTransform(s?.transforms?.right) },
    replacedColors: restoreRules(s?.replacedColors),
    loupe: s?.loupe === true,
  };
}

function formatTolerance(t: Rgba): string {
  return t.every((v) => v === t[0]) ? String(t[0]) : CHANNELS.map((c, i) => `${c} ${t[i]}`).join(" ");
}

/** Zoom that shows the whole canvas: whole multiples when upscaling keep pixels square. */
function fitZoom(cw: number, ch: number, pw: number, ph: number): number {
  const z = Math.min((pw - 2 * PAD) / cw, (ph - 2 * PAD) / ch);
  if (!Number.isFinite(z) || z <= 0) return 1;
  return z >= 1 ? Math.min(Math.floor(z), MAX_ZOOM) : Math.max(z, MIN_ZOOM);
}

const formatZoom = (z: number) => `${z < 0.1 ? (z * 100).toFixed(1) : Math.round(z * 100)}%`;

function formatPercent(p: number): string {
  if (p > 0 && p < 0.01) return "<0.01%";
  return `${p.toFixed(2)}%`;
}

function Info({ info }: { info: PictureInfo }) {
  const resized = info.width !== info.sourceWidth || info.height !== info.sourceHeight;
  return (
    <span className="fileinfo">
      {info.width} × {info.height}
      {resized && (
        <span className="muted">
          {" "}
          (from {info.sourceWidth} × {info.sourceHeight})
        </span>
      )}{" "}
      · {info.format} · {info.colorType} {info.bitDepth}-bit · {formatSize(info.fileSize)}
    </span>
  );
}

export function PictureCompare({ left, right, onPathsChange, settings, onSettingsChange }: CompareProps<PictureSettings>) {
  const [initial] = useState(() => restore(settings));
  const [mode, setMode] = useState<ViewMode>(initial.mode);
  const [tolerance, setTolerance] = useState(initial.tolerance);
  const [channelTolerance, setChannelTolerance] = useState<Rgba>(initial.channelTolerance);
  const [linkChannels, setLinkChannels] = useState(initial.linkChannels);
  const [highlight, setHighlight] = useState(initial.highlight);
  const [opacity, setOpacity] = useState(initial.opacity);
  const [transforms, setTransforms] = useState<PairTransform>(initial.transforms);
  const [replacedColors, setReplacedColors] = useState<ReplacedColor[]>(initial.replacedColors);
  const [loupe, setLoupe] = useState(initial.loupe);
  const reportSettings = useRef(onSettingsChange);
  reportSettings.current = onSettingsChange;
  useEffect(
    () =>
      reportSettings.current?.({
        mode,
        tolerance,
        channelTolerance,
        linkChannels,
        highlight,
        opacity,
        transforms,
        replacedColors,
        loupe,
      }),
    [mode, tolerance, channelTolerance, linkChannels, highlight, opacity, transforms, replacedColors, loupe],
  );

  // Everything the diff depends on, debounced as one JSON key so sliders don't flood the engine.
  const optionsJson = JSON.stringify({
    tolerance: linkChannels ? [tolerance, tolerance, tolerance, tolerance] : channelTolerance,
    replacedColors: replacedColors.filter((r) => r.enabled).map(({ left, right, tolerance }) => ({ left, right, tolerance })),
  } satisfies DiffOptions);
  const [debouncedJson, setDebouncedJson] = useState(optionsJson);
  useEffect(() => {
    const t = setTimeout(() => setDebouncedJson(optionsJson), TOLERANCE_DEBOUNCE_MS);
    return () => clearTimeout(t);
  }, [optionsJson]);
  const diffOptions = useMemo(() => JSON.parse(debouncedJson) as DiffOptions, [debouncedJson]);
  const transformsJson = JSON.stringify(transforms);
  const tx = useMemo(() => JSON.parse(transformsJson) as PairTransform, [transformsJson]);

  const setChannel = (i: number, v: number) =>
    setChannelTolerance((t) => t.map((x, j) => (j === i ? clamp(Math.round(v), 0, 255) : x)) as Rgba);
  const toggleLink = () => {
    if (linkChannels) setChannelTolerance([tolerance, tolerance, tolerance, tolerance]);
    else setTolerance(Math.max(...channelTolerance));
    setLinkChannels(!linkChannels);
  };
  const setSideTransform = (side: "left" | "right", t: PictureTransform) =>
    setTransforms((prev) => ({ ...prev, [side]: t }));

  const [sources, setSources] = useState<PictureSources | null>(null);
  const [comparison, setComparison] = useState<PictureComparison | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [diffing, setDiffing] = useState(false);
  const [reloadKey, setReloadKey] = useState(0);
  const [zoom, setZoom] = useState<Zoom>("fit");
  const loadSeq = useRef(0);
  const diffSeq = useRef(0);
  /** What the current `sources` were loaded for: files + refresh count, and transforms. */
  const loadedFor = useRef({ files: "", transforms: "" });
  /** Transforms the shown `sources` were produced with (the diff must match them). */
  const sourcesTx = useRef<PairTransform>({ left: IDENTITY_TRANSFORM, right: IDENTITY_TRANSFORM });

  // New paths start from a clean slate; a refresh keeps the old view until the new one arrives.
  useEffect(() => {
    setSources(null);
    setComparison(null);
    setZoom("fit");
  }, [left, right]);

  useEffect(() => {
    const id = ++loadSeq.current;
    setError(null);
    if (!left || !right) {
      setLoading(false);
      return;
    }
    setLoading(true);
    // Only a new file pair or a refresh re-reads the files; a transform change reuses the decoded images.
    const files = `${left}\n${right}\n${reloadKey}`;
    const reload = loadedFor.current.files !== files;
    const retransformed = loadedFor.current.transforms !== transformsJson;
    loadedFor.current = { files, transforms: transformsJson };
    pictureApi
      .load(left, right, tx, reload)
      .then((s) => {
        if (id !== loadSeq.current) return;
        sourcesTx.current = tx;
        // The old mask no longer lines up with re-oriented images.
        if (retransformed) setComparison(null);
        setSources(s);
      })
      .catch((e) => {
        if (id !== loadSeq.current) return;
        // Let the next attempt (e.g. after undoing a bad transform) re-read the files.
        loadedFor.current = { files: "", transforms: "" };
        setError(errorMessage(e));
        setSources(null);
        setComparison(null);
      })
      .finally(() => id === loadSeq.current && setLoading(false));
  }, [left, right, reloadKey, tx, transformsJson]);

  useEffect(() => {
    const id = ++diffSeq.current;
    if (!sources) {
      setDiffing(false);
      return;
    }
    setDiffing(true);
    pictureApi
      .diff(left, right, sourcesTx.current, diffOptions)
      .then((c) => id === diffSeq.current && setComparison(c))
      .catch((e) => id === diffSeq.current && setError(errorMessage(e)))
      .finally(() => id === diffSeq.current && setDiffing(false));
  }, [sources, left, right, diffOptions]);

  const reload = useCallback(() => setReloadKey((k) => k + 1), []);

  // ---------- loupe ----------

  const [probe, setProbe] = useState<PixelProbe | null>(null);
  const [pinned, setPinned] = useState(false);
  const [rulesOpen, setRulesOpen] = useState(false);
  const probeCtx = useRef({ left, right, tx: sourcesTx.current, options: diffOptions });
  probeCtx.current = { left, right, tx: sourcesTx.current, options: diffOptions };
  const pendingProbe = useRef<{ x: number; y: number } | null>(null);
  const lastProbe = useRef<{ x: number; y: number } | null>(null);
  const probing = useRef(false);

  // At most one probe in flight; while it runs only the latest position is remembered.
  const runProbe = useCallback(() => {
    const at = pendingProbe.current;
    if (probing.current || !at) return;
    pendingProbe.current = null;
    probing.current = true;
    const c = probeCtx.current;
    pictureApi
      .probe(c.left, c.right, c.tx, c.options, at.x, at.y, LOUPE_RADIUS)
      .then(setProbe)
      // The loupe is advisory: on failure (e.g. a file vanished) it just shows nothing.
      .catch(() => setProbe(null))
      .finally(() => {
        probing.current = false;
        runProbe();
      });
  }, []);
  const requestProbe = useCallback(
    (x: number, y: number) => {
      lastProbe.current = { x, y };
      pendingProbe.current = { x, y };
      runProbe();
    },
    [runProbe],
  );

  // New images or options change what the loupe shows at the same spot.
  useEffect(() => {
    if (!sources) {
      setProbe(null);
      setPinned(false);
      lastProbe.current = null;
    } else if (loupe && lastProbe.current) {
      const { x, y } = lastProbe.current;
      if (x < sources.canvasWidth && y < sources.canvasHeight) requestProbe(x, y);
      else setProbe(null);
    }
  }, [sources, diffOptions, loupe, requestProbe]);

  const addRule = useCallback((l: Rgba, r: Rgba) => {
    setReplacedColors((rules) => [...rules, { left: l, right: r, tolerance: 0, enabled: true }]);
    setRulesOpen(true);
  }, []);

  // ---------- zoom & pan ----------

  const [paneSize, setPaneSize] = useState({ w: 0, h: 0 });
  const [panning, setPanning] = useState(false);
  const [, bump] = useState(0);
  const root = useRef<HTMLDivElement>(null);
  const panes = useRef<(HTMLDivElement | null)[]>([]);
  const pan = useRef<{ x: number; y: number; left: number; top: number } | null>(null);
  /** Canvas point (full-res px) to keep under a pane offset after the next render. */
  const anchor = useRef<{ x: number; y: number; offX: number; offY: number } | null>(null);

  const cw = sources?.canvasWidth ?? 1;
  const ch = sources?.canvasHeight ?? 1;
  const fit = fitZoom(cw, ch, paneSize.w, paneSize.h);
  const z = zoom === "fit" ? fit : zoom;
  const zRef = useRef(z);
  zRef.current = z;

  const setPane = useCallback((i: number, el: HTMLDivElement | null) => {
    panes.current[i] = el;
  }, []);

  useEffect(() => {
    const pane = panes.current[0];
    if (!pane) return;
    const ro = new ResizeObserver(() => setPaneSize({ w: pane.clientWidth, h: pane.clientHeight }));
    ro.observe(pane);
    return () => ro.disconnect();
  }, [sources !== null]);

  const syncFrom = useCallback((src: HTMLDivElement) => {
    for (const p of panes.current) {
      if (!p || p === src) continue;
      if (p.scrollLeft !== src.scrollLeft) p.scrollLeft = src.scrollLeft;
      if (p.scrollTop !== src.scrollTop) p.scrollTop = src.scrollTop;
    }
  }, []);

  const canvasOf = (pane: HTMLElement) => pane.querySelector<HTMLElement>(".pic-canvas");

  /** Changes zoom keeping the canvas point under (clientX, clientY) of `pane` fixed (default: pane centre). */
  const zoomTo = useCallback((next: Zoom, at?: { pane: HTMLElement; clientX: number; clientY: number }) => {
    const pane = at?.pane ?? panes.current[0];
    const canvas = pane && canvasOf(pane);
    if (next !== "fit" && pane && canvas) {
      const pr = pane.getBoundingClientRect();
      const cr = canvas.getBoundingClientRect();
      const clientX = at?.clientX ?? pr.left + pane.clientWidth / 2;
      const clientY = at?.clientY ?? pr.top + pane.clientHeight / 2;
      anchor.current = {
        x: (clientX - cr.left) / zRef.current,
        y: (clientY - cr.top) / zRef.current,
        offX: clientX - pr.left,
        offY: clientY - pr.top,
      };
    }
    setZoom(next === "fit" ? next : clamp(next, MIN_ZOOM, MAX_ZOOM));
    bump((n) => n + 1);
  }, []);

  const zoomStep = useCallback(
    (dir: 1 | -1) => {
      const cur = zRef.current;
      const next =
        dir > 0 ? ZOOM_STEPS.find((s) => s > cur * 1.001) : [...ZOOM_STEPS].reverse().find((s) => s < cur * 0.999);
      if (next !== undefined) zoomTo(next);
    },
    [zoomTo],
  );

  // Apply a pending zoom anchor once the new size is laid out, then line up the other panes.
  useLayoutEffect(() => {
    const a = anchor.current;
    const pane = panes.current[0];
    if (!a || !pane) return;
    anchor.current = null;
    const canvas = canvasOf(pane);
    if (!canvas) return;
    const cr = canvas.getBoundingClientRect();
    const pr = pane.getBoundingClientRect();
    pane.scrollLeft += cr.left + a.x * z - pr.left - a.offX;
    pane.scrollTop += cr.top + a.y * z - pr.top - a.offY;
    syncFrom(pane);
  });

  // Ctrl/Cmd + wheel (and trackpad pinch) zooms around the pointer; plain wheel scrolls natively.
  useEffect(() => {
    const onWheel = (e: WheelEvent) => {
      if (!e.ctrlKey && !e.metaKey) return;
      e.preventDefault();
      const factor = clamp(Math.exp(-e.deltaY * 0.0025), 0.5, 2);
      zoomTo(zRef.current * factor, { pane: e.currentTarget as HTMLElement, clientX: e.clientX, clientY: e.clientY });
    };
    const els = panes.current.filter((p): p is HTMLDivElement => !!p);
    els.forEach((p) => p.addEventListener("wheel", onWheel, { passive: false }));
    return () => els.forEach((p) => p.removeEventListener("wheel", onWheel));
  }, [mode, sources !== null, zoomTo]);

  const jumpToDifference = useCallback(() => {
    const b = comparison?.stats.bounds;
    const pane = panes.current[0];
    if (!b || !pane) return;
    const next = clamp(
      Math.min(((pane.clientWidth - 2 * PAD) * 0.8) / b.width, ((pane.clientHeight - 2 * PAD) * 0.8) / b.height),
      MIN_ZOOM,
      JUMP_MAX_ZOOM,
    );
    anchor.current = {
      x: b.x + b.width / 2,
      y: b.y + b.height / 2,
      offX: pane.clientWidth / 2,
      offY: pane.clientHeight / 2,
    };
    setZoom(next);
    bump((n) => n + 1);
  }, [comparison]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Inactive tabs stay mounted but hidden; only the visible one reacts.
      if (!root.current?.offsetParent) return;
      if (embedInactive(root.current)) return; // split view: the folder list has the keys
      const mod = e.ctrlKey || e.metaKey;
      if (mod && (e.key === "=" || e.key === "+")) zoomStep(1);
      else if (mod && e.key === "-") zoomStep(-1);
      else if (mod && e.key === "0") zoomTo("fit");
      else if (e.key === "F5") reload();
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [zoomStep, zoomTo, reload]);

  /** Full-resolution canvas pixel under the pointer, or null outside the canvas. */
  const pixelAt = (e: ReactPointerEvent<HTMLDivElement>): { x: number; y: number } | null => {
    const canvas = canvasOf(e.currentTarget);
    if (!canvas) return null;
    const r = canvas.getBoundingClientRect();
    const x = Math.floor((e.clientX - r.left) / zRef.current);
    const y = Math.floor((e.clientY - r.top) / zRef.current);
    return x >= 0 && y >= 0 && x < cw && y < ch ? { x, y } : null;
  };
  const trackLoupe = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (!loupe || pinned || pan.current) return;
    const at = pixelAt(e);
    if (at) requestProbe(at.x, at.y);
  };

  const onPanStart = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (e.button !== 0 && e.button !== 1) return;
    const pane = e.currentTarget;
    const r = pane.getBoundingClientRect();
    // Leave presses on the pane's own scrollbars alone.
    if (e.clientX - r.left >= pane.clientWidth || e.clientY - r.top >= pane.clientHeight) return;
    // Alt+click pins the loupe on a pixel (the eyedropper for colour rules).
    if (e.altKey && e.button === 0) {
      const at = pixelAt(e);
      if (at) {
        setLoupe(true);
        setPinned(true);
        requestProbe(at.x, at.y);
      }
      e.preventDefault();
      return;
    }
    pan.current = { x: e.clientX, y: e.clientY, left: pane.scrollLeft, top: pane.scrollTop };
    pane.setPointerCapture(e.pointerId);
    setPanning(true);
    e.preventDefault();
  };
  const onPanMove = (e: ReactPointerEvent<HTMLDivElement>) => {
    trackLoupe(e);
    const p = pan.current;
    if (!p) return;
    e.currentTarget.scrollLeft = p.left - (e.clientX - p.x);
    e.currentTarget.scrollTop = p.top - (e.clientY - p.y);
  };
  const onPanEnd = () => {
    pan.current = null;
    setPanning(false);
  };

  // ---------- rendering ----------

  const [swipe, setSwipe] = useState(0.5);
  const stats = comparison?.stats;
  const hasDiffs = !!stats && stats.differingPixels > 0;
  const displayScale = sources?.displayScale ?? 1;
  const pixelated = z / displayScale > 1;

  const image = (src: string, info: PictureInfo, opts: { checker?: boolean; style?: CSSProperties } = {}) => (
    <img
      src={src}
      alt=""
      draggable={false}
      className={`pic-img${opts.checker === false ? "" : " checker"}`}
      style={{ width: info.width * z, height: info.height * z, ...opts.style }}
    />
  );
  const mask = (force = false) =>
    comparison && hasDiffs && (highlight || force) ? (
      <MaskLayer
        src={comparison.mask}
        naturalWidth={Math.max(1, Math.round(cw * displayScale))}
        width={cw * z}
        height={ch * z}
      />
    ) : null;

  const probeMark =
    loupe && probe && probe.x < cw && probe.y < ch ? (
      <div
        className="pic-probe-mark"
        style={{
          left: probe.x * z - 1,
          top: probe.y * z - 1,
          width: Math.max(z, 1) + 2,
          height: Math.max(z, 1) + 2,
        }}
      />
    ) : null;

  const paneContent = (i: number): ReactNode => {
    if (!sources) return null;
    switch (mode) {
      case "side":
        return (
          <>
            {i === 0 ? image(sources.leftImage, sources.left) : image(sources.rightImage, sources.right)}
            {mask()}
          </>
        );
      case "difference":
        return comparison ? (
          <>
            <img
              src={comparison.difference}
              alt=""
              draggable={false}
              className="pic-img checker"
              style={{ width: cw * z, height: ch * z }}
            />
            {mask(true)}
          </>
        ) : null;
      case "overlay":
        return (
          <>
            {image(sources.leftImage, sources.left)}
            {image(sources.rightImage, sources.right, { checker: false, style: { opacity } })}
            {mask()}
          </>
        );
      case "swipe": {
        const x = swipe * cw * z;
        return (
          <>
            {image(sources.leftImage, sources.left)}
            {image(sources.rightImage, sources.right, { style: { clipPath: `inset(0 0 0 ${x}px)` } })}
            {mask()}
            <SwipeDivider x={x} onChange={setSwipe} />
          </>
        );
      }
    }
  };

  const paneCount = mode === "side" ? 2 : 1;
  const busy = loading || (diffing && !comparison);

  return (
    <div className="session picture-compare" ref={root}>
      <div className="toolbar">
        <div className="seg-control">
          {MODES.map(({ id, label, icon: Icon }) => (
            <button key={id} className={mode === id ? "active" : ""} onClick={() => setMode(id)} title={label}>
              <Icon size={14} /> {label}
            </button>
          ))}
        </div>
        <span className="sep" />
        {linkChannels ? (
          <label title="Largest difference (0–255) on any channel still treated as equal">
            Tolerance
            <input
              type="range"
              min={0}
              max={255}
              value={tolerance}
              onChange={(e) => setTolerance(Number(e.target.value))}
            />
            <input
              type="number"
              className="pic-num"
              min={0}
              max={255}
              value={tolerance}
              onChange={(e) => {
                const v = Number(e.target.value);
                if (Number.isFinite(v)) setTolerance(clamp(Math.round(v), 0, 255));
              }}
            />
          </label>
        ) : (
          <span className="pic-channels" title="Largest difference (0–255) per channel still treated as equal">
            <span className="muted">Tolerance</span>
            {CHANNELS.map((c, i) => (
              <label key={c} className={`pic-chan pic-chan-${c}`}>
                {c}
                <input
                  type="number"
                  className="pic-num"
                  min={0}
                  max={255}
                  value={channelTolerance[i]}
                  aria-label={`${c} tolerance`}
                  onChange={(e) => {
                    const v = Number(e.target.value);
                    if (Number.isFinite(v)) setChannel(i, v);
                  }}
                />
              </label>
            ))}
          </span>
        )}
        <button
          className={`icon-btn${linkChannels ? "" : " active-toggle"}`}
          onClick={toggleLink}
          title={linkChannels ? "Set a separate tolerance for R, G, B and A" : "Use one tolerance for all channels"}
        >
          {linkChannels ? <Link2 size={14} /> : <Link2Off size={14} />}
        </button>
        <button
          className={replacedColors.some((r) => r.enabled) ? "active-toggle" : ""}
          onClick={() => setRulesOpen(true)}
          title="Ignore replaced colours: colour pairs whose pixels count as equal"
        >
          <Replace size={14} /> Colors
          {replacedColors.length > 0 && (
            <span className="pic-badge">{replacedColors.filter((r) => r.enabled).length}</span>
          )}
        </button>
        <label className="check" title="Paint differing pixels over the images">
          <input type="checkbox" checked={highlight} onChange={(e) => setHighlight(e.target.checked)} />
          Highlight
        </label>
        {mode === "overlay" && (
          <label title="Opacity of the right image">
            Opacity
            <input
              type="range"
              min={0}
              max={100}
              value={Math.round(opacity * 100)}
              onChange={(e) => setOpacity(Number(e.target.value) / 100)}
            />
            <span className="pic-value">{Math.round(opacity * 100)}%</span>
          </label>
        )}
        <span className="sep" />
        <button className="icon-btn" onClick={() => zoomStep(-1)} disabled={!sources} title="Zoom out (Ctrl+-)">
          <ZoomOut size={14} />
        </button>
        <span className="pic-value pic-zoom">{sources ? formatZoom(z) : "–"}</span>
        <button className="icon-btn" onClick={() => zoomStep(1)} disabled={!sources} title="Zoom in (Ctrl+=)">
          <ZoomIn size={14} />
        </button>
        <button
          className={zoom === "fit" ? "active-toggle" : ""}
          onClick={() => zoomTo("fit")}
          disabled={!sources}
          title="Fit to window (Ctrl+0)"
        >
          <Maximize size={14} /> Fit
        </button>
        <button className={zoom === 1 ? "active-toggle" : ""} onClick={() => zoomTo(1)} disabled={!sources} title="Actual size">
          100%
        </button>
        <button onClick={jumpToDifference} disabled={!stats?.bounds} title="Zoom to the area containing differences">
          <LocateFixed size={14} /> Jump to difference
        </button>
        <button
          className={loupe ? "active-toggle" : ""}
          onClick={() => {
            setLoupe(!loupe);
            setPinned(false);
          }}
          disabled={!sources}
          title="Loupe: magnified pixels and RGBA values under the mouse (Alt+click pins a pixel)"
        >
          <ScanEye size={14} /> Loupe
        </button>
        <span className="spacer" />
        <button onClick={reload} disabled={!left || !right} title="Refresh (F5)">
          <RefreshCw size={14} /> Refresh
        </button>
      </div>

      <PathBar left={left} right={right} directory={false} onCompare={onPathsChange} filters={IMAGE_FILTERS} />

      {left && right && (
        <div className="infobar pic-sides">
          {(["left", "right"] as const).map((side) => (
            <div key={side} className="pic-side">
              <TransformControls
                side={side}
                value={transforms[side]}
                onChange={(t) => setSideTransform(side, t)}
              />
              {sources && <Info info={sources[side]} />}
            </div>
          ))}
        </div>
      )}

      <div className="session-body">
        {busy && <div className="overlay">{loading ? "Loading images…" : "Comparing…"}</div>}
        {loupe && sources && (
          <Loupe
            probe={probe}
            pinned={pinned}
            onTogglePin={() => setPinned((p) => !p)}
            onAddRule={addRule}
            onClose={() => {
              setLoupe(false);
              setPinned(false);
            }}
          />
        )}
        {error && <div className="banner error">{error}</div>}
        {!left || !right ? (
          <div className="empty-state">Choose two images to compare.</div>
        ) : (
          sources && (
            <div className={`pic-views${panning ? " panning" : ""}`}>
              {Array.from({ length: paneCount }, (_, i) => (
                <div
                  key={i}
                  ref={(el) => setPane(i, el)}
                  className="pic-pane"
                  onScroll={(e) => syncFrom(e.currentTarget)}
                  onPointerDown={onPanStart}
                  onPointerMove={onPanMove}
                  onPointerUp={onPanEnd}
                  onPointerCancel={onPanEnd}
                >
                  <div className="pic-stage">
                    <div
                      className={`pic-canvas${pixelated ? " pixelated" : ""}`}
                      style={{ width: cw * z, height: ch * z }}
                    >
                      {paneContent(i)}
                      {probeMark}
                    </div>
                  </div>
                </div>
              ))}
            </div>
          )
        )}
      </div>

      {rulesOpen && (
        <ReplacedColorsDialog
          rules={replacedColors}
          onChange={setReplacedColors}
          loupe={probeCenter(probe)}
          onClose={() => setRulesOpen(false)}
        />
      )}

      <div className="statusbar">
        {stats && sources && (
          <>
            {stats.identical && stats.maxDelta === 0 ? (
              <span className="ok">Images are identical</span>
            ) : stats.identical ? (
              <span className="ok">No differences above tolerance {formatTolerance(stats.tolerance)}</span>
            ) : (
              <span className="s-different-fg">
                {stats.differingPixels.toLocaleString()} differing pixel{stats.differingPixels === 1 ? "" : "s"} (
                {formatPercent(stats.percent)})
              </span>
            )}
            {stats.maxDelta > 0 && <span>Max delta {stats.maxDelta}</span>}
            {stats.replacedPixels > 0 && (
              <span title="Pixels a replaced-colour rule declared equal">
                {stats.replacedPixels.toLocaleString()} replaced-colour pixel{stats.replacedPixels === 1 ? "" : "s"}{" "}
                ignored
              </span>
            )}
            {stats.sizeMismatch && (
              <span className="pic-warn">
                Size mismatch: {sources.left.width} × {sources.left.height} vs {sources.right.width} ×{" "}
                {sources.right.height}
              </span>
            )}
            {stats.bounds && (
              <span>
                Differences at {stats.bounds.x}, {stats.bounds.y} · {stats.bounds.width} × {stats.bounds.height}
              </span>
            )}
          </>
        )}
        <span className="spacer" />
        {sources && displayScale < 1 && (
          <span className="muted" title="Statistics are computed at full resolution">
            Preview at {Math.round(displayScale * 100)}% resolution
          </span>
        )}
        {diffing && comparison && <span className="muted">Comparing…</span>}
      </div>
    </div>
  );
}

/** The draggable vertical line in swipe mode; left of it shows the left image, right of it the right image. */
function SwipeDivider({ x, onChange }: { x: number; onChange: (fraction: number) => void }) {
  const dragging = useRef(false);
  const update = (e: ReactPointerEvent<HTMLDivElement>) => {
    const canvas = e.currentTarget.parentElement;
    if (!canvas) return;
    const r = canvas.getBoundingClientRect();
    onChange(clamp((e.clientX - r.left) / r.width, 0, 1));
  };
  return (
    <div
      className="pic-swipe"
      style={{ left: x }}
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.stopPropagation();
        e.preventDefault();
        e.currentTarget.setPointerCapture(e.pointerId);
        dragging.current = true;
      }}
      onPointerMove={(e) => {
        if (!dragging.current) return;
        e.stopPropagation();
        update(e);
      }}
      onPointerUp={() => (dragging.current = false)}
      onPointerCancel={() => (dragging.current = false)}
    >
      <span className="pic-swipe-grip" />
    </div>
  );
}

/** Alpha channel of a mask image plus the colour its marked pixels use. */
interface MaskData {
  alpha: Uint8Array;
  width: number;
  height: number;
  color: [number, number, number, number];
}

async function readMask(src: string): Promise<MaskData> {
  const img = new Image();
  img.src = src;
  await img.decode();
  const { naturalWidth: width, naturalHeight: height } = img;
  const canvas = document.createElement("canvas");
  canvas.width = width;
  canvas.height = height;
  const ctx = canvas.getContext("2d");
  if (!ctx) throw new Error("2D canvas unavailable");
  ctx.drawImage(img, 0, 0);
  const rgba = ctx.getImageData(0, 0, width, height).data;
  const alpha = new Uint8Array(width * height);
  let color: MaskData["color"] = [255, 32, 96, 255];
  let found = false;
  for (let i = 0; i < alpha.length; i++) {
    const a = rgba[i * 4 + 3];
    alpha[i] = a;
    if (a && !found) {
      found = true;
      color = [rgba[i * 4], rgba[i * 4 + 1], rgba[i * 4 + 2], a];
    }
  }
  return { alpha, width, height, color };
}

/**
 * Difference mask drawn over the canvas. At or above one screen pixel per mask
 * pixel the PNG is shown directly; when zoomed out further the mask is
 * max-pooled on a canvas so isolated differing pixels stay visible instead of
 * being averaged away by the browser's downscaling.
 */
function MaskLayer({
  src,
  naturalWidth,
  width,
  height,
}: {
  src: string;
  /** Pixel width of the mask PNG. */
  naturalWidth: number;
  /** CSS size of the canvas it covers. */
  width: number;
  height: number;
}) {
  const [loaded, setLoaded] = useState<{ src: string; mask: MaskData } | null>(null);
  const data = loaded?.src === src ? loaded.mask : null;
  const canvas = useRef<HTMLCanvasElement>(null);

  // Screen pixels per mask pixel; below 1 several mask pixels share a screen pixel.
  const density = width / naturalWidth;
  const block = density >= 1 ? 1 : Math.ceil(1 / density);

  useEffect(() => {
    if (block === 1 || data) return;
    let cancelled = false;
    readMask(src)
      .then((mask) => !cancelled && setLoaded({ src, mask }))
      .catch(() => {
        // Keep showing the plain image; only the pooling enhancement is lost.
      });
    return () => {
      cancelled = true;
    };
  }, [src, block, data]);

  useEffect(() => {
    const el = canvas.current;
    if (!el || !data || block === 1) return;
    const ow = Math.ceil(data.width / block);
    const oh = Math.ceil(data.height / block);
    el.width = ow;
    el.height = oh;
    const ctx = el.getContext("2d");
    if (!ctx) return;
    const out = ctx.createImageData(ow, oh);
    const { alpha, width: w, height: h, color } = data;
    for (let y = 0; y < h; y++) {
      const row = y * w;
      const outRow = Math.floor(y / block) * ow;
      for (let x = 0; x < w; x++) {
        if (alpha[row + x]) out.data.set(color, (outRow + Math.floor(x / block)) * 4);
      }
    }
    ctx.putImageData(out, 0, 0);
  }, [data, block]);

  if (block > 1 && data) {
    // The pooled canvas covers whole blocks, so it may extend slightly past the mask's edge.
    const scale = width / data.width;
    return (
      <canvas
        ref={canvas}
        className="pic-mask"
        style={{
          width: Math.ceil(data.width / block) * block * scale,
          height: Math.ceil(data.height / block) * block * scale,
        }}
      />
    );
  }
  return <img src={src} alt="" draggable={false} className="pic-mask" style={{ width, height }} />;
}
