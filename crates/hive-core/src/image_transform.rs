//! Image transforms: crop, rotate, flip, fit inside a box and convert between PNG, JPEG and
//! WebP. Needs the `images` feature.
//!
//! A picture is hostile input, so nothing is decoded before its header says it is small
//! enough (50 megapixels, 256 MiB of decoder memory), and the result is bounded the same way.
//! What it does not do: animation (a GIF is read as its first frame), lossy WebP or AVIF/HEIC
//! output (WebP is written lossless), color profiles (they are dropped) or any change to
//! pixels beyond what is asked (no sharpening, no metadata kept). The EXIF orientation of a
//! JPEG is applied, so a phone photo comes out upright.
use crate::{Error, Result, image as header};
use image_codec::{
    DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits,
    codecs::{jpeg::JpegEncoder, png::PngEncoder, webp::WebPEncoder},
    imageops::FilterType,
};
use serde::Deserialize;
use std::io::Cursor;

/// Pixels an input may have, checked from the header before any decoding.
pub const MAX_INPUT_PIXELS: u64 = 50_000_000;
/// Largest side of an output.
pub const MAX_OUTPUT_SIDE: u32 = 16_384;
const MAX_DECODER_ALLOC: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Png,
    Jpeg,
    Webp,
}
impl Format {
    pub fn mime_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// What to do, applied in this order: crop, rotate, flip, flop, fit, grayscale, encode.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Transform {
    pub crop: Option<Crop>,
    /// Clockwise degrees: 90, 180 or 270.
    pub rotate: Option<u16>,
    /// Mirror top to bottom.
    #[serde(default)]
    pub flip: bool,
    /// Mirror left to right.
    #[serde(default)]
    pub flop: bool,
    /// Fit inside this box keeping the proportions. Never enlarges.
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    #[serde(default)]
    pub grayscale: bool,
    /// Output format; the source's when absent (a GIF becomes PNG).
    pub format: Option<Format>,
    /// 1 to 100; only JPEG uses it. Default 85.
    pub quality: Option<u8>,
}

pub struct Transformed {
    pub bytes: Vec<u8>,
    pub format: Format,
    pub width: u32,
    pub height: u32,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Input(message.into())
}
fn codec(_: image_codec::ImageError) -> Error {
    invalid("the image could not be decoded or encoded")
}

impl Transform {
    fn is_empty(&self) -> bool {
        *self == Transform::default()
    }

    fn validate(&self) -> Result<()> {
        if self.is_empty() {
            return Err(invalid("no transformation requested"));
        }
        if let Some(degrees) = self.rotate
            && ![90, 180, 270].contains(&degrees)
        {
            return Err(invalid("rotate must be 90, 180 or 270"));
        }
        for (name, side) in [
            ("max_width", self.max_width),
            ("max_height", self.max_height),
        ] {
            if side.is_some_and(|side| !(1..=MAX_OUTPUT_SIDE).contains(&side)) {
                return Err(invalid(format!(
                    "{name} must be between 1 and {MAX_OUTPUT_SIDE}"
                )));
            }
        }
        if self
            .quality
            .is_some_and(|quality| !(1..=100).contains(&quality))
        {
            return Err(invalid("quality must be between 1 and 100"));
        }
        if let Some(crop) = self.crop
            && (crop.width == 0 || crop.height == 0)
        {
            return Err(invalid("crop must have a positive width and height"));
        }
        Ok(())
    }
}

/// Apply `transform` to the PNG, JPEG, GIF or WebP in `bytes`.
pub fn apply(bytes: &[u8], transform: &Transform) -> Result<Transformed> {
    transform.validate()?;
    let info = header::info(bytes).ok_or_else(|| invalid("not a recognised image"))?;
    if u64::from(info.width) * u64::from(info.height) > MAX_INPUT_PIXELS {
        return Err(invalid(format!(
            "image is larger than {} megapixels",
            MAX_INPUT_PIXELS / 1_000_000
        )));
    }
    let (source, default) = match info.mime_type {
        "image/png" => (ImageFormat::Png, Format::Png),
        "image/jpeg" => (ImageFormat::Jpeg, Format::Jpeg),
        "image/webp" => (ImageFormat::WebP, Format::Webp),
        "image/gif" => (ImageFormat::Gif, Format::Png),
        _ => return Err(invalid("unsupported image format")),
    };
    let mut reader = ImageReader::with_format(Cursor::new(bytes), source);
    let mut limits = Limits::default();
    limits.max_image_width = Some(info.width);
    limits.max_image_height = Some(info.height);
    limits.max_alloc = Some(MAX_DECODER_ALLOC);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(codec)?;
    let orientation = decoder.orientation().map_err(codec)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(codec)?;
    image.apply_orientation(orientation);

    if let Some(crop) = transform.crop {
        if u64::from(crop.x) + u64::from(crop.width) > u64::from(image.width())
            || u64::from(crop.y) + u64::from(crop.height) > u64::from(image.height())
        {
            return Err(invalid(format!(
                "crop is outside the image ({}x{})",
                image.width(),
                image.height()
            )));
        }
        image = image.crop_imm(crop.x, crop.y, crop.width, crop.height);
    }
    image = match transform.rotate {
        Some(90) => image.rotate90(),
        Some(180) => image.rotate180(),
        Some(270) => image.rotate270(),
        _ => image,
    };
    if transform.flip {
        image = image.flipv();
    }
    if transform.flop {
        image = image.fliph();
    }
    let (box_width, box_height) = (
        transform.max_width.unwrap_or(u32::MAX),
        transform.max_height.unwrap_or(u32::MAX),
    );
    if image.width() > box_width || image.height() > box_height {
        image = image.resize(box_width, box_height, FilterType::Lanczos3);
    }
    if transform.grayscale {
        image = DynamicImage::ImageLuma8(image.to_luma8());
    }

    let format = transform.format.unwrap_or(default);
    let (width, height) = (image.width(), image.height());
    let mut out = Vec::new();
    match format {
        Format::Png => PngEncoder::new(&mut out)
            .write_image(image.as_bytes(), width, height, image.color().into())
            .map_err(codec)?,
        Format::Webp => {
            let rgba = image.to_rgba8();
            WebPEncoder::new_lossless(&mut out)
                .write_image(
                    rgba.as_raw(),
                    width,
                    height,
                    image_codec::ExtendedColorType::Rgba8,
                )
                .map_err(codec)?
        }
        Format::Jpeg => {
            // JPEG has no alpha: transparent pixels are flattened onto white.
            let rgba = image.to_rgba8();
            let mut rgb = image_codec::RgbImage::new(width, height);
            for (to, from) in rgb.pixels_mut().zip(rgba.pixels()) {
                let alpha = u32::from(from[3]);
                for channel in 0..3 {
                    to[channel] =
                        ((u32::from(from[channel]) * alpha + 255 * (255 - alpha)) / 255) as u8;
                }
            }
            JpegEncoder::new_with_quality(&mut out, transform.quality.unwrap_or(85))
                .write_image(
                    rgb.as_raw(),
                    width,
                    height,
                    image_codec::ExtendedColorType::Rgb8,
                )
                .map_err(codec)?
        }
    }
    Ok(Transformed {
        bytes: out,
        format,
        width,
        height,
    })
}
