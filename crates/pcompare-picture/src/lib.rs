//! Picture comparison engine for PCompare.
//!
//! Decodes two images (first frame of animated formats), optionally rotates,
//! flips and scales each side ([`transform`]), compares them pixel by pixel
//! with per-channel tolerances and "replaced colour" rules, and renders
//! webview-ready PNG data URLs: both sides, a difference composite and a
//! transparent difference mask. Statistics are always computed at full
//! resolution; only the display images are downscaled when they exceed the
//! configured maximum dimension.

pub mod transform;

use base64::Engine as _;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{DynamicImage, ExtendedColorType, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, RgbaImage};
use pcompare_core::{CoreError, Result};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::io::Cursor;
use std::path::Path;
pub use transform::{ScaleMode, Transform};

/// Display images larger than this (in either dimension) are downscaled.
pub const DEFAULT_MAX_DISPLAY_DIM: u32 = 4096;

/// Colour used to paint differing pixels in the mask and difference images.
pub const HIGHLIGHT: [u8; 4] = [255, 32, 96, 255];

/// What the UI shows about one side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PictureInfo {
    pub width: u32,
    pub height: u32,
    /// Container format, e.g. `PNG` or `JPEG`.
    pub format: String,
    /// Channel layout as stored in the file, e.g. `RGBA`, `Grayscale`, `CMYK`.
    pub color_type: String,
    /// Bits per channel as stored in the file.
    pub bit_depth: u8,
    pub has_alpha: bool,
    pub file_size: u64,
    /// Size as decoded, before any [`Transform`] (`width`/`height` are after).
    pub source_width: u32,
    pub source_height: u32,
}

/// A decoded image normalized to 8-bit RGBA, plus its original properties.
#[derive(Debug, Clone)]
pub struct Picture {
    pub info: PictureInfo,
    pub pixels: RgbaImage,
}

impl Picture {
    /// Reads and decodes an image file. The extension is used as a format hint
    /// for formats without a signature (e.g. TGA); content sniffing wins otherwise.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_as(path, &path.display().to_string())
    }

    /// Like [`Picture::open`], but names the file `label` in error messages
    /// (useful when `path` is a download cache for a remote file).
    pub fn open_as(path: &Path, label: &str) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| CoreError::io(label, e))?;
        decode(&bytes, ImageFormat::from_path(path).ok()).map_err(|e| CoreError::Other(format!("{label}: {e}")))
    }

    /// Decodes an in-memory image. `format_hint` is used only when the
    /// content has no recognizable signature.
    pub fn decode(bytes: &[u8], format_hint: Option<ImageFormat>) -> Result<Self> {
        decode(bytes, format_hint).map_err(CoreError::Other)
    }
}

fn decode(bytes: &[u8], format_hint: Option<ImageFormat>) -> std::result::Result<Picture, String> {
    let mut reader = ImageReader::new(Cursor::new(bytes));
    if let Some(hint) = format_hint {
        reader.set_format(hint);
    }
    let reader = reader.with_guessed_format().map_err(|e| e.to_string())?;
    let format = reader.format().ok_or("unrecognized image format")?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().map_err(|e| e.to_string())?;
    let original = decoder.original_color_type();
    let mut image = DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    // Match what other viewers show for rotated camera JPEGs.
    image.apply_orientation(orientation);
    if image.width() == 0 || image.height() == 0 {
        return Err("image has no pixels".into());
    }
    let (color_type, bit_depth) = describe_color(original);
    Ok(Picture {
        info: PictureInfo {
            width: image.width(),
            height: image.height(),
            format: format_name(format),
            color_type: color_type.to_string(),
            bit_depth,
            has_alpha: image.color().has_alpha(),
            file_size: bytes.len() as u64,
            source_width: image.width(),
            source_height: image.height(),
        },
        pixels: image.into_rgba8(),
    })
}

fn format_name(format: ImageFormat) -> String {
    match format {
        ImageFormat::Png => "PNG".into(),
        ImageFormat::Jpeg => "JPEG".into(),
        ImageFormat::Gif => "GIF".into(),
        ImageFormat::WebP => "WebP".into(),
        ImageFormat::Pnm => "PNM".into(),
        ImageFormat::Tiff => "TIFF".into(),
        ImageFormat::Tga => "TGA".into(),
        ImageFormat::Bmp => "BMP".into(),
        ImageFormat::Ico => "ICO".into(),
        ImageFormat::Qoi => "QOI".into(),
        other => format!("{other:?}").to_uppercase(),
    }
}

fn describe_color(c: ExtendedColorType) -> (&'static str, u8) {
    use ExtendedColorType::*;
    let name = match c {
        A8 => "Alpha",
        L1 | L2 | L4 | L8 | L16 => "Grayscale",
        La1 | La2 | La4 | La8 | La16 => "Grayscale + alpha",
        Rgb1 | Rgb2 | Rgb4 | Rgb5x1 | Rgb8 | Rgb16 => "RGB",
        Rgba1 | Rgba2 | Rgba4 | Rgba8 | Rgba16 => "RGBA",
        Bgr8 => "BGR",
        Bgra8 => "BGRA",
        Rgb32F => "RGB float",
        Rgba32F => "RGBA float",
        Cmyk8 | Cmyk16 => "CMYK",
        _ => "Unknown",
    };
    let depth = c.bits_per_pixel() / u16::from(c.channel_count().max(1));
    (name, u8::try_from(depth).unwrap_or(u8::MAX))
}

/// Axis-aligned rectangle in full-resolution pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Full-resolution comparison statistics.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffStats {
    /// No pixel differs by more than the tolerance and the sizes match.
    pub identical: bool,
    /// Pixels differing beyond the tolerance, plus pixels present in only one image.
    pub differing_pixels: u64,
    /// Pixels present in at least one image.
    pub total_pixels: u64,
    /// `differing_pixels / total_pixels * 100`.
    pub percent: f64,
    /// Largest per-channel difference in the overlapping region, regardless of
    /// tolerance (0 means the overlap is exactly equal).
    pub max_delta: u8,
    /// Bounding box of all differing pixels, if any.
    pub bounds: Option<Rect>,
    pub size_mismatch: bool,
    /// Per-channel tolerance used: R, G, B, A.
    pub tolerance: [u8; 4],
    /// Pixels beyond the tolerance that a replaced-colour rule declared equal.
    pub replaced_pixels: u64,
}

/// "Ignore replaced colours": a left pixel near `left` facing a right pixel
/// near `right` counts as equal (e.g. a recoloured background).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColorRule {
    /// RGBA colour on the left side.
    pub left: [u8; 4],
    /// RGBA colour it was replaced with on the right side.
    pub right: [u8; 4],
    /// Largest per-channel distance from the rule's colours still matching.
    #[serde(default)]
    pub tolerance: u8,
}

impl ColorRule {
    fn near(px: &[u8; 4], color: &[u8; 4], tolerance: u8) -> bool {
        px.iter().zip(color).all(|(a, b)| a.abs_diff(*b) <= tolerance)
    }

    pub fn matches(&self, left: &[u8; 4], right: &[u8; 4]) -> bool {
        Self::near(left, &self.left, self.tolerance) && Self::near(right, &self.right, self.tolerance)
    }
}

/// How two images are compared.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DiffOptions {
    /// Largest difference still treated as equal, per channel: R, G, B, A.
    pub tolerance: [u8; 4],
    pub replaced_colors: Vec<ColorRule>,
}

impl DiffOptions {
    /// The same tolerance on every channel and no colour rules.
    pub fn uniform(tolerance: u8) -> Self {
        DiffOptions {
            tolerance: [tolerance; 4],
            replaced_colors: Vec::new(),
        }
    }
}

/// How one overlapping pixel pair compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PixelMatch {
    Same,
    /// Beyond the tolerance, but covered by a replaced-colour rule.
    Replaced,
    Different,
}

/// Largest per-channel difference between two pixels; fully transparent
/// pixels are equal whatever their colour.
fn pixel_delta(lp: &[u8; 4], rp: &[u8; 4]) -> u8 {
    if lp == rp || (lp[3] == 0 && rp[3] == 0) {
        return 0;
    }
    lp.iter().zip(rp).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0)
}

/// Compares one pixel pair under `options`.
pub fn compare_pixel(lp: &[u8; 4], rp: &[u8; 4], options: &DiffOptions) -> PixelMatch {
    if pixel_delta(lp, rp) == 0 {
        return PixelMatch::Same;
    }
    let beyond = lp
        .iter()
        .zip(rp)
        .zip(&options.tolerance)
        .any(|((a, b), t)| a.abs_diff(*b) > *t);
    if !beyond {
        PixelMatch::Same
    } else if options.replaced_colors.iter().any(|r| r.matches(lp, rp)) {
        PixelMatch::Replaced
    } else {
        PixelMatch::Different
    }
}

/// Result of [`diff_pixels`]: statistics plus a per-pixel mask over the
/// canvas (the smallest rectangle anchored at the top-left containing both images).
#[derive(Debug, Clone)]
pub struct PixelDiff {
    pub stats: DiffStats,
    pub width: u32,
    pub height: u32,
    /// One byte per canvas pixel, row-major; non-zero where the images differ.
    pub mask: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Accum {
    count: u64,
    replaced: u64,
    max_delta: u8,
    min_x: u32,
    max_x: u32,
    min_y: u32,
    max_y: u32,
}

impl Default for Accum {
    fn default() -> Self {
        Accum {
            count: 0,
            replaced: 0,
            max_delta: 0,
            min_x: u32::MAX,
            max_x: 0,
            min_y: u32::MAX,
            max_y: 0,
        }
    }
}

impl Accum {
    /// Records `n` differing pixels spanning columns `x0..=x1` of row `y`.
    fn mark(&mut self, x0: u32, x1: u32, y: u32, n: u64) {
        self.count += n;
        self.min_x = self.min_x.min(x0);
        self.max_x = self.max_x.max(x1);
        self.min_y = self.min_y.min(y);
        self.max_y = self.max_y.max(y);
    }

    fn merge(self, o: Accum) -> Accum {
        Accum {
            count: self.count + o.count,
            replaced: self.replaced + o.replaced,
            max_delta: self.max_delta.max(o.max_delta),
            min_x: self.min_x.min(o.min_x),
            max_x: self.max_x.max(o.max_x),
            min_y: self.min_y.min(o.min_y),
            max_y: self.max_y.max(o.max_y),
        }
    }
}

/// Compares two RGBA images. A pixel differs when any channel differs by more
/// than that channel's tolerance and no replaced-colour rule covers the pair;
/// pixels that are fully transparent on both sides are equal whatever their
/// colour. When the sizes differ, the overlapping top-left region is compared
/// and the area covered by only one image counts as different.
pub fn diff_pixels(left: &RgbaImage, right: &RgbaImage, options: &DiffOptions) -> PixelDiff {
    let (lw, lh) = left.dimensions();
    let (rw, rh) = right.dimensions();
    let (cw, ch) = (lw.max(rw), lh.max(rh));
    let (ow, oh) = (lw.min(rw), lh.min(rh));
    let (l, r) = (left.as_raw(), right.as_raw());
    let (lw_us, rw_us, ow_us) = (lw as usize, rw as usize, ow as usize);

    let mut mask = vec![0u8; cw as usize * ch as usize];
    let acc = mask
        .par_chunks_mut(cw as usize)
        .enumerate()
        .map(|(y, row)| {
            let y32 = y as u32;
            let (in_l, in_r) = (y32 < lh, y32 < rh);
            let mut acc = Accum::default();
            if in_l && in_r {
                let lrow = &l[y * lw_us * 4..(y * lw_us + ow_us) * 4];
                let rrow = &r[y * rw_us * 4..(y * rw_us + ow_us) * 4];
                for (x, (lp, rp)) in lrow.as_chunks::<4>().0.iter().zip(rrow.as_chunks::<4>().0).enumerate() {
                    let delta = pixel_delta(lp, rp);
                    if delta == 0 {
                        continue;
                    }
                    acc.max_delta = acc.max_delta.max(delta);
                    match compare_pixel(lp, rp, options) {
                        PixelMatch::Same => {}
                        PixelMatch::Replaced => acc.replaced += 1,
                        PixelMatch::Different => {
                            row[x] = 1;
                            acc.mark(x as u32, x as u32, y32, 1);
                        }
                    }
                }
            }
            // Columns covered by exactly one image on this row.
            let (start, end) = match (in_l, in_r) {
                (true, true) => (ow, lw.max(rw)),
                (true, false) => (0, lw),
                (false, true) => (0, rw),
                (false, false) => (0, 0),
            };
            if start < end {
                row[start as usize..end as usize].fill(1);
                acc.mark(start, end - 1, y32, u64::from(end - start));
            }
            acc
        })
        .reduce(Accum::default, Accum::merge);

    let total = u64::from(lw) * u64::from(lh) + u64::from(rw) * u64::from(rh) - u64::from(ow) * u64::from(oh);
    let size_mismatch = (lw, lh) != (rw, rh);
    let bounds = (acc.count > 0).then(|| Rect {
        x: acc.min_x,
        y: acc.min_y,
        width: acc.max_x - acc.min_x + 1,
        height: acc.max_y - acc.min_y + 1,
    });
    PixelDiff {
        stats: DiffStats {
            identical: acc.count == 0 && !size_mismatch,
            differing_pixels: acc.count,
            total_pixels: total,
            percent: if total == 0 {
                0.0
            } else {
                acc.count as f64 * 100.0 / total as f64
            },
            max_delta: acc.max_delta,
            bounds,
            size_mismatch,
            tolerance: options.tolerance,
            replaced_pixels: acc.replaced,
        },
        width: cw,
        height: ch,
        mask,
    }
}

/// Scale factor (≤ 1) that fits a `width`×`height` canvas within `max_dim`.
pub fn display_scale(width: u32, height: u32, max_dim: u32) -> f64 {
    let largest = width.max(height);
    if largest <= max_dim || max_dim == 0 {
        1.0
    } else {
        f64::from(max_dim) / f64::from(largest)
    }
}

fn scaled(len: u32, scale: f64) -> u32 {
    ((f64::from(len) * scale).round() as u32).max(1)
}

fn downscale(img: &RgbaImage, scale: f64) -> Option<RgbaImage> {
    (scale < 1.0).then(|| image::imageops::thumbnail(img, scaled(img.width(), scale), scaled(img.height(), scale)))
}

/// Reduces a full-resolution mask to display resolution so that a display
/// pixel is set whenever any source pixel it covers differs (single-pixel
/// differences must not vanish when downscaling).
fn pool_mask(diff: &PixelDiff, scale: f64) -> (Cow<'_, [u8]>, u32, u32) {
    if scale >= 1.0 {
        return (Cow::Borrowed(&diff.mask), diff.width, diff.height);
    }
    let (dw, dh) = (scaled(diff.width, scale), scaled(diff.height, scale));
    let to_display = |v: u32, max: u32| ((f64::from(v) * scale) as u32).min(max - 1) as usize;
    let cols: Vec<usize> = (0..diff.width).map(|x| to_display(x, dw)).collect();
    let mut out = vec![0u8; dw as usize * dh as usize];
    for (y, row) in diff.mask.chunks_exact(diff.width as usize).enumerate() {
        let base = to_display(y as u32, dh) * dw as usize;
        for (x, _) in row.iter().enumerate().filter(|(_, &m)| m != 0) {
            out[base + cols[x]] = 1;
        }
    }
    (Cow::Owned(out), dw, dh)
}

/// Transparent image with differing pixels painted in [`HIGHLIGHT`].
fn render_mask(mask: &[u8], width: u32, height: u32) -> RgbaImage {
    let mut out = RgbaImage::new(width, height);
    out.par_chunks_mut(4).zip(mask.par_iter()).for_each(|(px, &m)| {
        if m != 0 {
            px.copy_from_slice(&HIGHLIGHT);
        }
    });
    out
}

/// Dimmed grayscale of the left image (right where the left is absent) with
/// differing pixels painted in [`HIGHLIGHT`].
fn render_difference(left: &RgbaImage, right: &RgbaImage, mask: &[u8], width: u32, height: u32) -> RgbaImage {
    let mut out = RgbaImage::new(width, height);
    let w = width as usize;
    out.par_chunks_mut(w * 4).enumerate().for_each(|(y, row)| {
        let y32 = y as u32;
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            if mask[y * w + x] != 0 {
                px.copy_from_slice(&HIGHLIGHT);
                continue;
            }
            let x32 = x as u32;
            let src = if x32 < left.width() && y32 < left.height() {
                left.get_pixel(x32, y32).0
            } else if x32 < right.width() && y32 < right.height() {
                right.get_pixel(x32, y32).0
            } else {
                continue;
            };
            let luma = (299 * u32::from(src[0]) + 587 * u32::from(src[1]) + 114 * u32::from(src[2])) / 1000;
            let gray = (24 + luma * 2 / 5) as u8;
            px.copy_from_slice(&[gray, gray, gray, src[3]]);
        }
    });
    out
}

/// Encodes an image as a `data:image/png;base64,...` URL. Opaque images are
/// written as RGB to keep the payload small.
pub fn png_data_url(img: &RgbaImage) -> Result<String> {
    let mut png = Vec::new();
    let encoder = PngEncoder::new_with_quality(&mut png, CompressionType::Fast, FilterType::Adaptive);
    let raw = img.as_raw();
    let result = if raw.as_chunks::<4>().0.iter().all(|p| p[3] == u8::MAX) {
        let mut rgb = Vec::with_capacity(raw.len() / 4 * 3);
        raw.as_chunks::<4>()
            .0
            .iter()
            .for_each(|p| rgb.extend_from_slice(&p[..3]));
        encoder.write_image(&rgb, img.width(), img.height(), ExtendedColorType::Rgb8)
    } else {
        encoder.write_image(raw, img.width(), img.height(), ExtendedColorType::Rgba8)
    };
    result.map_err(|e| CoreError::Other(format!("cannot encode PNG: {e}")))?;
    Ok(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&png)
    ))
}

/// Both sides ready for display.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PictureSources {
    pub left: PictureInfo,
    pub right: PictureInfo,
    /// PNG data URL of the left image (downscaled by `display_scale`).
    pub left_image: String,
    pub right_image: String,
    /// Full-resolution size of the canvas holding both images.
    pub canvas_width: u32,
    pub canvas_height: u32,
    /// Display images are this fraction of full resolution (1 = not downscaled).
    pub display_scale: f64,
}

/// Comparison at one tolerance.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PictureComparison {
    pub stats: DiffStats,
    /// PNG data URL covering the canvas: transparent, differing pixels highlighted.
    pub mask: String,
    /// PNG data URL covering the canvas: dimmed grayscale with differences highlighted.
    pub difference: String,
}

/// Largest loupe window radius [`PicturePair::probe`] serves.
pub const MAX_PROBE_RADIUS: u32 = 16;

/// Pixels around a canvas position on both sides, row-major. Cells outside
/// an image are `None`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PixelProbe {
    /// Centre of the window, full-resolution canvas coordinates.
    pub x: u32,
    pub y: u32,
    /// Window side length in pixels (odd).
    pub size: u32,
    pub left: Vec<Option<[u8; 4]>>,
    pub right: Vec<Option<[u8; 4]>>,
    /// How each cell compares; `None` where neither image has a pixel.
    pub matches: Vec<Option<PixelMatch>>,
}

/// Two decoded pictures plus their display-resolution copies, reusable across
/// comparisons at different tolerances.
#[derive(Debug, Clone)]
pub struct PicturePair {
    pub left: Picture,
    pub right: Picture,
    scale: f64,
    // `None` when the picture is already small enough to display as is.
    left_display: Option<RgbaImage>,
    right_display: Option<RgbaImage>,
}

impl PicturePair {
    pub fn new(left: Picture, right: Picture, max_display_dim: u32) -> Self {
        let (cw, ch) = (
            left.pixels.width().max(right.pixels.width()),
            left.pixels.height().max(right.pixels.height()),
        );
        let scale = display_scale(cw, ch, max_display_dim);
        let (left_display, right_display) =
            rayon::join(|| downscale(&left.pixels, scale), || downscale(&right.pixels, scale));
        PicturePair {
            left,
            right,
            scale,
            left_display,
            right_display,
        }
    }

    /// Decodes both files in parallel. `labels` name the files in error
    /// messages (e.g. user-facing paths when `paths` point at a download cache).
    pub fn open(paths: [&Path; 2], labels: [&str; 2], max_display_dim: u32) -> Result<Self> {
        let (left, right) = rayon::join(
            || Picture::open_as(paths[0], labels[0]),
            || Picture::open_as(paths[1], labels[1]),
        );
        Ok(Self::new(left?, right?, max_display_dim))
    }

    /// Full-resolution canvas size (both images anchored at the top-left).
    pub fn canvas(&self) -> (u32, u32) {
        (
            self.left.pixels.width().max(self.right.pixels.width()),
            self.left.pixels.height().max(self.right.pixels.height()),
        )
    }

    pub fn display_scale(&self) -> f64 {
        self.scale
    }

    fn left_display(&self) -> &RgbaImage {
        self.left_display.as_ref().unwrap_or(&self.left.pixels)
    }

    fn right_display(&self) -> &RgbaImage {
        self.right_display.as_ref().unwrap_or(&self.right.pixels)
    }

    /// Image info plus PNG data URLs of both sides.
    pub fn sources(&self) -> Result<PictureSources> {
        let (left_image, right_image) = rayon::join(
            || png_data_url(self.left_display()),
            || png_data_url(self.right_display()),
        );
        let (canvas_width, canvas_height) = self.canvas();
        Ok(PictureSources {
            left: self.left.info.clone(),
            right: self.right.info.clone(),
            left_image: left_image?,
            right_image: right_image?,
            canvas_width,
            canvas_height,
            display_scale: self.scale,
        })
    }

    /// This pair with each side rotated, flipped and scaled, or `None` when
    /// the transforms leave both sides unchanged.
    pub fn transformed(&self, left: &Transform, right: &Transform, max_display_dim: u32) -> Result<Option<Self>> {
        let (l, r) = transform::apply_pair(&self.left.pixels, &self.right.pixels, left, right)?;
        if matches!((&l, &r), (Cow::Borrowed(_), Cow::Borrowed(_))) {
            return Ok(None);
        }
        let side = |pic: &Picture, pixels: Cow<'_, RgbaImage>| {
            let pixels = pixels.into_owned();
            Picture {
                info: PictureInfo {
                    width: pixels.width(),
                    height: pixels.height(),
                    ..pic.info.clone()
                },
                pixels,
            }
        };
        Ok(Some(Self::new(
            side(&self.left, l),
            side(&self.right, r),
            max_display_dim,
        )))
    }

    /// Full-resolution pixels in a `(2 * radius + 1)²` window centred on
    /// (`x`, `y`) of both sides, for the loupe.
    pub fn probe(&self, x: u32, y: u32, radius: u32, options: &DiffOptions) -> PixelProbe {
        let radius = radius.min(MAX_PROBE_RADIUS);
        let size = 2 * radius + 1;
        let get = |img: &RgbaImage, px: i64, py: i64| {
            (px >= 0 && py >= 0 && px < i64::from(img.width()) && py < i64::from(img.height()))
                .then(|| img.get_pixel(px as u32, py as u32).0)
        };
        let (cx, cy, r) = (i64::from(x), i64::from(y), i64::from(radius));
        let mut probe = PixelProbe {
            x,
            y,
            size,
            left: Vec::with_capacity((size * size) as usize),
            right: Vec::with_capacity((size * size) as usize),
            matches: Vec::with_capacity((size * size) as usize),
        };
        for py in cy - r..=cy + r {
            for px in cx - r..=cx + r {
                let (l, rt) = (get(&self.left.pixels, px, py), get(&self.right.pixels, px, py));
                probe.matches.push(match (l, rt) {
                    (Some(a), Some(b)) => Some(compare_pixel(&a, &b, options)),
                    (None, None) => None,
                    _ => Some(PixelMatch::Different),
                });
                probe.left.push(l);
                probe.right.push(rt);
            }
        }
        probe
    }

    /// Full-resolution statistics plus display-resolution mask and difference images.
    pub fn compare(&self, options: &DiffOptions) -> Result<PictureComparison> {
        let diff = diff_pixels(&self.left.pixels, &self.right.pixels, options);
        let (mask, dw, dh) = pool_mask(&diff, self.scale);
        let (mask_url, difference_url) = rayon::join(
            || png_data_url(&render_mask(&mask, dw, dh)),
            || {
                png_data_url(&render_difference(
                    self.left_display(),
                    self.right_display(),
                    &mask,
                    dw,
                    dh,
                ))
            },
        );
        Ok(PictureComparison {
            stats: diff.stats,
            mask: mask_url?,
            difference: difference_url?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Luma, Rgba};

    fn solid(w: u32, h: u32, px: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba(px))
    }

    fn png_bytes(img: &DynamicImage) -> Vec<u8> {
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn identical_images() {
        let a = solid(8, 6, [10, 20, 30, 255]);
        let d = diff_pixels(&a, &a.clone(), &DiffOptions::uniform(0));
        assert!(d.stats.identical);
        assert_eq!(d.stats.differing_pixels, 0);
        assert_eq!(d.stats.total_pixels, 48);
        assert_eq!(d.stats.max_delta, 0);
        assert_eq!(d.stats.bounds, None);
        assert!(d.mask.iter().all(|&m| m == 0));
    }

    #[test]
    fn single_pixel_within_and_over_tolerance() {
        let a = solid(10, 10, [100, 100, 100, 255]);
        let mut b = a.clone();
        b.put_pixel(3, 7, Rgba([100, 105, 100, 255]));

        let within = diff_pixels(&a, &b, &DiffOptions::uniform(5));
        assert!(within.stats.identical);
        assert_eq!(within.stats.max_delta, 5);

        let over = diff_pixels(&a, &b, &DiffOptions::uniform(4));
        assert!(!over.stats.identical);
        assert_eq!(over.stats.differing_pixels, 1);
        assert_eq!(over.stats.max_delta, 5);
        assert!((over.stats.percent - 1.0).abs() < 1e-9);
        assert_eq!(
            over.stats.bounds,
            Some(Rect {
                x: 3,
                y: 7,
                width: 1,
                height: 1
            })
        );
        assert_eq!(over.mask[7 * 10 + 3], 1);
    }

    #[test]
    fn different_sizes_count_non_overlap() {
        let a = solid(4, 4, [1, 2, 3, 255]);
        let b = solid(6, 3, [1, 2, 3, 255]);
        let d = diff_pixels(&a, &b, &DiffOptions::uniform(0));
        assert_eq!((d.width, d.height), (6, 4));
        assert!(d.stats.size_mismatch);
        assert!(!d.stats.identical);
        // 16 + 18 - 12 overlapping pixels; everything outside the overlap differs.
        assert_eq!(d.stats.total_pixels, 22);
        assert_eq!(d.stats.differing_pixels, 10);
        assert_eq!(
            d.stats.bounds,
            Some(Rect {
                x: 0,
                y: 0,
                width: 6,
                height: 4
            })
        );
        assert_eq!(d.mask[3 * 6 + 5], 0, "corner covered by neither image");
        assert_eq!(d.mask[3 * 6], 1, "left-only row");
        assert_eq!(d.mask[5], 1, "right-only column");
        assert_eq!(d.mask[0], 0);
    }

    #[test]
    fn alpha_differences() {
        let a = solid(2, 2, [50, 60, 70, 255]);
        let b = solid(2, 2, [50, 60, 70, 128]);
        let d = diff_pixels(&a, &b, &DiffOptions::uniform(0));
        assert_eq!(d.stats.differing_pixels, 4);
        assert_eq!(d.stats.max_delta, 127);

        // Invisible colour differences don't count.
        let c = solid(2, 2, [0, 0, 0, 0]);
        let e = solid(2, 2, [255, 255, 255, 0]);
        let d = diff_pixels(&c, &e, &DiffOptions::uniform(0));
        assert!(d.stats.identical);
        assert_eq!(d.stats.max_delta, 0);
    }

    #[test]
    fn decodes_png_info() {
        let img = DynamicImage::ImageRgba8(solid(5, 3, [1, 2, 3, 4]));
        let p = Picture::decode(&png_bytes(&img), None).unwrap();
        assert_eq!((p.info.width, p.info.height), (5, 3));
        assert_eq!(p.info.format, "PNG");
        assert_eq!(p.info.color_type, "RGBA");
        assert_eq!(p.info.bit_depth, 8);
        assert!(p.info.has_alpha);

        let gray16 = DynamicImage::ImageLuma16(ImageBuffer::from_pixel(2, 2, Luma([40000u16])));
        let p = Picture::decode(&png_bytes(&gray16), None).unwrap();
        assert_eq!(p.info.color_type, "Grayscale");
        assert_eq!(p.info.bit_depth, 16);
        assert!(!p.info.has_alpha);
        assert_eq!(p.pixels.get_pixel(0, 0).0[3], 255);
    }

    #[test]
    fn rejects_corrupt_and_unsupported_input() {
        assert!(Picture::decode(b"definitely not an image", None).is_err());
        let png = png_bytes(&DynamicImage::ImageRgb8(image::RgbImage::new(16, 16)));
        assert!(Picture::decode(&png[..png.len() / 2], None).is_err());
    }

    #[test]
    fn open_uses_extension_for_signatureless_formats() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pic.tga");
        DynamicImage::ImageRgb8(image::RgbImage::from_pixel(3, 2, image::Rgb([9, 8, 7])))
            .save(&path)
            .unwrap();
        let p = Picture::open(&path).unwrap();
        assert_eq!(p.info.format, "TGA");
        assert_eq!(p.pixels.get_pixel(2, 1).0, [9, 8, 7, 255]);

        let bad = dir.path().join("bad.png");
        std::fs::write(&bad, b"garbage").unwrap();
        let err = Picture::open(&bad).unwrap_err().to_string();
        assert!(err.contains("bad.png"), "{err}");
        assert!(Picture::open(&dir.path().join("missing.png")).is_err());
    }

    fn picture(pixels: RgbaImage) -> Picture {
        Picture {
            info: PictureInfo {
                width: pixels.width(),
                height: pixels.height(),
                format: "PNG".into(),
                color_type: "RGBA".into(),
                bit_depth: 8,
                has_alpha: true,
                file_size: 0,
                source_width: pixels.width(),
                source_height: pixels.height(),
            },
            pixels,
        }
    }

    fn decode_url(url: &str) -> RgbaImage {
        let b64 = url.strip_prefix("data:image/png;base64,").expect("PNG data URL");
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        image::load_from_memory(&bytes).unwrap().into_rgba8()
    }

    #[test]
    fn downscaled_display_keeps_single_pixel_differences() {
        let a = solid(100, 50, [200, 200, 200, 255]);
        let mut b = a.clone();
        b.put_pixel(57, 33, Rgba([0, 0, 0, 255]));
        let pair = PicturePair::new(picture(a), picture(b), 40);
        assert!((pair.display_scale() - 0.4).abs() < 1e-9);

        let sources = pair.sources().unwrap();
        assert_eq!((sources.canvas_width, sources.canvas_height), (100, 50));
        assert_eq!(decode_url(&sources.left_image).dimensions(), (40, 20));

        let cmp = pair.compare(&DiffOptions::default()).unwrap();
        assert_eq!(cmp.stats.differing_pixels, 1);
        let mask = decode_url(&cmp.mask);
        assert_eq!(mask.dimensions(), (40, 20));
        let marked: Vec<_> = mask.enumerate_pixels().filter(|(_, _, p)| p.0[3] != 0).collect();
        assert_eq!(marked.len(), 1);
        assert_eq!((marked[0].0, marked[0].1), (22, 13));
        assert_eq!(decode_url(&cmp.difference).get_pixel(22, 13).0, HIGHLIGHT);
    }

    #[test]
    fn full_size_display_when_small() {
        let a = solid(3, 3, [0, 0, 0, 255]);
        let pair = PicturePair::new(picture(a.clone()), picture(a), DEFAULT_MAX_DISPLAY_DIM);
        assert_eq!(pair.display_scale(), 1.0);
        let cmp = pair.compare(&DiffOptions::default()).unwrap();
        assert!(cmp.stats.identical);
        let diff = decode_url(&cmp.difference);
        assert_eq!(diff.get_pixel(1, 1).0, [24, 24, 24, 255]);
        assert!(decode_url(&cmp.mask).pixels().all(|p| p.0[3] == 0));
    }

    #[test]
    fn per_channel_tolerance() {
        let a = solid(2, 1, [100, 100, 100, 255]);
        let mut b = a.clone();
        b.put_pixel(0, 0, Rgba([110, 100, 100, 255]));
        b.put_pixel(1, 0, Rgba([100, 100, 103, 255]));

        let opts = |t: [u8; 4]| DiffOptions {
            tolerance: t,
            ..Default::default()
        };
        // Red may move by 10, blue only by 2: the blue change still differs.
        let d = diff_pixels(&a, &b, &opts([10, 0, 2, 0]));
        assert_eq!(d.stats.differing_pixels, 1);
        assert_eq!(d.mask, vec![0, 1]);
        assert_eq!(d.stats.tolerance, [10, 0, 2, 0]);
        assert_eq!(d.stats.max_delta, 10);

        assert!(diff_pixels(&a, &b, &opts([10, 0, 3, 0])).stats.identical);
        assert_eq!(diff_pixels(&a, &b, &opts([9, 255, 255, 255])).mask, vec![1, 0]);
    }

    #[test]
    fn replaced_colors_count_as_equal() {
        let white = [255, 255, 255, 255];
        let mut a = solid(4, 1, white);
        a.put_pixel(3, 0, Rgba([0, 0, 0, 255]));
        let mut b = solid(4, 1, [250, 20, 20, 255]);
        b.put_pixel(3, 0, Rgba([0, 0, 255, 255]));

        let none = diff_pixels(&a, &b, &DiffOptions::default());
        assert_eq!(none.stats.differing_pixels, 4);
        assert_eq!(none.stats.replaced_pixels, 0);

        // White → red within 5 per channel; black stays different.
        let rule = ColorRule {
            left: white,
            right: [255, 16, 16, 255],
            tolerance: 5,
        };
        let opts = DiffOptions {
            replaced_colors: vec![rule],
            ..Default::default()
        };
        let d = diff_pixels(&a, &b, &opts);
        assert_eq!(d.stats.differing_pixels, 1);
        assert_eq!(d.stats.replaced_pixels, 3);
        assert_eq!(d.mask, vec![0, 0, 0, 1]);
        assert!(!d.stats.identical);

        // Rules are directional: swapping the sides doesn't match.
        assert_eq!(diff_pixels(&b, &a, &opts).stats.replaced_pixels, 0);

        // A tight rule tolerance misses the slightly-off red.
        let tight = DiffOptions {
            replaced_colors: vec![ColorRule { tolerance: 3, ..rule }],
            ..Default::default()
        };
        assert_eq!(diff_pixels(&a, &b, &tight).stats.replaced_pixels, 0);

        assert_eq!(compare_pixel(&white, &white, &opts), PixelMatch::Same);
        assert_eq!(compare_pixel(&white, &[255, 16, 16, 255], &opts), PixelMatch::Replaced);
        assert_eq!(compare_pixel(&white, &[0, 0, 0, 255], &opts), PixelMatch::Different);
    }

    #[test]
    fn transformed_pair_diffs_transformed_pixels() {
        // The right side is the left mirrored; flipping it back makes them equal.
        let left = RgbaImage::from_fn(4, 2, |x, y| Rgba([x as u8 * 50, y as u8 * 90, 0, 255]));
        let mut right = left.clone();
        image::imageops::flip_horizontal_in_place(&mut right);
        let pair = PicturePair::new(picture(left), picture(right), DEFAULT_MAX_DISPLAY_DIM);
        assert!(!pair.compare(&DiffOptions::default()).unwrap().stats.identical);
        assert!(pair
            .transformed(&Transform::default(), &Transform::default(), DEFAULT_MAX_DISPLAY_DIM)
            .unwrap()
            .is_none());

        let flip = Transform {
            flip_horizontal: true,
            ..Default::default()
        };
        let fixed = pair
            .transformed(&Transform::default(), &flip, DEFAULT_MAX_DISPLAY_DIM)
            .unwrap()
            .expect("right side changes");
        assert!(fixed.compare(&DiffOptions::default()).unwrap().stats.identical);

        let rotated = pair
            .transformed(
                &Transform {
                    rotate: 90,
                    ..Default::default()
                },
                &Transform::default(),
                DEFAULT_MAX_DISPLAY_DIM,
            )
            .unwrap()
            .unwrap();
        assert_eq!((rotated.left.info.width, rotated.left.info.height), (2, 4));
        assert_eq!(
            (rotated.left.info.source_width, rotated.left.info.source_height),
            (4, 2)
        );
        assert_eq!(rotated.canvas(), (4, 4));
        let sources = rotated.sources().unwrap();
        assert_eq!(decode_url(&sources.left_image).dimensions(), (2, 4));
    }

    #[test]
    fn probe_window() {
        let mut a = solid(3, 2, [10, 10, 10, 255]);
        a.put_pixel(0, 0, Rgba([1, 2, 3, 4]));
        let b = solid(2, 2, [10, 10, 10, 255]);
        let pair = PicturePair::new(picture(a), picture(b), DEFAULT_MAX_DISPLAY_DIM);
        let p = pair.probe(0, 0, 1, &DiffOptions::default());
        assert_eq!(p.size, 3);
        assert_eq!(p.left.len(), 9);
        // Row-major: the centre is index 4; the cell above-left is off-canvas.
        assert_eq!(p.left[4], Some([1, 2, 3, 4]));
        assert_eq!(p.right[4], Some([10, 10, 10, 255]));
        assert_eq!(p.matches[4], Some(PixelMatch::Different));
        assert_eq!(p.left[0], None);
        assert_eq!(p.matches[0], None);
        assert_eq!(p.matches[5], Some(PixelMatch::Same));

        // (2, 0) exists only on the left.
        let p = pair.probe(2, 0, 0, &DiffOptions::default());
        assert_eq!((p.left[0].is_some(), p.right[0]), (true, None));
        assert_eq!(p.matches[0], Some(PixelMatch::Different));
        assert_eq!(
            pair.probe(0, 0, 100, &DiffOptions::default()).size,
            2 * MAX_PROBE_RADIUS + 1
        );
    }
}
