//! Per-side geometric transforms (rotate, flip, scale) applied to the decoded
//! pixels before comparing, so statistics and the difference mask reflect what
//! the user sees.

use image::imageops::{self, FilterType};
use image::RgbaImage;
use pcompare_core::{CoreError, Result};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Scaled images may not exceed this size in either dimension.
pub const MAX_SCALED_DIM: u32 = 16_384;
/// Accepted range for [`ScaleMode::Percent`].
pub const MIN_SCALE_PERCENT: u32 = 1;
pub const MAX_SCALE_PERCENT: u32 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScaleMode {
    /// Keep the image's own size.
    #[default]
    None,
    /// Resample to the other side's (transformed) size. When both sides ask
    /// for this, only the left side is resampled.
    MatchOther,
    /// Resample to [`Transform::scale_percent`] of the oriented size.
    Percent,
}

/// What to do to one side before comparing. Rotation is applied first, then
/// the flips (in the rotated frame, i.e. as displayed), then scaling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Transform {
    /// Clockwise rotation in degrees; only multiples of 90 are meaningful.
    pub rotate: u16,
    /// Mirror left↔right.
    pub flip_horizontal: bool,
    /// Mirror top↔bottom.
    pub flip_vertical: bool,
    pub scale: ScaleMode,
    /// Used when `scale` is [`ScaleMode::Percent`].
    pub scale_percent: u32,
}

impl Default for Transform {
    fn default() -> Self {
        Transform {
            rotate: 0,
            flip_horizontal: false,
            flip_vertical: false,
            scale: ScaleMode::None,
            scale_percent: 100,
        }
    }
}

impl Transform {
    /// Rotation normalised to 0, 90, 180 or 270 (rounded down to a quarter turn).
    pub fn quarter_turns(&self) -> u16 {
        (self.rotate % 360) / 90
    }

    fn reorients(&self) -> bool {
        self.quarter_turns() != 0 || self.flip_horizontal || self.flip_vertical
    }

    /// Size after rotation (flips don't change it).
    pub fn oriented_size(&self, width: u32, height: u32) -> (u32, u32) {
        if self.quarter_turns() % 2 == 1 {
            (height, width)
        } else {
            (width, height)
        }
    }

    /// Rotates then flips; borrows `img` when nothing changes.
    pub fn orient<'a>(&self, img: &'a RgbaImage) -> Cow<'a, RgbaImage> {
        if !self.reorients() {
            return Cow::Borrowed(img);
        }
        let mut out = match self.quarter_turns() {
            1 => imageops::rotate90(img),
            2 => imageops::rotate180(img),
            3 => imageops::rotate270(img),
            _ => img.clone(),
        };
        if self.flip_horizontal {
            imageops::flip_horizontal_in_place(&mut out);
        }
        if self.flip_vertical {
            imageops::flip_vertical_in_place(&mut out);
        }
        Cow::Owned(out)
    }

    /// Target size for [`ScaleMode::Percent`], or `None` when it is a no-op.
    fn percent_size(&self, width: u32, height: u32) -> Result<Option<(u32, u32)>> {
        if self.scale != ScaleMode::Percent || self.scale_percent == 100 {
            return Ok(None);
        }
        if !(MIN_SCALE_PERCENT..=MAX_SCALE_PERCENT).contains(&self.scale_percent) {
            return Err(CoreError::Other(format!(
                "scale must be between {MIN_SCALE_PERCENT}% and {MAX_SCALE_PERCENT}%"
            )));
        }
        let scale = |v: u32| ((u64::from(v) * u64::from(self.scale_percent) + 50) / 100).max(1);
        let (w, h) = (scale(width), scale(height));
        let max = u64::from(MAX_SCALED_DIM);
        if w > max || h > max {
            return Err(CoreError::Other(format!(
                "scaling to {}% would make the image {w} × {h}; the limit is {MAX_SCALED_DIM} pixels per side",
                self.scale_percent
            )));
        }
        Ok(Some((w as u32, h as u32)))
    }
}

/// Resamples `img` to `width`×`height` (a no-op borrow when already that size).
pub fn resample<'a>(img: Cow<'a, RgbaImage>, width: u32, height: u32) -> Cow<'a, RgbaImage> {
    if img.dimensions() == (width, height) {
        return img;
    }
    // Catmull-Rom is sharp enough for pixel comparison and much cheaper than Lanczos.
    Cow::Owned(imageops::resize(img.as_ref(), width, height, FilterType::CatmullRom))
}

/// Applies both sides' transforms. Each side is oriented and percent-scaled
/// on its own; "match other side" then resamples to the other side's result.
/// Unchanged sides are borrowed.
pub fn apply_pair<'a>(
    left: &'a RgbaImage,
    right: &'a RgbaImage,
    lt: &Transform,
    rt: &Transform,
) -> Result<(Cow<'a, RgbaImage>, Cow<'a, RgbaImage>)> {
    let side = |img: &'a RgbaImage, t: &Transform| -> Result<Cow<'a, RgbaImage>> {
        let oriented = t.orient(img);
        Ok(match t.percent_size(oriented.width(), oriented.height())? {
            Some((w, h)) => resample(oriented, w, h),
            None => oriented,
        })
    };
    let (mut l, mut r) = (side(left, lt)?, side(right, rt)?);
    if lt.scale == ScaleMode::MatchOther {
        let (w, h) = r.dimensions();
        l = resample(l, w, h);
    } else if rt.scale == ScaleMode::MatchOther {
        let (w, h) = l.dimensions();
        r = resample(r, w, h);
    }
    Ok((l, r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    /// 3×2 image whose pixels encode their own coordinates.
    fn coords() -> RgbaImage {
        RgbaImage::from_fn(3, 2, |x, y| Rgba([x as u8, y as u8, 0, 255]))
    }

    fn at(img: &RgbaImage, x: u32, y: u32) -> (u8, u8) {
        let p = img.get_pixel(x, y).0;
        (p[0], p[1])
    }

    #[test]
    fn identity_borrows() {
        let img = coords();
        assert!(matches!(Transform::default().orient(&img), Cow::Borrowed(_)));
        let (l, r) = apply_pair(&img, &img, &Transform::default(), &Transform::default()).unwrap();
        assert!(matches!(l, Cow::Borrowed(_)) && matches!(r, Cow::Borrowed(_)));
    }

    #[test]
    fn rotations() {
        let img = coords();
        let rot = |deg| {
            Transform {
                rotate: deg,
                ..Default::default()
            }
            .orient(&img)
            .into_owned()
        };
        let r90 = rot(90);
        assert_eq!(r90.dimensions(), (2, 3));
        // Clockwise: the old bottom-left corner becomes the top-left.
        assert_eq!(at(&r90, 0, 0), (0, 1));
        assert_eq!(at(&r90, 1, 0), (0, 0));
        assert_eq!(at(&r90, 0, 2), (2, 1));

        let r180 = rot(180);
        assert_eq!(r180.dimensions(), (3, 2));
        assert_eq!(at(&r180, 0, 0), (2, 1));

        let r270 = rot(270);
        assert_eq!(r270.dimensions(), (2, 3));
        assert_eq!(at(&r270, 0, 0), (2, 0));
        assert_eq!(rot(360).dimensions(), (3, 2));
        assert_eq!(
            Transform {
                rotate: 450,
                ..Default::default()
            }
            .oriented_size(3, 2),
            (2, 3)
        );
    }

    #[test]
    fn flips_apply_after_rotation() {
        let img = coords();
        let h = Transform {
            flip_horizontal: true,
            ..Default::default()
        }
        .orient(&img)
        .into_owned();
        assert_eq!(at(&h, 0, 0), (2, 0));
        let v = Transform {
            flip_vertical: true,
            ..Default::default()
        }
        .orient(&img)
        .into_owned();
        assert_eq!(at(&v, 0, 0), (0, 1));

        // Rotate 90° clockwise, then mirror the displayed result left↔right.
        let rh = Transform {
            rotate: 90,
            flip_horizontal: true,
            ..Default::default()
        }
        .orient(&img)
        .into_owned();
        assert_eq!(rh.dimensions(), (2, 3));
        assert_eq!(at(&rh, 0, 0), (0, 0));
        assert_eq!(at(&rh, 1, 2), (2, 1));
    }

    #[test]
    fn percent_scaling() {
        let img = RgbaImage::from_pixel(10, 4, Rgba([7, 7, 7, 255]));
        let half = Transform {
            scale: ScaleMode::Percent,
            scale_percent: 50,
            ..Default::default()
        };
        let (l, r) = apply_pair(&img, &img, &half, &Transform::default()).unwrap();
        assert_eq!(l.dimensions(), (5, 2));
        assert_eq!(l.get_pixel(2, 1).0, [7, 7, 7, 255]);
        assert!(matches!(r, Cow::Borrowed(_)));

        let tiny = Transform {
            scale: ScaleMode::Percent,
            scale_percent: 1,
            ..Default::default()
        };
        assert_eq!(apply_pair(&img, &img, &tiny, &tiny).unwrap().0.dimensions(), (1, 1));

        let bad = Transform {
            scale: ScaleMode::Percent,
            scale_percent: 0,
            ..Default::default()
        };
        assert!(apply_pair(&img, &img, &bad, &Transform::default()).is_err());
        let huge = RgbaImage::new(MAX_SCALED_DIM, 1);
        let double = Transform {
            scale: ScaleMode::Percent,
            scale_percent: 200,
            ..Default::default()
        };
        assert!(apply_pair(&huge, &img, &double, &Transform::default()).is_err());
    }

    #[test]
    fn match_other_side() {
        let small = RgbaImage::from_pixel(4, 3, Rgba([1, 2, 3, 255]));
        let big = RgbaImage::from_pixel(8, 6, Rgba([1, 2, 3, 255]));
        let matching = Transform {
            scale: ScaleMode::MatchOther,
            ..Default::default()
        };
        let (l, r) = apply_pair(&small, &big, &matching, &Transform::default()).unwrap();
        assert_eq!(l.dimensions(), (8, 6));
        assert_eq!(r.dimensions(), (8, 6));

        // Right matches the left's rotated size.
        let rotated = Transform {
            rotate: 90,
            ..Default::default()
        };
        let (l, r) = apply_pair(&small, &big, &rotated, &matching).unwrap();
        assert_eq!(l.dimensions(), (3, 4));
        assert_eq!(r.dimensions(), (3, 4));

        // Both asking: only the left side follows.
        let (l, r) = apply_pair(&small, &big, &matching, &matching).unwrap();
        assert_eq!((l.dimensions(), r.dimensions()), ((8, 6), (8, 6)));
    }
}
