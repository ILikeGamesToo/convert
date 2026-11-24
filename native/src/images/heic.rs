// SPDX-License-Identifier: GPL-2.0-or-later

//! HEIC decoding.
//!
//! `heifer` decodes the HEIF container and the HEVC codestream in pure Rust: it
//! applies the `clap`/`irot`/`imir` container transforms, merges the alpha
//! auxiliary item, resolves grid tiles and colour-manages the samples to sRGB
//! from an embedded ICC profile when there is one. The EXIF orientation tag is
//! *not* applied by `heifer`, so it is read here and the shared rotation code
//! applies it, exactly as it does for JPEG and AVIF.

use crate::error::ConvertError;
use crate::images::MAX_IMAGE_ALLOC;
use heifer::heifer_isobmff::boxes::Property;

/// Decodes a HEIC file to RGBA.
///
/// 8-bit content becomes `Rgba8`; 10- and 12-bit content becomes `Rgba16` with
/// the samples normalized to the full 16-bit range, matching the AVIF path.
pub(crate) fn decode(input: &[u8]) -> Result<image::DynamicImage, ConvertError> {
    let options = heifer::Options::new().with_max_pixels(MAX_IMAGE_ALLOC / 4);
    let decoded = heifer::decode_srgb_with_options(input, &options).map_err(heic_error)?;
    let (width, height) = (decoded.width, decoded.height);
    if decoded.bit_depth > 8 {
        let max = u32::from(decoded.max_value());
        let samples = decoded
            .data
            .iter()
            .map(|&sample| ((u32::from(sample) * 65_535 + max / 2) / max) as u16)
            .collect();
        let buffer =
            image::ImageBuffer::<image::Rgba<u16>, Vec<u16>>::from_raw(width, height, samples)
                .ok_or_else(mismatched_buffer)?;
        return Ok(image::DynamicImage::ImageRgba16(buffer));
    }
    let buffer = image::RgbaImage::from_raw(width, height, decoded.to_rgba8())
        .ok_or_else(mismatched_buffer)?;
    Ok(image::DynamicImage::ImageRgba8(buffer))
}

/// The EXIF orientation tags a HEIC file carries, for the primary image.
///
/// `heifer` applies the container's `irot`/`imir` transforms itself, so a file
/// that carries one of those has already been rotated and its EXIF tag is
/// informational; returning nothing there keeps the image from being turned
/// twice. Apple's HEICs carry the rotation in EXIF instead, with no container
/// transform, which is the case this covers.
pub(crate) fn exif_orientations(input: &[u8]) -> Vec<u16> {
    use heifer::heifer_isobmff::HeifFile;

    let Ok(file) = HeifFile::parse(input) else {
        return Vec::new();
    };
    if container_orients_pixels(input) {
        return Vec::new();
    }
    let Ok(Some(exif)) = file.exif(file.primary_id) else {
        return Vec::new();
    };
    heifer::Exif::parse(&exif)
        .and_then(|exif| exif.orientation())
        .map(|orientation| vec![orientation])
        .unwrap_or_default()
}

/// The EXIF TIFF stream carried by the file's `Exif` item, if any.
///
/// `heifer` strips the `ExifDataBlock` offset and an optional `Exif\0\0`
/// prefix, so the bytes can be handed to another container as-is.
pub(crate) fn exif_stream(input: &[u8]) -> Option<Vec<u8>> {
    use heifer::heifer_isobmff::HeifFile;

    let file = HeifFile::parse(input).ok()?;
    file.exif(file.primary_id).ok()?
}

/// Whether `heifer` rotated the decoded pixels from an `irot`/`imir` property,
/// which leaves the EXIF orientation tag describing a rotation that has
/// already happened.
pub(crate) fn container_orients_pixels(input: &[u8]) -> bool {
    use heifer::heifer_isobmff::HeifFile;

    let Ok(file) = HeifFile::parse(input) else {
        return false;
    };
    let Ok(mut properties) = file.item_properties(file.primary_id) else {
        return false;
    };
    properties.any(|property| matches!(property, Property::Rotation(_) | Property::Mirror(_)))
}

fn heic_error(error: heifer::Error) -> ConvertError {
    ConvertError::Heic(error.to_string())
}

fn mismatched_buffer() -> ConvertError {
    ConvertError::Heic("the decoded image does not match its dimensions".to_owned())
}

#[cfg(test)]
mod tests {
    use crate::convert::convert_bytes_with_options;
    use crate::format::OutputFormat;
    use crate::options::ConversionOptions;
    use crate::test_support::fixture_path;

    fn decode_to_rgba(name: &str, options: ConversionOptions) -> image::RgbaImage {
        let input = std::fs::read(fixture_path(name)).unwrap();
        let converted =
            convert_bytes_with_options(name, &input, OutputFormat::Png, options).unwrap();
        image::load_from_memory(&converted[0].bytes)
            .unwrap()
            .to_rgba8()
    }

    #[test]
    fn decodes_alpha_and_the_exif_orientation() {
        // 64x32, left half opaque red and right half half-transparent green,
        // with EXIF orientation 6: the decoded image is 32x64 with red on top,
        // green at the bottom and an alpha of 128 in the green half.
        let decoded = decode_to_rgba(
            "sample-heic-alpha-orientation.heic",
            ConversionOptions::default(),
        );
        assert_eq!((decoded.width(), decoded.height()), (32, 64));
        let red = decoded.get_pixel(16, 8).0;
        let green = decoded.get_pixel(16, 56).0;
        for (channel, expected) in [254_u32, 0, 0].iter().enumerate() {
            assert!(
                u32::from(red[channel]).abs_diff(*expected) <= 3,
                "the top half is {red:?}, expected the red source"
            );
        }
        assert_eq!(red[3], 255, "the top half should be opaque");
        for (channel, expected) in [0_u32, 255, 1].iter().enumerate() {
            assert!(
                u32::from(green[channel]).abs_diff(*expected) <= 3,
                "the bottom half is {green:?}, expected the green source"
            );
        }
        assert!(
            i32::from(green[3]).abs_diff(128) <= 3,
            "the bottom half alpha is {}",
            green[3]
        );
    }

    #[test]
    fn honours_the_no_exif_orientation_option() {
        let decoded = decode_to_rgba(
            "sample-heic-alpha-orientation.heic",
            ConversionOptions {
                image_apply_exif_orientation: false,
                ..Default::default()
            },
        );
        assert_eq!((decoded.width(), decoded.height()), (64, 32));
        assert_eq!(decoded.get_pixel(16, 8).0[3], 255);
    }

    #[test]
    fn decodes_a_public_photo_fixture() {
        // libheif's example image: 1280x854, no alpha, no EXIF orientation. The
        // sampled pixels are what FFmpeg 9.1's decode of the same file gives,
        // within a few counts of colour-conversion rounding.
        let decoded = decode_to_rgba("sample-heic-photo.heic", ConversionOptions::default());
        assert_eq!((decoded.width(), decoded.height()), (1280, 854));
        for (point, expected) in [
            ((0, 0), [166_u32, 174, 206]),
            ((640, 427), [142_u32, 133, 88]),
            ((1279, 853), [163_u32, 130, 33]),
        ] {
            let pixel = decoded.get_pixel(point.0, point.1).0;
            for (channel, want) in expected.iter().enumerate() {
                assert!(
                    u32::from(pixel[channel]).abs_diff(*want) <= 8,
                    "pixel {point:?} is {pixel:?}, expected {expected:?}"
                );
            }
            assert_eq!(pixel[3], 255);
        }
        assert!(
            decoded.pixels().all(|pixel| pixel.0[3] == 255),
            "the fixture has no alpha channel, so every pixel should be opaque"
        );
    }

    #[test]
    fn reports_a_typed_error_for_a_heic_brand_with_no_image_in_it() {
        // The brand says HEIC, so detection routes it to the HEIC decoder, which
        // then refuses it. (An AVIF payload named `.heic` decodes fine: content
        // wins over the extension everywhere in this app.)
        let mut input = 24_u32.to_be_bytes().to_vec();
        input.extend_from_slice(b"ftypheic");
        input.extend_from_slice(&0_u32.to_be_bytes());
        input.extend_from_slice(b"heicmif1");
        input.extend_from_slice(b"not an image at all");
        let error = crate::convert::convert_bytes_with_options(
            "photo.heic",
            &input,
            OutputFormat::Png,
            ConversionOptions::default(),
        )
        .unwrap_err();
        assert!(
            matches!(error, crate::error::ConvertError::Heic(_)),
            "expected a typed HEIC error, got {error:?}"
        );
    }

    #[test]
    fn reports_a_typed_error_for_a_truncated_heic() {
        let mut input = std::fs::read(fixture_path("sample-heic-photo.heic")).unwrap();
        input.truncate(2048);
        let error = crate::convert::convert_bytes_with_options(
            "photo.heic",
            &input,
            OutputFormat::Png,
            ConversionOptions::default(),
        )
        .unwrap_err();
        assert!(
            matches!(error, crate::error::ConvertError::Heic(_)),
            "expected a typed HEIC error, got {error:?}"
        );
    }
}
