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
  tolerance: number;
}

export interface PictureComparison {
  stats: PictureDiffStats;
  /** Canvas-sized PNG data URL: transparent with differing pixels highlighted. */
  mask: string;
  /** Canvas-sized PNG data URL: dimmed grayscale with differences highlighted. */
  difference: string;
}

export const pictureApi = {
  /** Decodes both images afresh and returns their info and display images. */
  load: (left: string, right: string) => invoke<PictureSources>("picture_load", { left, right }),
  /** Compares the pair loaded by `load` at a per-channel tolerance (0–255). */
  diff: (left: string, right: string, tolerance: number) =>
    invoke<PictureComparison>("picture_diff", { left, right, tolerance }),
};
