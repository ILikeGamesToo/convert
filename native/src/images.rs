// SPDX-License-Identifier: GPL-2.0-or-later

//! Image decoding, EXIF orientation, compositing and encoding.

use crate::error::ConvertError;
use crate::format::OutputFormat;
use crate::options::ConversionOptions;
use image::ImageEncoder;
use std::io::Cursor;

pub(crate) const MAX_SVG_DIMENSION: u32 = 8_192;

pub(crate) const MAX_SVG_PIXELS: u64 = 32_000_000;

pub(crate) const MAX_IMAGE_DIMENSION: u32 = 32_768;

pub(crate) const MAX_IMAGE_ALLOC: u64 = 512 * 1024 * 1024;

pub(crate) fn load_image(
    input: &[u8],
    options: ConversionOptions,
) -> Result<image::DynamicImage, ConvertError> {
    let mut reader = image::ImageReader::new(Cursor::new(input)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC);
    reader.limits(limits);
    let image = reader.decode()?;
    if !options.image_apply_exif_orientation {
        return Ok(image);
    }
    Ok(match exif_orientation(input) {
        Some(orientation) if orientation != 1 => apply_orientation(image, orientation),
        _ => image,
    })
}

/// Reads the EXIF orientation tag from JPEG, TIFF, or PNG (`eXIf`) input.
///
/// A duplicated orientation tag in the primary image IFD, or across multiple
/// primary EXIF segments, is ambiguous, so it is ignored rather than picking
/// one and potentially rotating the image the wrong way. The same is true of a
/// missing or malformed tag; callers then leave the pixels alone.
pub(crate) fn exif_orientation(input: &[u8]) -> Option<u16> {
    let mut orientations = Vec::new();
    if input.starts_with(&[0xFF, 0xD8]) {
        let mut position = 2;
        while position + 4 <= input.len() {
            if input[position] != 0xFF {
                position += 1;
                continue;
            }
            let marker = input[position + 1];
            if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
                position += 2;
                continue;
            }
            if marker == 0xDA || marker == 0xD9 {
                break;
            }
            let length = u16::from_be_bytes([input[position + 2], input[position + 3]]) as usize;
            if length < 2 {
                break;
            }
            if marker == 0xE1 {
                if let Some(segment) = input.get(position + 4..position + 2 + length) {
                    if let Some(tiff) = segment.strip_prefix(b"Exif\0\0") {
                        orientations.extend(tiff_orientations(tiff));
                    }
                }
            }
            position = match position.checked_add(2 + length) {
                Some(next) => next,
                None => break,
            };
        }
    } else if input.starts_with(b"II*\0") || input.starts_with(b"MM\0*") {
        orientations.extend(tiff_orientations(input));
    } else if input.starts_with(b"\x89PNG\r\n\x1a\n") {
        let mut position = 8;
        while position + 8 <= input.len() {
            let length = match input.get(position..position + 4) {
                Some(bytes) => u32::from_be_bytes(bytes.try_into().ok()?) as usize,
                None => break,
            };
            let kind = match input.get(position + 4..position + 8) {
                Some(kind) => kind,
                None => break,
            };
            let body_start = position + 8;
            let body_end = match body_start.checked_add(length) {
                Some(end) => end,
                None => break,
            };
            let body = match input.get(body_start..body_end) {
                Some(body) => body,
                None => break,
            };
            if kind == b"eXIf" {
                orientations.extend(tiff_orientations(body));
            }
            if kind == b"IEND" {
                break;
            }
            position = match position.checked_add(12 + length) {
                Some(next) => next,
                None => break,
            };
        }
    }
    match orientations.as_slice() {
        [orientation] => Some(*orientation),
        _ => None,
    }
}

/// Collects orientation tags from the primary image IFD (IFD0) only.
///
/// A thumbnail IFD can carry its own orientation tag, but that tag describes
/// the thumbnail rather than the main image. Returning every primary tag lets
/// the caller detect duplicates across EXIF segments.
pub(crate) fn tiff_orientations(tiff: &[u8]) -> Vec<u16> {
    let little = tiff.starts_with(b"II");
    let mut orientations = Vec::new();
    if !little && !tiff.starts_with(b"MM") {
        return orientations;
    }
    let read16 = |position: usize| -> Option<u16> {
        let bytes = tiff.get(position..position + 2)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    };
    let read32 = |position: usize| -> Option<u32> {
        let bytes = tiff.get(position..position + 4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };
    let ifd = match read32(4) {
        Some(ifd) => ifd as usize,
        None => return orientations,
    };
    let entries = match read16(ifd) {
        Some(entries) => usize::from(entries),
        None => return orientations,
    };
    for index in 0..entries {
        let entry = ifd + 2 + index * 12;
        if read16(entry) == Some(0x0112)
            && read16(entry + 2) == Some(3)
            && read32(entry + 4) == Some(1)
        {
            if let Some(value) = read16(entry + 8) {
                orientations.push(value);
            }
        }
    }
    orientations
}

/// Copies JPEG APP1 EXIF segments into a JPEG result. The pixels have already
/// been rotated when `image_apply_exif_orientation` is enabled, so the copied
/// orientation tag is reset to 1 to avoid rotating the result twice.
pub(crate) fn preserve_jpeg_exif(
    input: &[u8],
    output: Vec<u8>,
    options: ConversionOptions,
) -> Vec<u8> {
    if !input.starts_with(&[0xFF, 0xD8]) || !output.starts_with(&[0xFF, 0xD8]) {
        return output;
    }
    let segments = jpeg_exif_segments(input);
    if segments.is_empty() {
        return output;
    }
    let mut result =
        Vec::with_capacity(output.len() + segments.iter().map(Vec::len).sum::<usize>());
    result.extend_from_slice(&output[..2]);
    for mut segment in segments {
        if options.image_apply_exif_orientation && segment.len() > 6 {
            set_tiff_orientation(&mut segment[6..], 1);
        }
        if segment.len() + 2 > usize::from(u16::MAX) {
            continue;
        }
        result.extend_from_slice(&[0xFF, 0xE1]);
        result.extend_from_slice(&((segment.len() + 2) as u16).to_be_bytes());
        result.extend_from_slice(&segment);
    }
    result.extend_from_slice(&output[2..]);
    result
}

pub(crate) fn jpeg_exif_segments(input: &[u8]) -> Vec<Vec<u8>> {
    let mut segments = Vec::new();
    let mut position = 2;
    while position + 4 <= input.len() {
        if input[position] != 0xFF {
            position += 1;
            continue;
        }
        let marker = input[position + 1];
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            position += 2;
            continue;
        }
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        let length = u16::from_be_bytes([input[position + 2], input[position + 3]]) as usize;
        if length < 2 {
            break;
        }
        if marker == 0xE1 {
            if let Some(segment) = input.get(position + 4..position + 2 + length) {
                if segment.starts_with(b"Exif\0\0") {
                    segments.push(segment.to_vec());
                }
            }
        }
        position = match position.checked_add(2 + length) {
            Some(next) => next,
            None => break,
        };
    }
    segments
}

pub(crate) fn set_tiff_orientation(tiff: &mut [u8], orientation: u16) {
    let little = tiff.starts_with(b"II");
    if !little && !tiff.starts_with(b"MM") {
        return;
    }
    let read16 = |bytes: &[u8], position: usize| -> Option<u16> {
        let bytes = bytes.get(position..position + 2)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    };
    let read32 = |bytes: &[u8], position: usize| -> Option<u32> {
        let bytes = bytes.get(position..position + 4)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };
    let Some(ifd) = read32(tiff, 4).map(|offset| offset as usize) else {
        return;
    };
    let Some(entries) = read16(tiff, ifd).map(usize::from) else {
        return;
    };
    for index in 0..entries {
        let entry = ifd + 2 + index * 12;
        if read16(tiff, entry) == Some(0x0112)
            && read16(tiff, entry + 2) == Some(3)
            && read32(tiff, entry + 4) == Some(1)
        {
            let value = if little {
                orientation.to_le_bytes()
            } else {
                orientation.to_be_bytes()
            };
            if let Some(slot) = tiff.get_mut(entry + 8..entry + 10) {
                slot.copy_from_slice(&value);
            }
        }
    }
}

pub(crate) fn apply_orientation(
    image: image::DynamicImage,
    orientation: u16,
) -> image::DynamicImage {
    match orientation {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate270().fliph(),
        8 => image.rotate270(),
        _ => image,
    }
}

pub(crate) fn composite_channel(source: u8, alpha: u8, background: u8) -> u8 {
    let alpha = u16::from(alpha);
    let inverse = 255 - alpha;
    ((u16::from(source) * alpha + u16::from(background) * inverse + 127) / 255) as u8
}

/// Composites transparent pixels onto an opaque background using straight-alpha
/// blending: `source * alpha + background * (1 - alpha)`.
pub(crate) fn flatten_image(
    image: image::DynamicImage,
    background: [u8; 3],
) -> image::DynamicImage {
    if !image.color().has_alpha() {
        return image;
    }
    let rgba = image.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (pixel, output) in rgba.pixels().zip(rgb.pixels_mut()) {
        for channel in 0..3 {
            output[channel] = composite_channel(pixel[channel], pixel[3], background[channel]);
        }
    }
    image::DynamicImage::ImageRgb8(rgb)
}

pub(crate) fn encode_image(
    image: image::DynamicImage,
    target: OutputFormat,
    options: ConversionOptions,
) -> Result<Vec<u8>, ConvertError> {
    let format = target
        .image_format()
        .expect("encode_image is only called for image targets");

    let image = if target.uses_image_background() {
        flatten_image(image, options.image_background)
    } else {
        image
    };

    if matches!(
        target,
        OutputFormat::Ppm | OutputFormat::Pbm | OutputFormat::Pgm | OutputFormat::Pam
    ) {
        return encode_pnm_image(image, target);
    }

    let image = match format {
        image::ImageFormat::Hdr => image::DynamicImage::ImageRgb32F(image.to_rgb32f()),
        image::ImageFormat::Jpeg | image::ImageFormat::Bmp => {
            image::DynamicImage::ImageRgb8(image.to_rgb8())
        }
        image::ImageFormat::Ico => image::DynamicImage::ImageRgba8(fit_icon(image)),
        image::ImageFormat::Gif | image::ImageFormat::Tga | image::ImageFormat::Qoi => {
            image::DynamicImage::ImageRgba8(image.to_rgba8())
        }
        image::ImageFormat::Farbfeld => image::DynamicImage::ImageRgba16(image.to_rgba16()),
        _ => {
            if matches!(
                image.color(),
                image::ColorType::Rgb32F | image::ColorType::Rgba32F
            ) {
                image::DynamicImage::ImageRgba8(image.to_rgba8())
            } else {
                image
            }
        }
    };

    let mut output = Cursor::new(Vec::new());
    if format == image::ImageFormat::Jpeg {
        let quality = options
            .jpeg_quality
            .unwrap_or(ConversionOptions::DEFAULT_JPEG_QUALITY)
            .clamp(1, 100);
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, quality);
        image.write_with_encoder(encoder)?;
    } else if format == image::ImageFormat::Png {
        let compression = match options.png_compression {
            Some(level) => image::codecs::png::CompressionType::Level(level.clamp(1, 9)),
            None => image::codecs::png::CompressionType::Fast,
        };
        let encoder = image::codecs::png::PngEncoder::new_with_quality(
            &mut output,
            compression,
            image::codecs::png::FilterType::Adaptive,
        );
        image.write_with_encoder(encoder)?;
    } else {
        image.write_to(&mut output, format)?;
    }
    Ok(output.into_inner())
}

pub(crate) fn encode_pnm_image(
    image: image::DynamicImage,
    target: OutputFormat,
) -> Result<Vec<u8>, ConvertError> {
    use image::codecs::pnm::{PnmEncoder, PnmSubtype, SampleEncoding};

    let mut output = Cursor::new(Vec::new());
    match target {
        OutputFormat::Ppm => {
            let image = image.to_rgb8();
            PnmEncoder::new(&mut output)
                .with_subtype(PnmSubtype::Pixmap(SampleEncoding::Binary))
                .write_image(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    image::ExtendedColorType::Rgb8,
                )?;
        }
        OutputFormat::Pbm => {
            let image = image.to_luma8();
            let bits: Vec<u8> = image
                .pixels()
                .map(|pixel| u8::from(pixel[0] >= 128))
                .collect();
            PnmEncoder::new(&mut output)
                .with_subtype(PnmSubtype::Bitmap(SampleEncoding::Binary))
                .write_image(
                    &bits,
                    image.width(),
                    image.height(),
                    image::ExtendedColorType::L8,
                )?;
        }
        OutputFormat::Pgm => {
            let image = image.to_luma8();
            PnmEncoder::new(&mut output)
                .with_subtype(PnmSubtype::Graymap(SampleEncoding::Binary))
                .write_image(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    image::ExtendedColorType::L8,
                )?;
        }
        OutputFormat::Pam => {
            let image = image.to_rgba8();
            PnmEncoder::new(&mut output)
                .with_subtype(PnmSubtype::ArbitraryMap)
                .write_image(
                    image.as_raw(),
                    image.width(),
                    image.height(),
                    image::ExtendedColorType::Rgba8,
                )?;
        }
        _ => unreachable!("only PNM-family targets reach this helper"),
    }
    Ok(output.into_inner())
}

pub(crate) fn fit_icon(image: image::DynamicImage) -> image::RgbaImage {
    let rgba = image.to_rgba8();
    let (width, height) = rgba.dimensions();
    if width <= 256 && height <= 256 {
        return rgba;
    }
    let scale = 256.0 / width.max(height) as f32;
    let width = ((width as f32 * scale).round() as u32).max(1);
    let height = ((height as f32 * scale).round() as u32).max(1);
    image::imageops::resize(&rgba, width, height, image::imageops::FilterType::Lanczos3)
}

pub(crate) fn rasterize_svg(input: &[u8]) -> Result<image::DynamicImage, ConvertError> {
    let mut options = resvg::usvg::Options::default();
    let mut fontdb = resvg::usvg::fontdb::Database::new();
    fontdb.load_system_fonts();
    options.fontdb = std::sync::Arc::new(fontdb);

    let tree = resvg::usvg::Tree::from_data(input, &options)
        .map_err(|error| ConvertError::Svg(error.to_string()))?;
    let size = tree.size().to_int_size();
    if size.width() == 0 || size.height() == 0 {
        return Err(ConvertError::Svg("the SVG has no visible size".to_owned()));
    }
    let width = size.width();
    let height = size.height();
    if width > MAX_SVG_DIMENSION || height > MAX_SVG_DIMENSION {
        return Err(ConvertError::SvgTooLarge);
    }
    if u64::from(width) * u64::from(height) > MAX_SVG_PIXELS {
        return Err(ConvertError::SvgTooLarge);
    }

    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(width, height).ok_or(ConvertError::SvgTooLarge)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::default(),
        &mut pixmap.as_mut(),
    );
    let buffer = image::RgbaImage::from_raw(width, height, pixmap.take())
        .ok_or_else(|| ConvertError::Svg("could not read the rendered SVG pixels".to_owned()))?;
    Ok(image::DynamicImage::ImageRgba8(buffer))
}
