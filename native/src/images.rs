// SPDX-License-Identifier: GPL-2.0-or-later

//! Image decoding, EXIF orientation, compositing and encoding.

use crate::error::ConvertError;
use crate::format::OutputFormat;
use crate::options::ConversionOptions;
use image::ImageEncoder;
use std::io::Cursor;

pub(crate) mod avif;
pub(crate) mod heic;

pub(crate) const MAX_SVG_DIMENSION: u32 = 8_192;

pub(crate) const MAX_SVG_PIXELS: u64 = 32_000_000;

pub(crate) const MAX_IMAGE_DIMENSION: u32 = 32_768;

pub(crate) const MAX_IMAGE_ALLOC: u64 = 512 * 1024 * 1024;

pub(crate) fn load_image(
    input: &[u8],
    options: ConversionOptions,
) -> Result<image::DynamicImage, ConvertError> {
    let image = if crate::shared::is_avif_input(input) {
        avif::decode(input)?
    } else if crate::shared::is_heic_input(input) {
        heic::decode(input)?
    } else {
        decode_guessed_image(input)?
    };
    if !options.image_apply_exif_orientation {
        return Ok(image);
    }
    Ok(match exif_orientation(input) {
        Some(orientation) if orientation != 1 => apply_orientation(image, orientation),
        _ => image,
    })
}

fn decode_guessed_image(input: &[u8]) -> Result<image::DynamicImage, ConvertError> {
    let mut reader = image::ImageReader::new(Cursor::new(input)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC);
    reader.limits(limits);
    Ok(reader.decode()?)
}

/// Reads the EXIF orientation tag from JPEG, TIFF, PNG (`eXIf`), AVIF and HEIC
/// (the `Exif` metadata item) input.
///
/// A duplicated orientation tag in the primary image IFD, or across multiple
/// primary EXIF segments, is ambiguous, so it is ignored rather than picking
/// one and potentially rotating the image the wrong way. The same is true of a
/// missing or malformed tag; callers then leave the pixels alone.
pub(crate) fn exif_orientation(input: &[u8]) -> Option<u16> {
    let mut orientations = Vec::new();
    if crate::shared::is_avif_input(input) {
        orientations.extend(avif::exif_orientations(input));
    } else if crate::shared::is_heic_input(input) {
        orientations.extend(heic::exif_orientations(input));
    } else if input.starts_with(&[0xFF, 0xD8]) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::{convert_bytes, convert_bytes_with_options};
    use crate::error::ConvertError;
    use crate::format::OutputFormat;
    use crate::options::ConversionOptions;
    use crate::test_support::{png_fixture, tiff_with_orientations};
    use std::{io::Cursor, path::PathBuf};
    fn detailed_png_fixture() -> Vec<u8> {
        let mut pixels = image::RgbImage::new(64, 64);
        for (x, y, pixel) in pixels.enumerate_pixels_mut() {
            *pixel = image::Rgb([
                (x * 4) as u8,
                (y * 4) as u8,
                ((x * 17 + y * 29) % 256) as u8,
            ]);
        }
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(pixels)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }
    #[test]
    fn converts_between_supported_image_formats() {
        let result = convert_bytes("sample.png", &png_fixture(), OutputFormat::Webp).unwrap();
        assert_eq!(result[0].name, PathBuf::from("sample.webp"));
        let decoded =
            image::load_from_memory_with_format(&result[0].bytes, image::ImageFormat::WebP)
                .unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 2));
    }
    #[test]
    fn applies_jpeg_quality_and_png_compression_options() {
        let input = detailed_png_fixture();
        let low_quality = convert_bytes_with_options(
            "sample.png",
            &input,
            OutputFormat::Jpeg,
            ConversionOptions {
                jpeg_quality: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        let high_quality = convert_bytes_with_options(
            "sample.png",
            &input,
            OutputFormat::Jpeg,
            ConversionOptions {
                jpeg_quality: Some(95),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(low_quality[0].name, PathBuf::from("sample.jpg"));
        assert_eq!(high_quality[0].name, PathBuf::from("sample.jpg"));
        assert!(
            high_quality[0].bytes.len() > low_quality[0].bytes.len(),
            "higher JPEG quality should produce a larger file"
        );
        assert!(image::load_from_memory_with_format(
            &high_quality[0].bytes,
            image::ImageFormat::Jpeg
        )
        .is_ok());

        let fast_png = convert_bytes_with_options(
            "sample.png",
            &input,
            OutputFormat::Png,
            ConversionOptions {
                png_compression: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let best_png = convert_bytes_with_options(
            "sample.png",
            &input,
            OutputFormat::Png,
            ConversionOptions {
                png_compression: Some(9),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(fast_png[0].name, PathBuf::from("sample.png"));
        assert_eq!(best_png[0].name, PathBuf::from("sample.png"));
        assert!(
            best_png[0].bytes.len() <= fast_png[0].bytes.len(),
            "higher PNG compression should not produce a larger file"
        );
        assert!(
            image::load_from_memory_with_format(&best_png[0].bytes, image::ImageFormat::Png)
                .is_ok()
        );
    }
    #[test]
    fn converts_to_bmp_and_tiff_outputs() {
        for (target, extension, format) in [
            (OutputFormat::Bmp, "bmp", image::ImageFormat::Bmp),
            (OutputFormat::Tiff, "tiff", image::ImageFormat::Tiff),
        ] {
            let result = convert_bytes("sample.png", &png_fixture(), target).unwrap();
            assert_eq!(result[0].name, PathBuf::from(format!("sample.{extension}")));

            let decoded = image::load_from_memory_with_format(&result[0].bytes, format).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (2, 2));
        }
    }
    #[test]
    fn converts_bmp_and_tiff_inputs_to_png() {
        for (extension, format) in [
            ("bmp", image::ImageFormat::Bmp),
            ("tif", image::ImageFormat::Tiff),
            ("tiff", image::ImageFormat::Tiff),
        ] {
            let mut input = Cursor::new(Vec::new());
            image::load_from_memory(&png_fixture())
                .unwrap()
                .write_to(&mut input, format)
                .unwrap();

            let result = convert_bytes(
                &format!("sample.{extension}"),
                &input.into_inner(),
                OutputFormat::Png,
            )
            .unwrap();
            let decoded =
                image::load_from_memory_with_format(&result[0].bytes, image::ImageFormat::Png)
                    .unwrap();
            assert_eq!((decoded.width(), decoded.height()), (2, 2));
        }
    }
    #[test]
    fn converts_png_to_the_new_image_formats() {
        for (target, format) in [
            (OutputFormat::Gif, image::ImageFormat::Gif),
            (OutputFormat::Ico, image::ImageFormat::Ico),
            (OutputFormat::Ppm, image::ImageFormat::Pnm),
            (OutputFormat::Tga, image::ImageFormat::Tga),
            (OutputFormat::Qoi, image::ImageFormat::Qoi),
            (OutputFormat::Hdr, image::ImageFormat::Hdr),
            (OutputFormat::Farbfeld, image::ImageFormat::Farbfeld),
        ] {
            let result = convert_bytes("sample.png", &png_fixture(), target).unwrap();
            let decoded = image::load_from_memory_with_format(&result[0].bytes, format).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (2, 2));
        }
    }
    #[test]
    fn converts_png_to_pnm_variants() {
        for (target, extension, magic) in [
            (OutputFormat::Pbm, "pbm", &b"P4"[..]),
            (OutputFormat::Pgm, "pgm", &b"P5"[..]),
            (OutputFormat::Pam, "pam", &b"P7"[..]),
        ] {
            let converted = convert_bytes("sample.png", &png_fixture(), target).unwrap();
            assert_eq!(
                converted[0].name,
                PathBuf::from(format!("sample.{extension}"))
            );
            assert!(
                converted[0].bytes.starts_with(magic),
                "{extension} output is missing its magic number"
            );
            let decoded = image::load_from_memory(&converted[0].bytes).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (2, 2));
            if target == OutputFormat::Pbm {
                assert!(
                    decoded.to_luma8().pixels().all(|pixel| pixel[0] == 0),
                    "dark pixels should stay black in a PBM"
                );
            }
        }
    }
    #[test]
    fn rasterizes_svg_images_to_raster_formats() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#ff0000"/></svg>"##;
        let result = convert_bytes("icon.svg", svg, OutputFormat::Png).unwrap();
        assert_eq!(result[0].name, PathBuf::from("icon.png"));

        let decoded =
            image::load_from_memory_with_format(&result[0].bytes, image::ImageFormat::Png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4, 4));
        let pixel = decoded.to_rgba8().get_pixel(1, 1).0;
        assert!(pixel[0] > 200 && pixel[1] < 40 && pixel[2] < 40);
    }
    #[test]
    fn rejects_oversized_svgs_before_allocating_a_pixmap() {
        let too_wide = br##"<svg xmlns="http://www.w3.org/2000/svg" width="9000" height="10"><rect width="9000" height="10"/></svg>"##;
        assert!(matches!(
            convert_bytes("huge.svg", too_wide, OutputFormat::Png),
            Err(ConvertError::SvgTooLarge)
        ));

        let too_many_pixels = br##"<svg xmlns="http://www.w3.org/2000/svg" width="8000" height="8000"><rect width="8000" height="8000"/></svg>"##;
        assert!(matches!(
            convert_bytes("huge.svg", too_many_pixels, OutputFormat::Png),
            Err(ConvertError::SvgTooLarge)
        ));
    }
    fn tiff_with_primary_and_thumbnail_orientations(primary: u16, thumbnail: u16) -> Vec<u8> {
        fn push_ifd(tiff: &mut Vec<u8>, orientation: u16, next_ifd: u32) {
            tiff.extend_from_slice(&1_u16.to_le_bytes());
            tiff.extend_from_slice(&0x0112_u16.to_le_bytes());
            tiff.extend_from_slice(&3_u16.to_le_bytes());
            tiff.extend_from_slice(&1_u32.to_le_bytes());
            tiff.extend_from_slice(&orientation.to_le_bytes());
            tiff.extend_from_slice(&[0, 0]);
            tiff.extend_from_slice(&next_ifd.to_le_bytes());
        }

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II*\0");
        tiff.extend_from_slice(&8_u32.to_le_bytes());
        let thumbnail_ifd = 8 + 2 + 12 + 4;
        push_ifd(&mut tiff, primary, thumbnail_ifd);
        push_ifd(&mut tiff, thumbnail, 0);
        tiff
    }
    fn jpeg_with_exif_orientations(image: &image::DynamicImage, orientations: &[u16]) -> Vec<u8> {
        jpeg_with_exif_tiff(image, &tiff_with_orientations(orientations))
    }
    fn jpeg_with_exif_tiff(image: &image::DynamicImage, tiff: &[u8]) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        image
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut cursor,
                100,
            ))
            .unwrap();
        let jpeg = cursor.into_inner();
        let mut result = jpeg[..2].to_vec();
        let mut segment = b"Exif\0\0".to_vec();
        segment.extend_from_slice(tiff);
        result.extend_from_slice(&[0xFF, 0xE1]);
        result.extend_from_slice(&((segment.len() + 2) as u16).to_be_bytes());
        result.extend_from_slice(&segment);
        result.extend_from_slice(&jpeg[2..]);
        result
    }
    fn assert_rgb_near(actual: [u8; 3], expected: [u8; 3], tolerance: u8, label: &str) {
        for (channel, (actual, expected)) in actual.iter().zip(expected).enumerate() {
            assert!(
                actual.abs_diff(expected) <= tolerance,
                "{label}: channel {channel} got {actual}, expected {expected}"
            );
        }
    }
    fn expected_composite(source: [u8; 3], alpha: u8, background: [u8; 3]) -> [u8; 3] {
        let alpha = u16::from(alpha);
        let inverse = 255 - alpha;
        std::array::from_fn(|channel| {
            ((u16::from(source[channel]) * alpha + u16::from(background[channel]) * inverse + 127)
                / 255) as u8
        })
    }
    #[test]
    fn preserves_jpeg_exif_without_double_rotating() {
        let source = image::RgbImage::from_fn(8, 8, |x, _| {
            if x < 4 {
                image::Rgb([255, 0, 0])
            } else {
                image::Rgb([0, 0, 255])
            }
        });
        let input = jpeg_with_exif_orientations(&image::DynamicImage::ImageRgb8(source), &[6]);

        let rotated = convert_bytes("photo.jpg", &input, OutputFormat::Jpeg).unwrap();
        assert_eq!(exif_orientation(&rotated[0].bytes), Some(1));
        assert_eq!(
            image::load_from_memory(&rotated[0].bytes).unwrap().width(),
            8
        );

        let unrotated = convert_bytes_with_options(
            "photo.jpg",
            &input,
            OutputFormat::Jpeg,
            ConversionOptions {
                image_apply_exif_orientation: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(exif_orientation(&unrotated[0].bytes), Some(6));
    }
    #[test]
    fn exif_orientation_rotates_corners_and_ignores_duplicate_or_invalid_tags() {
        let mut source = image::RgbImage::new(16, 16);
        for (x, y, pixel) in source.enumerate_pixels_mut() {
            *pixel = match (x < 8, y < 8) {
                (true, true) => image::Rgb([255, 0, 0]),
                (false, true) => image::Rgb([0, 255, 0]),
                (true, false) => image::Rgb([0, 0, 255]),
                (false, false) => image::Rgb([255, 255, 0]),
            };
        }
        let source = image::DynamicImage::ImageRgb8(source);

        let oriented = jpeg_with_exif_orientations(&source, &[6]);
        assert_eq!(super::exif_orientation(&oriented), Some(6));
        let converted = convert_bytes("photo.jpg", &oriented, OutputFormat::Png).unwrap();
        let decoded = image::load_from_memory(&converted[0].bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (16, 16));
        let pixels = decoded.to_rgb8();
        assert_rgb_near(pixels.get_pixel(4, 4).0, [0, 0, 255], 35, "top-left");
        assert_rgb_near(pixels.get_pixel(12, 4).0, [255, 0, 0], 35, "top-right");
        assert_rgb_near(pixels.get_pixel(4, 12).0, [255, 255, 0], 35, "bottom-left");
        assert_rgb_near(pixels.get_pixel(12, 12).0, [0, 255, 0], 35, "bottom-right");

        let plain = jpeg_with_exif_orientations(&source, &[]);
        assert_eq!(super::exif_orientation(&plain), None);
        let plain = image::load_from_memory(
            &convert_bytes("plain.jpg", &plain, OutputFormat::Png).unwrap()[0].bytes,
        )
        .unwrap()
        .to_rgb8();
        assert_rgb_near(plain.get_pixel(4, 4).0, [255, 0, 0], 35, "plain top-left");

        let invalid = jpeg_with_exif_orientations(&source, &[9]);
        assert_eq!(super::exif_orientation(&invalid), Some(9));
        let invalid = image::load_from_memory(
            &convert_bytes("invalid.jpg", &invalid, OutputFormat::Png).unwrap()[0].bytes,
        )
        .unwrap()
        .to_rgb8();
        assert_rgb_near(
            invalid.get_pixel(4, 4).0,
            [255, 0, 0],
            35,
            "invalid top-left",
        );

        let duplicate = jpeg_with_exif_orientations(&source, &[6, 6]);
        assert_eq!(super::exif_orientation(&duplicate), None);
        let duplicate = image::load_from_memory(
            &convert_bytes("duplicate.jpg", &duplicate, OutputFormat::Png).unwrap()[0].bytes,
        )
        .unwrap()
        .to_rgb8();
        assert_rgb_near(
            duplicate.get_pixel(4, 4).0,
            [255, 0, 0],
            35,
            "duplicate top-left",
        );

        let disabled = convert_bytes_with_options(
            "photo.jpg",
            &oriented,
            OutputFormat::Png,
            ConversionOptions {
                image_apply_exif_orientation: false,
                ..Default::default()
            },
        )
        .unwrap();
        let disabled = image::load_from_memory(&disabled[0].bytes).unwrap();
        assert_eq!((disabled.width(), disabled.height()), (16, 16));
        assert_rgb_near(
            disabled.to_rgb8().get_pixel(4, 4).0,
            [255, 0, 0],
            35,
            "disabled top-left",
        );
    }
    #[test]
    fn exif_orientation_ignores_thumbnail_ifd_orientation() {
        let mut source = image::RgbImage::new(2, 1);
        source.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        source.put_pixel(1, 0, image::Rgb([0, 0, 255]));
        let source = image::DynamicImage::ImageRgb8(source);
        let jpeg =
            jpeg_with_exif_tiff(&source, &tiff_with_primary_and_thumbnail_orientations(6, 3));

        assert_eq!(super::exif_orientation(&jpeg), Some(6));
        let converted = convert_bytes("photo.jpg", &jpeg, OutputFormat::Png).unwrap();
        let decoded = image::load_from_memory(&converted[0].bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1, 2));
        let pixels = decoded.to_rgb8();
        let top = pixels.get_pixel(0, 0).0;
        let bottom = pixels.get_pixel(0, 1).0;
        assert!(top[0] > 200 && top[2] < 60, "top pixel is {top:?}");
        assert!(
            bottom[2] > 200 && bottom[0] < 60,
            "bottom pixel is {bottom:?}"
        );
    }
    #[test]
    fn applies_exif_orientation_when_loading_images() {
        let mut image = image::RgbImage::new(2, 1);
        image.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        image.put_pixel(1, 0, image::Rgb([0, 0, 255]));
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II*\0");
        tiff.extend_from_slice(&8_u32.to_le_bytes());
        tiff.extend_from_slice(&1_u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112_u16.to_le_bytes());
        tiff.extend_from_slice(&3_u16.to_le_bytes());
        tiff.extend_from_slice(&1_u32.to_le_bytes());
        tiff.extend_from_slice(&6_u16.to_le_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&0_u32.to_le_bytes());
        let mut segment = b"Exif\0\0".to_vec();
        segment.extend_from_slice(&tiff);
        let mut oriented = jpeg[..2].to_vec();
        oriented.extend_from_slice(&[0xFF, 0xE1]);
        oriented.extend_from_slice(&((segment.len() + 2) as u16).to_be_bytes());
        oriented.extend_from_slice(&segment);
        oriented.extend_from_slice(&jpeg[2..]);

        assert_eq!(super::exif_orientation(&oriented), Some(6));
        let converted = convert_bytes("photo.jpg", &oriented, OutputFormat::Png).unwrap();
        let decoded = image::load_from_memory(&converted[0].bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1, 2));
        let pixels = decoded.to_rgb8();
        let top = pixels.get_pixel(0, 0).0;
        let bottom = pixels.get_pixel(0, 1).0;
        assert!(top[0] > 200 && top[2] < 60, "top pixel is {top:?}");
        assert!(
            bottom[2] > 200 && bottom[0] < 60,
            "bottom pixel is {bottom:?}"
        );

        let unrotated = convert_bytes_with_options(
            "photo.jpg",
            &oriented,
            OutputFormat::Png,
            ConversionOptions {
                image_apply_exif_orientation: false,
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = image::load_from_memory(&unrotated[0].bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 1));
    }
    #[test]
    fn composites_transparency_onto_the_configured_background() {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            1,
            1,
            image::Rgba([255, 0, 0, 0]),
        ))
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();

        let bmp = convert_bytes_with_options(
            "clear.png",
            &png,
            OutputFormat::Bmp,
            ConversionOptions {
                image_background: [0, 0, 255],
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = image::load_from_memory(&bmp[0].bytes).unwrap().to_rgb8();
        assert_eq!(decoded.get_pixel(0, 0).0, [0, 0, 255]);

        let default = convert_bytes("clear.png", &png, OutputFormat::Bmp).unwrap();
        let decoded = image::load_from_memory(&default[0].bytes)
            .unwrap()
            .to_rgb8();
        assert_eq!(decoded.get_pixel(0, 0).0, [255, 255, 255]);
    }
    #[test]
    fn composites_transparency_into_jpeg_and_bmp_with_two_backgrounds() {
        let transparent = image::Rgba([255, 0, 0, 0]);
        let semi_transparent = image::Rgba([200, 100, 50, 128]);
        let opaque = image::Rgba([10, 200, 30, 255]);
        let mut rgba = image::RgbaImage::new(24, 8);
        for (x, _, pixel) in rgba.enumerate_pixels_mut() {
            *pixel = match x {
                0..=7 => transparent,
                8..=15 => semi_transparent,
                _ => opaque,
            };
        }
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        for background in [[0, 0, 255], [12, 34, 56]] {
            let expected = [
                expected_composite([255, 0, 0], 0, background),
                expected_composite([200, 100, 50], 128, background),
                expected_composite([10, 200, 30], 255, background),
            ];
            for (target, format, tolerance) in [
                (OutputFormat::Jpeg, image::ImageFormat::Jpeg, 25),
                (OutputFormat::Bmp, image::ImageFormat::Bmp, 0),
            ] {
                let converted = convert_bytes_with_options(
                    "clear.png",
                    &png,
                    target,
                    ConversionOptions {
                        image_background: background,
                        jpeg_quality: Some(100),
                        ..Default::default()
                    },
                )
                .unwrap();
                let decoded = image::load_from_memory_with_format(&converted[0].bytes, format)
                    .unwrap()
                    .to_rgb8();
                assert_eq!((decoded.width(), decoded.height()), (24, 8));
                for (x, expected, label) in [
                    (4, expected[0], "transparent area"),
                    (12, expected[1], "semi-transparent area"),
                    (20, expected[2], "opaque area"),
                ] {
                    assert_rgb_near(decoded.get_pixel(x, 4).0, expected, tolerance, label);
                }
            }
        }
    }
    #[test]
    fn alpha_preserving_targets_ignore_the_configured_background() {
        let pixels = [
            image::Rgba([255, 0, 0, 0]),
            image::Rgba([200, 100, 50, 128]),
            image::Rgba([10, 200, 30, 255]),
        ];
        let mut rgba = image::RgbaImage::new(3, 1);
        for (x, pixel) in pixels.iter().enumerate() {
            rgba.put_pixel(x as u32, 0, *pixel);
        }
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        for (target, format) in [
            (OutputFormat::Png, image::ImageFormat::Png),
            (OutputFormat::Webp, image::ImageFormat::WebP),
        ] {
            let converted = convert_bytes_with_options(
                "alpha.png",
                &png,
                target,
                ConversionOptions {
                    image_background: [1, 2, 3],
                    ..Default::default()
                },
            )
            .unwrap();
            let decoded = image::load_from_memory_with_format(&converted[0].bytes, format).unwrap();
            assert!(
                decoded.color().has_alpha(),
                "{target:?} output lost its alpha channel"
            );
            let decoded = decoded.to_rgba8();
            assert_eq!((decoded.width(), decoded.height()), (3, 1));
            for (x, expected) in pixels.iter().enumerate() {
                assert_eq!(*decoded.get_pixel(x as u32, 0), *expected);
            }
        }
    }
    #[test]
    fn animated_gif_converts_to_a_single_frame() {
        use image::codecs::gif::{GifEncoder, Repeat};

        let mut gif = Vec::new();
        let mut encoder = GifEncoder::new(&mut gif);
        encoder.set_repeat(Repeat::Infinite).unwrap();
        encoder
            .encode_frames([
                image::Frame::new(image::RgbaImage::from_pixel(
                    2,
                    2,
                    image::Rgba([255, 0, 0, 255]),
                )),
                image::Frame::new(image::RgbaImage::from_pixel(
                    2,
                    2,
                    image::Rgba([0, 0, 255, 255]),
                )),
            ])
            .unwrap();
        drop(encoder);

        let converted = convert_bytes("animated.gif", &gif, OutputFormat::Png).unwrap();
        let decoded = image::load_from_memory(&converted[0].bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 2));
        let pixel = decoded.to_rgb8().get_pixel(0, 0).0;
        assert!(
            pixel[0] > 200 && pixel[2] < 60,
            "the first GIF frame was not retained: {pixel:?}"
        );
    }
}
