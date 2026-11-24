// SPDX-License-Identifier: GPL-2.0-or-later

//! AVIF decoding.
//!
//! `gamut-avif` owns the ISO BMFF container: it validates the primary item,
//! resolves `grid`/`iden`/`iovl` derivations, merges the alpha auxiliary item,
//! applies the `clap`/`irot`/`imir` transformative properties and converts the
//! planar samples to RGBA. The AV1 codestream itself is decoded by `rav1d`, the
//! pure-Rust port of `dav1d`, through the `Av1StillDecoder` hook.

use crate::error::ConvertError;
use crate::images::{MAX_IMAGE_ALLOC, MAX_IMAGE_DIMENSION};
use gamut_avif::{
    Av1Config, Av1StillDecoder, AvifContainer, ChromaFormat, DecodeLimits, DecodedFrame,
};
use gamut_core::{Error as GamutError, Result as GamutResult};
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{
    DAV1D_PIXEL_LAYOUT_I400, DAV1D_PIXEL_LAYOUT_I420, DAV1D_PIXEL_LAYOUT_I422,
    DAV1D_PIXEL_LAYOUT_I444,
};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_get_picture,
    dav1d_open, dav1d_picture_unref, dav1d_send_data,
};
use rav1d::Dav1dResult;
use std::mem::MaybeUninit;
use std::ptr::NonNull;

/// Decodes an AVIF file to RGBA.
///
/// 8-bit content becomes `Rgba8`; 10- and 12-bit content becomes `Rgba16` with
/// the samples normalized to the full 16-bit range, so the deeper output
/// formats keep their precision instead of being narrowed here.
pub(crate) fn decode(input: &[u8]) -> Result<image::DynamicImage, ConvertError> {
    let container = AvifContainer::parse(input).map_err(avif_error)?;
    let still = container.image();
    if !still.is_av1_still() {
        return Err(ConvertError::Avif(
            "the file is not an AV1 still image; HEIC/HEIF input is not supported".to_owned(),
        ));
    }
    let mut decoder = Rav1dDecoder::new()?;
    if coded_bit_depth(still).unwrap_or(8) > 8 {
        // RGBA16 is eight bytes per pixel, RGBA8 four, so the pixel cap that
        // keeps the decoded buffer inside `MAX_IMAGE_ALLOC` differs per path.
        let limits = DecodeLimits::new(MAX_IMAGE_DIMENSION, MAX_IMAGE_ALLOC / 8);
        let decoded = still
            .decode_primary_rgba16_with_limits(&mut decoder, limits)
            .map_err(avif_error)?;
        let (width, height) = (decoded.width(), decoded.height());
        let buffer = image::ImageBuffer::<image::Rgba<u16>, Vec<u16>>::from_raw(
            width,
            height,
            decoded.into_samples(),
        )
        .ok_or_else(mismatched_buffer)?;
        return Ok(image::DynamicImage::ImageRgba16(buffer));
    }
    let limits = DecodeLimits::new(MAX_IMAGE_DIMENSION, MAX_IMAGE_ALLOC / 4);
    let decoded = still
        .decode_primary_rgba8_with_limits(&mut decoder, limits)
        .map_err(avif_error)?;
    let (width, height) = (decoded.width(), decoded.height());
    let buffer = image::RgbaImage::from_raw(width, height, decoded.into_samples())
        .ok_or_else(mismatched_buffer)?;
    Ok(image::DynamicImage::ImageRgba8(buffer))
}

/// The EXIF orientation tags carried by an AVIF file's `Exif` metadata item.
///
/// HEIF/AVIF wraps the TIFF stream in an `ExifDataBlock` whose first four bytes
/// are the offset of the TIFF header, so the header is either right after the
/// offset, at the offset, or at the offset counted from the block start; the
/// writers disagree, so every position that carries a TIFF signature is tried.
pub(crate) fn exif_orientations(input: &[u8]) -> Vec<u16> {
    let Ok(container) = AvifContainer::parse(input) else {
        return Vec::new();
    };
    let Some(exif) = container.image().exif() else {
        return Vec::new();
    };
    let payload = &exif.as_isobmff_item().payload;
    let Some(offset) = payload
        .get(0..4)
        .map(|bytes| u32::from_be_bytes(bytes.try_into().expect("four bytes")) as usize)
    else {
        return Vec::new();
    };
    [4 + offset, offset, 4]
        .into_iter()
        .find_map(|start| {
            let tiff = payload.get(start..)?;
            let magic = tiff.starts_with(b"II*\0") || tiff.starts_with(b"MM\0*");
            magic.then_some(tiff)
        })
        .map(crate::images::tiff_orientations)
        .unwrap_or_default()
}

/// The coded bit depth of the primary image, from its `av1C` record.
///
/// A `grid` primary item carries no `av1C` of its own, so the tiles it derives
/// from are consulted instead.
fn coded_bit_depth(still: &gamut_avif::AvifImage) -> Option<u8> {
    let primary = still.primary_item();
    if let Some(Ok(config)) = primary.av1_config() {
        return Some(config.bit_depth());
    }
    still
        .derivation_sources(primary.id())
        .into_iter()
        .find_map(|source| match source.av1_config() {
            Some(Ok(config)) => Some(config.bit_depth()),
            _ => None,
        })
}

fn avif_error(error: GamutError) -> ConvertError {
    ConvertError::Avif(error.to_string())
}

fn mismatched_buffer() -> ConvertError {
    ConvertError::Avif("the decoded image does not match its dimensions".to_owned())
}

fn decode_error(message: &'static str) -> GamutError {
    GamutError::invalid_input("convert2it", message)
}

/// `rav1d` reports success as 0 and failure as a negated `errno`.
fn check(status: Dav1dResult) -> GamutResult<()> {
    if status.0 == 0 {
        Ok(())
    } else {
        Err(decode_error("the AV1 codestream could not be decoded"))
    }
}

/// One `rav1d` decoding context, reused for the primary item and its alpha
/// auxiliary.
struct Rav1dDecoder {
    context: Option<Dav1dContext>,
}

impl Rav1dDecoder {
    fn new() -> Result<Self, ConvertError> {
        let threads = std::thread::available_parallelism()
            .map_or(4, |threads| threads.get())
            .min(16);
        // SAFETY: `settings` is only read after `dav1d_default_settings` has
        // written it, and the context it produces is released in `Drop`.
        unsafe {
            let mut settings = MaybeUninit::<Dav1dSettings>::uninit();
            dav1d_default_settings(NonNull::from(&mut settings).cast());
            let settings = settings.assume_init_mut();
            settings.n_threads = threads as _;
            // A still image is a single frame, so nothing can be pipelined:
            // keeping one frame context makes the frame available immediately.
            settings.max_frame_delay = 1;
            // Film grain is part of the decoded pixels, and the AVIF encoders
            // apply it too, so it must not be left to the caller.
            settings.apply_grain = 1;
            let mut context = None;
            let status = dav1d_open(
                Some(NonNull::from(&mut context)),
                Some(NonNull::from(settings)),
            );
            if status.0 != 0 {
                return Err(ConvertError::Avif(
                    "the AV1 decoder could not be started".to_owned(),
                ));
            }
            Ok(Self { context })
        }
    }
}

impl Av1StillDecoder for Rav1dDecoder {
    fn decode_still(&mut self, config: &Av1Config, payload: &[u8]) -> GamutResult<DecodedFrame> {
        let Some(context) = self.context else {
            return Err(decode_error("the AV1 decoder is closed"));
        };
        // One `av01` item is one temporal unit: a temporal delimiter, the
        // `av1C` configuration OBUs and then the item's own OBUs, every OBU
        // carrying its size so the decoder can read them as a single stream.
        let mut stream = Vec::new();
        config.full_stream(payload, &mut stream)?;
        // SAFETY: `context` comes from `dav1d_open`, `data` and `picture` are
        // released before they go out of scope, and `stream` outlives the copy
        // `dav1d_data_create` makes of it.
        unsafe {
            let mut data = Dav1dData::default();
            let buffer = dav1d_data_create(Some(NonNull::from(&mut data)), stream.len());
            let Some(buffer) = NonNull::new(buffer) else {
                dav1d_data_unref(Some(NonNull::from(&mut data)));
                return Err(decode_error("the AV1 stream could not be buffered"));
            };
            std::ptr::copy_nonoverlapping(stream.as_ptr(), buffer.as_ptr(), stream.len());
            let sent = dav1d_send_data(Some(context), Some(NonNull::from(&mut data)));
            dav1d_data_unref(Some(NonNull::from(&mut data)));
            check(sent)?;

            // A progressively decodable item produces one picture per layer and
            // only the last one is the complete image, so keep the newest.
            let mut newest = None;
            loop {
                let mut picture = Dav1dPicture::default();
                if dav1d_get_picture(Some(context), Some(NonNull::from(&mut picture))).0 != 0 {
                    dav1d_picture_unref(Some(NonNull::from(&mut picture)));
                    break;
                }
                if let Some(mut previous) = newest.replace(picture) {
                    dav1d_picture_unref(Some(NonNull::from(&mut previous)));
                }
            }
            let Some(picture) = newest else {
                return Err(decode_error("the AV1 item produced no image"));
            };
            let frame = frame_from_picture(&picture);
            dav1d_picture_unref(Some(NonNull::from(&picture)));
            frame
        }
    }
}

impl Drop for Rav1dDecoder {
    fn drop(&mut self) {
        // SAFETY: the context is released exactly once, here, and every picture
        // taken from it was released by `decode_still`.
        unsafe { dav1d_close(Some(NonNull::from(&mut self.context))) };
    }
}

/// Copies a decoded picture's planes into the planar frame `gamut-avif` expects.
///
/// # Safety
///
/// `picture` must be a decoded picture that has not been unreferenced yet, and
/// its planes must still be alive for the duration of the copy.
unsafe fn frame_from_picture(picture: &Dav1dPicture) -> GamutResult<DecodedFrame> {
    let width = u32::try_from(picture.p.w).map_err(|_| decode_error("negative image width"))?;
    let height = u32::try_from(picture.p.h).map_err(|_| decode_error("negative image height"))?;
    let bit_depth = u8::try_from(picture.p.bpc).map_err(|_| decode_error("bad bit depth"))?;
    let chroma = match picture.p.layout {
        DAV1D_PIXEL_LAYOUT_I400 => ChromaFormat::Monochrome,
        DAV1D_PIXEL_LAYOUT_I420 => ChromaFormat::Yuv420,
        DAV1D_PIXEL_LAYOUT_I422 => ChromaFormat::Yuv422,
        DAV1D_PIXEL_LAYOUT_I444 => ChromaFormat::Yuv444,
        _ => return Err(decode_error("unknown pixel layout")),
    };
    let sample_bytes = if bit_depth <= 8 { 1 } else { 2 };
    let (chroma_width, chroma_height) = chroma.chroma_dimensions(width, height);
    let plane_count = if chroma == ChromaFormat::Monochrome {
        1
    } else {
        3
    };
    let mut planes = Vec::with_capacity(plane_count);
    for plane in 0..plane_count {
        let (plane_width, plane_height) = if plane == 0 {
            (width, height)
        } else {
            (chroma_width, chroma_height)
        };
        let data = picture.data[plane].ok_or_else(|| decode_error("a plane is missing"))?;
        let stride_bytes = picture.stride[usize::from(plane != 0)];
        if stride_bytes <= 0 || !(stride_bytes as usize).is_multiple_of(sample_bytes) {
            return Err(decode_error("a plane has an unusable stride"));
        }
        let stride = stride_bytes as usize / sample_bytes;
        let row_length = plane_width as usize * sample_bytes;
        if stride < plane_width as usize {
            return Err(decode_error("a plane is narrower than its stride"));
        }
        let mut samples = Vec::with_capacity(plane_width as usize * plane_height as usize);
        // SAFETY: the decoder allocates `stride * height` bytes for the plane,
        // and both loops stay inside `plane_width` columns of `plane_height`
        // rows.
        let base = data.as_ptr().cast::<u8>();
        for row in 0..plane_height as usize {
            let start = base.add(row * stride * sample_bytes);
            if sample_bytes == 1 {
                let row = std::slice::from_raw_parts(start, row_length);
                samples.extend(row.iter().map(|&sample| u16::from(sample)));
            } else {
                let row = std::slice::from_raw_parts(start.cast::<u16>(), plane_width as usize);
                samples.extend(row.iter().map(|&sample| u16::from_le(sample)));
            }
        }
        planes.push(samples);
    }
    let (y, cb, cr) = match planes.len() {
        1 => (planes.remove(0), Vec::new(), Vec::new()),
        _ => {
            let mut planes = planes.into_iter();
            (
                planes.next().expect("luma plane"),
                planes.next().expect("chroma plane"),
                planes.next().expect("chroma plane"),
            )
        }
    };
    DecodedFrame::new(width, height, bit_depth, chroma, y, cb, cr)
}

#[cfg(test)]
mod tests {
    use crate::convert::convert_bytes;
    use crate::format::OutputFormat;
    use crate::test_support::fixture_path;

    fn decode_to_png(name: &str) -> image::RgbaImage {
        let input = std::fs::read(fixture_path(name)).unwrap();
        let converted = convert_bytes(name, &input, OutputFormat::Png).unwrap();
        image::load_from_memory(&converted[0].bytes)
            .unwrap()
            .to_rgba8()
    }

    /// The quadrants of `sample-avif-quad*.avif`, a 64×64 image made of four
    /// 32×32 solid blocks. FFmpeg's `color` source writes them as 253/0/0,
    /// 0/254/0, 0/0/254 and 255/255/255, so those are the expected colours.
    const QUADRANTS: [[u8; 3]; 4] = [[253, 0, 0], [0, 254, 0], [0, 0, 254], [255, 255, 255]];

    fn assert_quadrants(decoded: &image::RgbaImage, tolerance: i32) {
        assert_eq!((decoded.width(), decoded.height()), (64, 64));
        for (index, expected) in QUADRANTS.iter().enumerate() {
            let (x, y) = match index {
                0 => (16, 16),
                1 => (48, 16),
                2 => (16, 48),
                _ => (48, 48),
            };
            let pixel = decoded.get_pixel(x, y).0;
            for channel in 0..3 {
                let difference = i32::from(pixel[channel]) - i32::from(expected[channel]);
                assert!(
                    difference.abs() <= tolerance,
                    "quadrant {index} channel {channel} is {pixel:?}, expected {expected:?}"
                );
            }
            assert_eq!(pixel[3], 255, "quadrant {index} is not opaque");
        }
    }

    #[test]
    fn decodes_a_known_8_bit_image_within_tolerance() {
        assert_quadrants(&decode_to_png("sample-avif-quad.avif"), 3);
    }

    #[test]
    fn decodes_a_known_10_bit_image_within_tolerance() {
        // The 10-bit source keeps its depth, so the decoded PNG is 16-bit;
        // narrowing it here only makes the expected colours comparable.
        assert_quadrants(&decode_to_png("sample-avif-quad-10bit.avif"), 3);
    }

    #[test]
    fn decodes_the_alpha_auxiliary_and_the_rotation() {
        // `irot` turns the 256×256 source a quarter turn, so the decoded image
        // is 256×512, and the alpha auxiliary item carries the translucency.
        let decoded = decode_to_png("sample-avif-alpha-irot.avif");
        assert_eq!((decoded.width(), decoded.height()), (256, 512));
        let partial = decoded
            .pixels()
            .filter(|pixel| pixel.0[3] > 0 && pixel.0[3] < 255)
            .count();
        assert!(partial > 0, "the alpha channel was not merged");
    }

    #[test]
    fn assembles_a_grid_image() {
        // A 2×2 grid of 40×40 tiles with a non-grid alpha item.
        let decoded = decode_to_png("sample-avif-grid.avif");
        assert_eq!((decoded.width(), decoded.height()), (80, 80));
    }

    #[test]
    fn decodes_the_still_of_an_image_sequence() {
        // An `avis` file: the primary still is decoded and the track is ignored.
        let decoded = decode_to_png("sample-avif-sequence.avif");
        assert_eq!((decoded.width(), decoded.height()), (150, 150));
    }

    #[test]
    fn recognises_the_avif_brands() {
        fn ftyp(major: &[u8; 4], compatible: &[u8]) -> Vec<u8> {
            let size = 16 + compatible.len() as u32;
            let mut bytes = size.to_be_bytes().to_vec();
            bytes.extend_from_slice(b"ftyp");
            bytes.extend_from_slice(major);
            bytes.extend_from_slice(&0_u32.to_be_bytes());
            bytes.extend_from_slice(compatible);
            bytes
        }

        // HEIC is an ISO BMFF image too, but its brand is not AVIF's.
        assert!(!crate::shared::is_avif_input(&ftyp(b"heic", b"heicmif1")));
        assert!(crate::shared::is_avif_input(&ftyp(b"avif", b"avifmif1")));
        assert!(crate::shared::is_avif_input(&ftyp(b"avis", b"avismif1")));
        // A generic `mif1` file declares AVIF among the compatible brands.
        assert!(crate::shared::is_avif_input(&ftyp(
            b"mif1",
            b"mif1miafavif"
        )));
        assert!(!crate::shared::is_avif_input(&ftyp(b"isom", b"isomiso2")));
        assert!(!crate::shared::is_avif_input(b"not an ISO BMFF file"));
    }

    /// A minimal AVIF whose `Exif` item carries an orientation tag, built with
    /// the container writer `gamut-avif` itself uses so that no fixture with an
    /// orientation tag has to be committed.
    fn avif_with_orientation(orientation: u16) -> Vec<u8> {
        use gamut_isobmff::{write, IsoBmffImage, Item, ItemReference, Property, PropertyKind};

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II*\0");
        tiff.extend_from_slice(&8_u32.to_le_bytes());
        tiff.extend_from_slice(&1_u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112_u16.to_le_bytes());
        tiff.extend_from_slice(&3_u16.to_le_bytes());
        tiff.extend_from_slice(&1_u32.to_le_bytes());
        tiff.extend_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&0_u32.to_le_bytes());
        let mut exif = 0_u32.to_be_bytes().to_vec();
        exif.extend_from_slice(&tiff);

        let image = IsoBmffImage::new(
            *b"avif",
            vec![*b"avif", *b"mif1", *b"miaf"],
            1,
            vec![
                Item {
                    id: 1,
                    item_type: *b"av01",
                    name: String::new(),
                    content_type: None,
                    content_encoding: None,
                    hidden: false,
                    references: Vec::new(),
                    properties: vec![Property {
                        essential: true,
                        kind: PropertyKind::ImageSpatialExtents {
                            width: 2,
                            height: 1,
                        },
                    }],
                    payload: vec![0x12, 0x00],
                },
                Item {
                    id: 2,
                    item_type: *b"Exif",
                    name: String::new(),
                    content_type: None,
                    content_encoding: None,
                    hidden: false,
                    references: vec![ItemReference {
                        reference_type: *b"cdsc",
                        to_item_ids: vec![1],
                    }],
                    properties: Vec::new(),
                    payload: exif,
                },
            ],
        );
        write(&image).unwrap()
    }

    #[test]
    fn reads_the_exif_orientation_from_the_exif_item() {
        assert_eq!(
            crate::images::exif_orientation(&avif_with_orientation(6)),
            Some(6)
        );
        assert_eq!(
            crate::images::exif_orientation(&avif_with_orientation(1)),
            Some(1)
        );
    }

    #[test]
    fn ignores_an_avif_without_an_exif_item() {
        let input = std::fs::read(fixture_path("sample-avif-quad.avif")).unwrap();
        assert_eq!(crate::images::exif_orientation(&input), None);
    }
}
