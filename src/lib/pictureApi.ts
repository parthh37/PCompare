// Typed wrappers for the picture compare commands (src-tauri/src/picture.rs).
import { invoke } from "@tauri-apps/api/core";

export interface PictureInfo {
  width: number;
  height: number;
  /** Container format, e.g. "PNG". */
  format: string;
  /** Channel layout as stored, e.g. "RGBA", "Grayscale". */
  colorType: string;
  bitDepth: number;
  hasAlpha: boolean;
  fileSize: number;
  /** Size as decoded, before rotation/scaling (`width`/`height` are after). */
  sourceWidth: number;
  sourceHeight: number;
}

export type ScaleMode = "none" | "matchOther" | "percent";

/** Applied to one side before comparing: rotate (clockwise), then flip, then scale. */
export interface PictureTransform {
  rotate: 0 | 90 | 180 | 270;
  flipHorizontal: boolean;
  flipVertical: boolean;
  scale: ScaleMode;
  /** Used when `scale` is "percent" (1–1000). */
  scalePercent: number;
}

export interface PairTransform {
  left: PictureTransform;
  right: PictureTransform;
}

export const IDENTITY_TRANSFORM: PictureTransform = {
  rotate: 0,
  flipHorizontal: false,
  flipVertical: false,
  scale: "none",
  scalePercent: 100,
};

export type Rgba = [number, number, number, number];

/** A left pixel near `left` facing a right pixel near `right` counts as equal. */
export interface ColorRule {
  left: Rgba;
  right: Rgba;
  /** Per-channel distance from the rule's colours still matching (0–255). */
  tolerance: number;
}

export interface DiffOptions {
  /** Per-channel tolerance: R, G, B, A (0–255). */
  tolerance: Rgba;
  replacedColors: ColorRule[];
}

export type PixelMatch = "same" | "replaced" | "different";

/** Full-resolution pixels around a canvas position, row-major; null outside an image. */
export interface PixelProbe {
  x: number;
  y: number;
  /** Window side length (odd). */
  size: number;
  left: (Rgba | null)[];
  right: (Rgba | null)[];
  matches: (PixelMatch | null)[];
}

export interface PictureSources {
  left: PictureInfo;
  right: PictureInfo;
  /** PNG data URLs, downscaled by `displayScale`. */
  leftImage: string;
  rightImage: string;
  /** Full-resolution size of the canvas holding both images (top-left anchored). */
  canvasWidth: number;
  canvasHeight: number;
  displayScale: number;
}

export interface PixelRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface PictureDiffStats {
  identical: boolean;
  differingPixels: number;
  totalPixels: number;
  percent: number;
  /** Largest per-channel delta in the overlap, regardless of tolerance. */
  maxDelta: number;
  bounds: PixelRect | null;
  sizeMismatch: boolean;
  /** Per-channel tolerance used: R, G, B, A. */
  tolerance: Rgba;
  /** Pixels beyond the tolerance that a replaced-colour rule declared equal. */
  replacedPixels: number;
}

export interface PictureComparison {
  stats: PictureDiffStats;
  /** Canvas-sized PNG data URL: transparent with differing pixels highlighted. */
  mask: string;
  /** Canvas-sized PNG data URL: dimmed grayscale with differences highlighted. */
  difference: string;
}

export const pictureApi = {
  /**
   * Returns both images' info and display images after `transforms`. `reload`
   * re-reads the files from disk; leave it off when only the transforms changed.
   */
  load: (left: string, right: string, transforms: PairTransform, reload: boolean) =>
    invoke<PictureSources>("picture_load", { left, right, transforms, reload }),
  /** Compares the (transformed) pair loaded by `load`. */
  diff: (left: string, right: string, transforms: PairTransform, options: DiffOptions) =>
    invoke<PictureComparison>("picture_diff", { left, right, transforms, options }),
  /** Pixels in a (2·radius+1)² window around canvas position (x, y), for the loupe. */
  probe: (
    left: string,
    right: string,
    transforms: PairTransform,
    options: DiffOptions,
    x: number,
    y: number,
    radius: number,
  ) => invoke<PixelProbe>("picture_probe", { left, right, transforms, options, x, y, radius }),
};
