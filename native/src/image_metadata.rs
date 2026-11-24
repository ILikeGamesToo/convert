// SPDX-License-Identifier: GPL-2.0-or-later

//! EXIF and ICC profile carry-over for image conversions.
//!
//! A conversion decodes the source pixels and re-encodes them, so anything a
//! container carries beside the pixels is lost unless it is copied across
//! explicitly. This module reads the EXIF TIFF stream and the ICC profile from
//! the input container and hands them to the encoder, per target capability:
//!
//! - JPEG keeps EXIF in an APP1 segment and the profile in APP2 segments.
//! - PNG keeps EXIF in `eXIf` and the profile in `iCCP`.
//! - WebP keeps EXIF in `EXIF` and the profile in `ICCP`.
//! - TIFF keeps EXIF as tags in its image directory and the profile in tag
//!   34675, which is why TIFF output has its own encoder in `images.rs`.
//!
//! Every other image target (GIF, BMP, ICO, PNM, TGA, QOI, HDR, Farbfeld, PDF)
//! has nowhere to put either block, so they are dropped there by design.
//!
//! AVIF and HEIC inputs contribute EXIF but not their ICC profile: both
//! decoders colour-manage to sRGB, so the pixels no longer carry the numbers
//! the source profile describes and copying it across would mis-tag them.

use image::ImageDecoder;
use std::io::Cursor;

/// Tag numbers this module reads or rewrites.
pub(crate) const TAG_ORIENTATION: u16 = 0x0112;
const TAG_EXIF_IFD: u16 = 0x8769;
const TAG_GPS_IFD: u16 = 0x8825;
const TAG_ICC_PROFILE: u16 = 0x8773;
const TAG_MAKER_NOTE: u16 = 0x927C;
const TAG_INTEROP_IFD: u16 = 0xA005;
const TAG_PIXEL_X_DIMENSION: u16 = 0xA002;
const TAG_PIXEL_Y_DIMENSION: u16 = 0xA003;

/// IFD0 tags that describe the picture rather than its pixel layout. A TIFF
/// source is its own EXIF container, so these are the tags carried out of it;
/// the structural ones (dimensions, strip offsets, sample format) describe the
/// file being replaced and would be wrong in the new one.
const DESCRIPTIVE_IFD0_TAGS: [u16; 12] = [
    0x010E, // ImageDescription
    0x010F, // Make
    0x0110, // Model
    TAG_ORIENTATION,
    0x011A, // XResolution
    0x011B, // YResolution
    0x0128, // ResolutionUnit
    0x0131, // Software
    0x0132, // DateTime
    0x013B, // Artist
    0x013C, // HostComputer
    0x8298, // Copyright
];

/// The metadata blocks an image input carried that an output can also carry.
#[derive(Clone, Debug, Default)]
pub(crate) struct ImageMetadata {
    /// A raw EXIF TIFF stream, with no JPEG `Exif\0\0` prefix.
    pub(crate) exif: Option<Vec<u8>>,
    /// A raw ICC colour profile.
    pub(crate) icc: Option<Vec<u8>>,
}

impl ImageMetadata {
    pub(crate) fn is_empty(&self) -> bool {
        self.exif.is_none() && self.icc.is_none()
    }

    /// Drops the blocks a JPEG cannot store.
    ///
    /// The JPEG encoder writes EXIF into one APP1 segment and the profile into
    /// APP2 chunks, both length-prefixed with a 16-bit count. A PNG `eXIf`
    /// chunk or a WebP `EXIF` chunk has no such limit, so an oversized block
    /// has to be dropped rather than turned into a failed conversion.
    pub(crate) fn capped_for_jpeg(&self) -> ImageMetadata {
        const MAX_EXIF: usize = u16::MAX as usize - 6 - 2;
        const MAX_ICC: usize = (u16::MAX as usize - 16) * 255;
        ImageMetadata {
            exif: self
                .exif
                .as_ref()
                .filter(|exif| exif.len() <= MAX_EXIF)
                .cloned(),
            icc: self
                .icc
                .as_ref()
                .filter(|icc| icc.len() <= MAX_ICC)
                .cloned(),
        }
    }

    /// Resets the orientation tag to 1, for an output whose pixels are already
    /// in display orientation. Leaving a stale tag there is what makes an
    /// image rotate twice in the next application that reads it.
    pub(crate) fn reset_orientation(&mut self) {
        if let Some(exif) = self.exif.as_mut() {
            crate::images::set_tiff_orientation(exif, 1);
        }
    }
}

/// Extracts EXIF and the ICC profile from a supported image input.
///
/// Inputs with neither block, or with a format this app does not decode,
/// return the empty value and the conversion proceeds unchanged.
pub(crate) fn extract(input: &[u8]) -> ImageMetadata {
    if crate::shared::is_avif_input(input) {
        return ImageMetadata {
            exif: crate::images::avif::exif_stream(input),
            icc: None,
        };
    }
    if crate::shared::is_heic_input(input) {
        return ImageMetadata {
            exif: crate::images::heic::exif_stream(input),
            icc: None,
        };
    }
    if is_tiff(input) {
        return tiff_metadata(input);
    }

    let mut metadata = decoder_metadata(input);
    if input.starts_with(&[0xFF, 0xD8]) && jpeg_component_count(input) == Some(4) {
        // A CMYK or YCCK JPEG is converted to RGB by the decoder, so its
        // CMYK profile would describe numbers the output no longer has.
        metadata.icc = None;
    }
    if let Some(exif) = metadata.exif.as_mut() {
        let prefix = exif_prefix_len(exif);
        exif.drain(..prefix);
        if !is_tiff(exif) {
            metadata.exif = None;
        }
    }
    if metadata.exif.is_none() && metadata.icc.is_none() {
        return ImageMetadata::default();
    }
    metadata
}

/// Whether the input decoder rotates the pixels from a container transform
/// that the app cannot decline, which makes the EXIF orientation tag
/// informational and therefore stale in the output.
pub(crate) fn container_orients_pixels(input: &[u8]) -> bool {
    if crate::shared::is_avif_input(input) {
        return crate::images::avif::container_orients_pixels(input);
    }
    if crate::shared::is_heic_input(input) {
        return crate::images::heic::container_orients_pixels(input);
    }
    false
}

fn decoder_metadata(input: &[u8]) -> ImageMetadata {
    let Ok(reader) = image::ImageReader::new(Cursor::new(input)).with_guessed_format() else {
        return ImageMetadata::default();
    };
    let Ok(mut decoder) = reader.into_decoder() else {
        return ImageMetadata::default();
    };
    ImageMetadata {
        exif: decoder.exif_metadata().ok().flatten(),
        icc: decoder
            .icc_profile()
            .ok()
            .flatten()
            .filter(|icc| !icc.is_empty()),
    }
}

fn tiff_metadata(input: &[u8]) -> ImageMetadata {
    let Some(directories) = parse_exif(input) else {
        return ImageMetadata::default();
    };
    let icc = directories
        .ifd0
        .iter()
        .find(|field| field.tag == TAG_ICC_PROFILE)
        .and_then(|field| field.value.as_bytes())
        .filter(|icc| !icc.is_empty())
        .map(<[u8]>::to_vec);
    let ifd0: Vec<ExifField> = directories
        .ifd0
        .iter()
        .filter(|field| DESCRIPTIVE_IFD0_TAGS.contains(&field.tag))
        .cloned()
        .collect();
    let exif = if ifd0.is_empty() && directories.exif.is_empty() && directories.gps.is_empty() {
        None
    } else {
        Some(build_exif(&ExifDirectories {
            ifd0,
            exif: directories.exif,
            gps: directories.gps,
            little_endian: directories.little_endian,
        }))
    };
    ImageMetadata { exif, icc }
}

pub(crate) fn is_tiff(input: &[u8]) -> bool {
    input.starts_with(b"II*\0") || input.starts_with(b"MM\0*")
}

fn exif_prefix_len(input: &[u8]) -> usize {
    if input.starts_with(b"Exif\0\0") {
        6
    } else {
        0
    }
}

/// The number of components of a JPEG's frame, from its start-of-frame marker.
fn jpeg_component_count(input: &[u8]) -> Option<u8> {
    let mut position = 2;
    while position + 4 <= input.len() {
        if input[position] != 0xFF {
            position += 1;
            continue;
        }
        let marker = input[position + 1];
        if marker == 0xFF {
            position += 1;
            continue;
        }
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            position += 2;
            continue;
        }
        if marker == 0xDA || marker == 0xD9 {
            return None;
        }
        let length = u16::from_be_bytes([input[position + 2], input[position + 3]]) as usize;
        if length < 2 {
            return None;
        }
        let start_of_frame =
            (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if start_of_frame {
            return input.get(position + 9).copied();
        }
        position = position.checked_add(2 + length)?;
    }
    None
}

/// One EXIF (TIFF) tag with its value already dereferenced.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExifField {
    pub(crate) tag: u16,
    pub(crate) value: ExifValue,
}

/// A TIFF field value in the type the source declared.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ExifValue {
    Ascii(Vec<u8>),
    Short(Vec<u16>),
    Long(Vec<u32>),
    Rational(Vec<(u32, u32)>),
    SShort(Vec<i16>),
    SLong(Vec<i32>),
    SRational(Vec<(i32, i32)>),
    Float(Vec<f32>),
    Double(Vec<f64>),
    /// `BYTE`, `SBYTE` and `UNDEFINED`; the bytes are passed through as-is.
    Bytes(Vec<u8>),
}

impl ExifValue {
    pub(crate) fn kind(&self) -> u16 {
        match self {
            Self::Bytes(_) => 7,
            Self::Ascii(_) => 2,
            Self::Short(_) => 3,
            Self::Long(_) => 4,
            Self::Rational(_) => 5,
            Self::SShort(_) => 8,
            Self::SLong(_) => 9,
            Self::SRational(_) => 10,
            Self::Float(_) => 11,
            Self::Double(_) => 12,
        }
    }

    pub(crate) fn count(&self) -> u32 {
        match self {
            Self::Ascii(bytes) | Self::Bytes(bytes) => bytes.len() as u32,
            Self::Short(values) => values.len() as u32,
            Self::Long(values) => values.len() as u32,
            Self::Rational(values) => values.len() as u32,
            Self::SShort(values) => values.len() as u32,
            Self::SLong(values) => values.len() as u32,
            Self::SRational(values) => values.len() as u32,
            Self::Float(values) => values.len() as u32,
            Self::Double(values) => values.len() as u32,
        }
    }

    pub(crate) fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Ascii(bytes) | Self::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn as_str(&self) -> Option<&str> {
        let Self::Ascii(bytes) = self else {
            return None;
        };
        let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
        std::str::from_utf8(bytes).ok()
    }

    fn parse(kind: u16, bytes: &[u8], little: bool) -> Option<Self> {
        let read16 = |chunk: &[u8]| -> u16 {
            let bytes: [u8; 2] = chunk.try_into().expect("two bytes");
            if little {
                u16::from_le_bytes(bytes)
            } else {
                u16::from_be_bytes(bytes)
            }
        };
        let read32 = |chunk: &[u8]| -> u32 {
            let bytes: [u8; 4] = chunk.try_into().expect("four bytes");
            if little {
                u32::from_le_bytes(bytes)
            } else {
                u32::from_be_bytes(bytes)
            }
        };
        let chunked = |size: usize| bytes.chunks_exact(size);
        let value = match kind {
            1 | 6 | 7 => Self::Bytes(bytes.to_vec()),
            2 => Self::Ascii(bytes.to_vec()),
            3 => Self::Short(chunked(2).map(read16).collect()),
            4 => Self::Long(chunked(4).map(read32).collect()),
            5 => Self::Rational(
                chunked(8)
                    .map(|pair| (read32(&pair[..4]), read32(&pair[4..])))
                    .collect(),
            ),
            8 => Self::SShort(chunked(2).map(|pair| read16(pair) as i16).collect()),
            9 => Self::SLong(chunked(4).map(|value| read32(value) as i32).collect()),
            10 => Self::SRational(
                chunked(8)
                    .map(|pair| (read32(&pair[..4]) as i32, read32(&pair[4..]) as i32))
                    .collect(),
            ),
            11 => Self::Float(
                chunked(4)
                    .map(|value| f32::from_bits(read32(value)))
                    .collect(),
            ),
            12 => Self::Double(
                chunked(8)
                    .map(|value| {
                        let (high, low) = if little {
                            (read32(&value[4..]), read32(&value[..4]))
                        } else {
                            (read32(&value[..4]), read32(&value[4..]))
                        };
                        f64::from_bits((u64::from(high) << 32) | u64::from(low))
                    })
                    .collect(),
            ),
            _ => return None,
        };
        Some(value)
    }

    /// Serializes the value in the requested byte order.
    fn to_bytes(&self, little: bool) -> Vec<u8> {
        fn push16(out: &mut Vec<u8>, value: u16, little: bool) {
            out.extend_from_slice(&if little {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            });
        }
        fn push32(out: &mut Vec<u8>, value: u32, little: bool) {
            out.extend_from_slice(&if little {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            });
        }
        let mut out = Vec::new();
        match self {
            Self::Ascii(bytes) => {
                out.extend_from_slice(bytes);
                if !bytes.ends_with(&[0]) {
                    out.push(0);
                }
            }
            Self::Bytes(bytes) => out.extend_from_slice(bytes),
            Self::Short(values) => values
                .iter()
                .for_each(|&value| push16(&mut out, value, little)),
            Self::Long(values) => values
                .iter()
                .for_each(|&value| push32(&mut out, value, little)),
            Self::Rational(values) => {
                for &(numerator, denominator) in values {
                    push32(&mut out, numerator, little);
                    push32(&mut out, denominator, little);
                }
            }
            Self::SShort(values) => values
                .iter()
                .for_each(|&value| push16(&mut out, value as u16, little)),
            Self::SLong(values) => values
                .iter()
                .for_each(|&value| push32(&mut out, value as u32, little)),
            Self::SRational(values) => {
                for &(numerator, denominator) in values {
                    push32(&mut out, numerator as u32, little);
                    push32(&mut out, denominator as u32, little);
                }
            }
            Self::Float(values) => values
                .iter()
                .for_each(|&value| push32(&mut out, value.to_bits(), little)),
            Self::Double(values) => values.iter().for_each(|&value| {
                let bits = value.to_bits();
                if little {
                    push32(&mut out, bits as u32, little);
                    push32(&mut out, (bits >> 32) as u32, little);
                } else {
                    push32(&mut out, (bits >> 32) as u32, little);
                    push32(&mut out, bits as u32, little);
                }
            }),
        }
        out
    }
}

/// The three directories an EXIF block is built from.
#[derive(Clone, Debug)]
pub(crate) struct ExifDirectories {
    pub(crate) ifd0: Vec<ExifField>,
    pub(crate) exif: Vec<ExifField>,
    pub(crate) gps: Vec<ExifField>,
    pub(crate) little_endian: bool,
}

/// The IFD0 fields that can be written verbatim into another container.
/// Pointer tags are excluded: their values are offsets into the source file,
/// and the caller writes its own pointers for whatever it actually carries.
pub(crate) fn carried_ifd0(fields: &[ExifField]) -> impl Iterator<Item = &ExifField> {
    fields
        .iter()
        .filter(|field| !matches!(field.tag, TAG_EXIF_IFD | TAG_GPS_IFD | TAG_ICC_PROFILE))
}

/// Reads the IFD0, ExifIFD and GPS IFD out of an EXIF TIFF stream.
///
/// The MakerNote and Interoperability IFDs are not carried: both contain
/// offsets that refer to their original file, so a relocated copy would point
/// at the wrong bytes. A thumbnail IFD (IFD1) describes the source image and is
/// skipped for the same reason.
pub(crate) fn parse_exif(tiff: &[u8]) -> Option<ExifDirectories> {
    let little_endian = match tiff.get(..4)? {
        b"II*\0" => true,
        b"MM\0*" => false,
        _ => return None,
    };
    let ifd0_offset = read_u32(tiff, 4, little_endian)? as usize;
    let ifd0 = read_ifd(tiff, ifd0_offset, little_endian)?;
    let pointer = |tag: u16| -> Option<usize> {
        ifd0.iter()
            .find(|field| field.tag == tag)
            .and_then(|field| match &field.value {
                ExifValue::Long(values) => values.first().copied(),
                ExifValue::Short(values) => values.first().map(|&value| u32::from(value)),
                _ => None,
            })
            .map(|offset| offset as usize)
    };
    let exif = pointer(TAG_EXIF_IFD)
        .and_then(|offset| read_ifd(tiff, offset, little_endian))
        .map(|fields| {
            fields
                .into_iter()
                .filter(|field| {
                    !matches!(
                        field.tag,
                        TAG_MAKER_NOTE
                            | TAG_INTEROP_IFD
                            | TAG_PIXEL_X_DIMENSION
                            | TAG_PIXEL_Y_DIMENSION
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let gps = pointer(TAG_GPS_IFD)
        .and_then(|offset| read_ifd(tiff, offset, little_endian))
        .unwrap_or_default();
    Some(ExifDirectories {
        ifd0,
        exif,
        gps,
        little_endian,
    })
}

fn read_ifd(tiff: &[u8], offset: usize, little: bool) -> Option<Vec<ExifField>> {
    let count = usize::from(read_u16(tiff, offset, little)?);
    let mut fields = Vec::with_capacity(count.min(64));
    for index in 0..count.min(1024) {
        let entry = offset.checked_add(2 + index * 12)?;
        let tag = read_u16(tiff, entry, little)?;
        let kind = read_u16(tiff, entry + 2, little)?;
        let count = read_u32(tiff, entry + 4, little)? as usize;
        let size = type_size(kind)?.checked_mul(count)?;
        let bytes = if size <= 4 {
            tiff.get(entry + 8..entry + 8 + size)?.to_vec()
        } else {
            let start = read_u32(tiff, entry + 8, little)? as usize;
            tiff.get(start..start.checked_add(size)?)?.to_vec()
        };
        if let Some(value) = ExifValue::parse(kind, &bytes, little) {
            fields.push(ExifField { tag, value });
        }
    }
    Some(fields)
}

fn type_size(kind: u16) -> Option<usize> {
    Some(match kind {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => return None,
    })
}

fn read_u16(bytes: &[u8], at: usize, little: bool) -> Option<u16> {
    let value: [u8; 2] = bytes.get(at..at.checked_add(2)?)?.try_into().ok()?;
    Some(if little {
        u16::from_le_bytes(value)
    } else {
        u16::from_be_bytes(value)
    })
}

fn read_u32(bytes: &[u8], at: usize, little: bool) -> Option<u32> {
    let value: [u8; 4] = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(if little {
        u32::from_le_bytes(value)
    } else {
        u32::from_be_bytes(value)
    })
}

/// Builds a standalone EXIF TIFF stream from the three directories.
///
/// The layout is header, IFD0, IFD0 values, ExifIFD, its values, GPS IFD, its
/// values, with the pointer tags in IFD0 filled in. A directory with no fields
/// is omitted rather than written empty.
pub(crate) fn build_exif(directories: &ExifDirectories) -> Vec<u8> {
    let little = directories.little_endian;
    let mut ifd0: Vec<ExifField> = directories
        .ifd0
        .iter()
        .filter(|field| !matches!(field.tag, TAG_EXIF_IFD | TAG_GPS_IFD))
        .cloned()
        .collect();
    let has_exif = !directories.exif.is_empty();
    let has_gps = !directories.gps.is_empty();
    let ifd0_entries = ifd0.len() + usize::from(has_exif) + usize::from(has_gps);
    let ifd0_len = directory_len(&ifd0, ifd0_entries, little);
    let exif_offset = 8 + ifd0_len;
    let exif_len = directory_len(&directories.exif, directories.exif.len(), little);
    let gps_offset = exif_offset + exif_len;

    if has_exif {
        ifd0.push(ExifField {
            tag: TAG_EXIF_IFD,
            value: ExifValue::Long(vec![exif_offset as u32]),
        });
    }
    if has_gps {
        ifd0.push(ExifField {
            tag: TAG_GPS_IFD,
            value: ExifValue::Long(vec![gps_offset as u32]),
        });
    }

    let mut output = Vec::with_capacity(gps_offset + ifd0_len);
    output.extend_from_slice(if little { b"II*\0" } else { b"MM\0*" });
    output.extend_from_slice(&if little {
        8_u32.to_le_bytes()
    } else {
        8_u32.to_be_bytes()
    });
    write_directory(&mut output, &ifd0, little);
    if has_exif {
        write_directory(&mut output, &directories.exif, little);
    }
    if has_gps {
        write_directory(&mut output, &directories.gps, little);
    }
    output
}

fn directory_len(fields: &[ExifField], entry_count: usize, little: bool) -> usize {
    let mut size = 2 + entry_count * 12 + 4;
    for field in fields {
        let bytes = field.value.to_bytes(little);
        if bytes.len() > 4 {
            size += bytes.len() + (bytes.len() & 1);
        }
    }
    size
}

/// Writes one directory at the end of `output`. Appending is the only mode:
/// the caller has already reserved the space by computing the offsets first.
fn write_directory(output: &mut Vec<u8>, fields: &[ExifField], little: bool) {
    let mut fields: Vec<&ExifField> = fields.iter().collect();
    fields.sort_by_key(|field| field.tag);
    fields.dedup_by_key(|field| field.tag);
    let base = output.len();
    let data_start = base + 2 + fields.len() * 12 + 4;

    let mut data = Vec::new();
    output.extend_from_slice(&u16_bytes(fields.len() as u16, little));
    for field in fields {
        let bytes = field.value.to_bytes(little);
        output.extend_from_slice(&u16_bytes(field.tag, little));
        output.extend_from_slice(&u16_bytes(field.value.kind(), little));
        output.extend_from_slice(&u32_bytes(field.value.count(), little));
        if bytes.len() <= 4 {
            let mut inline = [0_u8; 4];
            inline[..bytes.len()].copy_from_slice(&bytes);
            output.extend_from_slice(&inline);
        } else {
            let offset = data_start + data.len();
            output.extend_from_slice(&u32_bytes(offset as u32, little));
            data.extend_from_slice(&bytes);
            if data.len() % 2 == 1 {
                data.push(0);
            }
        }
    }
    output.extend_from_slice(&u32_bytes(0, little));
    output.extend_from_slice(&data);
}

fn u16_bytes(value: u16, little: bool) -> [u8; 2] {
    if little {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    }
}

fn u32_bytes(value: u32, little: bool) -> [u8; 4] {
    if little {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::convert_bytes_with_options;
    use crate::format::OutputFormat;
    use crate::options::ConversionOptions;
    use crate::test_support::fixture_path;
    use image::GenericImageView;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(fixture_path(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
    }

    fn convert(name: &str, input: &[u8], target: OutputFormat) -> Vec<u8> {
        convert_with(name, input, target, ConversionOptions::default())
    }

    fn convert_with(
        name: &str,
        input: &[u8],
        target: OutputFormat,
        options: ConversionOptions,
    ) -> Vec<u8> {
        convert_bytes_with_options(name, input, target, options)
            .expect("the conversion succeeds")
            .remove(0)
            .bytes
    }

    fn field(tag: u16, value: ExifValue) -> ExifField {
        ExifField { tag, value }
    }

    fn exif_field(directories: &ExifDirectories, tag: u16) -> Option<ExifValue> {
        directories
            .ifd0
            .iter()
            .chain(directories.exif.iter())
            .chain(directories.gps.iter())
            .find(|field| field.tag == tag)
            .map(|field| field.value.clone())
    }

    fn assert_carried(name: &str, source: &[u8], output: &[u8]) {
        let source_metadata = extract(source);
        let metadata = extract(output);
        let exif = metadata
            .exif
            .unwrap_or_else(|| panic!("{name} lost its EXIF block"));
        let directories =
            parse_exif(&exif).unwrap_or_else(|| panic!("{name} wrote an unreadable EXIF block"));
        assert_eq!(
            exif_field(&directories, 0x010F).and_then(|value| value.as_str().map(str::to_owned)),
            Some("Convert2it".to_owned()),
            "{name} lost the Make tag"
        );
        assert_eq!(
            exif_field(&directories, 0x0110).and_then(|value| value.as_str().map(str::to_owned)),
            Some("Metadata Test".to_owned()),
            "{name} lost the Model tag"
        );
        assert_eq!(
            exif_field(&directories, 0x9003).and_then(|value| value.as_str().map(str::to_owned)),
            Some("2026:11:24 12:00:00".to_owned()),
            "{name} lost the DateTimeOriginal tag"
        );
        assert_eq!(
            metadata.icc, source_metadata.icc,
            "{name} did not carry the ICC profile byte for byte"
        );
        assert!(!directories.gps.is_empty(), "{name} lost the GPS IFD");
    }

    #[test]
    fn carries_exif_and_icc_from_jpeg_to_every_capable_target() {
        let source = fixture("metadata-photo.jpg");
        let source_directories = parse_exif(&extract(&source).exif.unwrap()).unwrap();
        assert_eq!(
            exif_field(&source_directories, TAG_ORIENTATION),
            Some(ExifValue::Short(vec![6])),
            "the fixture must carry orientation 6"
        );

        for (target, name) in [
            (OutputFormat::Jpeg, "out.jpg"),
            (OutputFormat::Png, "out.png"),
            (OutputFormat::Webp, "out.webp"),
            (OutputFormat::Tiff, "out.tiff"),
        ] {
            let output = convert("metadata-photo.jpg", &source, target);
            let metadata = extract(&output);
            assert!(metadata.exif.is_some(), "{name} lost its EXIF block");
            assert_eq!(
                metadata.icc,
                extract(&source).icc,
                "{name} did not carry the ICC profile"
            );
            let directories = parse_exif(&metadata.exif.unwrap()).unwrap();
            assert_eq!(
                exif_field(&directories, 0x010F)
                    .and_then(|value| value.as_str().map(str::to_owned)),
                Some("Convert2it".to_owned()),
                "{name} lost the Make tag"
            );
            assert_eq!(
                exif_field(&directories, TAG_ORIENTATION),
                Some(ExifValue::Short(vec![1])),
                "{name} left a stale orientation tag on rotated pixels"
            );
        }
    }

    #[test]
    fn carries_exif_and_icc_between_png_jpeg_and_webp() {
        let png = fixture("metadata-photo.png");
        let jpeg = fixture("metadata-photo.jpg");
        for (source_name, source) in [("metadata-photo.png", &png), ("metadata-photo.jpg", &jpeg)] {
            for (target, target_name) in [
                (OutputFormat::Jpeg, "jpg"),
                (OutputFormat::Png, "png"),
                (OutputFormat::Webp, "webp"),
            ] {
                let output = convert(source_name, source, target);
                assert_carried(&format!("{source_name} -> {target_name}"), source, &output);
            }
        }
        // A WebP output is itself a metadata source: re-converting it must not
        // lose what the first step wrote.
        let webp = convert("metadata-photo.jpg", &jpeg, OutputFormat::Webp);
        let round_tripped = convert("out.webp", &webp, OutputFormat::Jpeg);
        assert_carried("JPEG -> WebP -> JPEG", &jpeg, &round_tripped);
    }

    #[test]
    fn tiff_input_rebuilds_its_exif_for_other_targets() {
        let source = fixture("metadata-photo.jpg");
        let tiff = convert("metadata-photo.jpg", &source, OutputFormat::Tiff);
        let jpeg = convert("metadata.tiff", &tiff, OutputFormat::Jpeg);
        let metadata = extract(&jpeg);
        let directories = parse_exif(&metadata.exif.unwrap()).unwrap();
        assert_eq!(
            exif_field(&directories, 0x010F).and_then(|value| value.as_str().map(str::to_owned)),
            Some("Convert2it".to_owned())
        );
        assert_eq!(
            exif_field(&directories, 0x010E),
            None,
            "the fixture has no ImageDescription to carry"
        );
        assert_eq!(
            exif_field(&directories, 0x829A),
            Some(ExifValue::Rational(vec![(1, 125)])),
            "the Exif IFD did not survive TIFF input"
        );
        assert_eq!(
            metadata.icc,
            extract(&source).icc,
            "TIFF input dropped the ICC profile"
        );
    }

    #[test]
    fn applies_orientation_once_and_resets_the_tag() {
        let source = fixture("metadata-photo.jpg");
        let rotated = convert("metadata-photo.jpg", &source, OutputFormat::Png);
        assert_eq!(
            image::load_from_memory(&rotated).unwrap().dimensions(),
            (24, 32),
            "orientation 6 must rotate the 32x24 source"
        );
        assert_eq!(crate::images::exif_orientation(&rotated), Some(1));

        let kept = convert_with(
            "metadata-photo.jpg",
            &source,
            OutputFormat::Png,
            ConversionOptions {
                image_apply_exif_orientation: false,
                ..Default::default()
            },
        );
        assert_eq!(
            image::load_from_memory(&kept).unwrap().dimensions(),
            (32, 24),
            "the option must leave the pixels alone"
        );
        assert_eq!(crate::images::exif_orientation(&kept), Some(6));
    }

    #[test]
    fn keeps_a_webp_orientation_tag_it_did_not_apply() {
        // WebP orientation is not applied by `load_image`, so the tag must be
        // carried unchanged rather than reset to 1 on unrotated pixels.
        let source = fixture("metadata-photo.jpg");
        let webp = convert_with(
            "metadata-photo.jpg",
            &source,
            OutputFormat::Webp,
            ConversionOptions {
                image_apply_exif_orientation: false,
                ..Default::default()
            },
        );
        assert_eq!(
            image::load_from_memory(&webp).unwrap().dimensions(),
            (32, 24)
        );
        let jpeg = convert("source.webp", &webp, OutputFormat::Jpeg);
        assert_eq!(
            crate::images::exif_orientation(&jpeg),
            Some(6),
            "the tag describes the unrotated pixels and must survive"
        );
        assert_eq!(
            image::load_from_memory(&jpeg).unwrap().dimensions(),
            (32, 24)
        );
    }

    #[test]
    fn heic_orientation_is_applied_once_and_the_tag_reset() {
        let source = fixture("sample-heic-alpha-orientation.heic");
        let jpeg = convert(
            "sample-heic-alpha-orientation.heic",
            &source,
            OutputFormat::Jpeg,
        );
        assert_eq!(
            image::load_from_memory(&jpeg).unwrap().dimensions(),
            (32, 64)
        );
        assert_eq!(crate::images::exif_orientation(&jpeg), Some(1));
        assert!(
            extract(&jpeg).exif.is_some(),
            "the HEIC's Exif item must reach the JPEG"
        );
    }

    #[test]
    fn drops_an_exif_block_a_jpeg_cannot_store_instead_of_failing() {
        use image::ImageEncoder;
        let png = fixture("metadata-photo.png");
        let mut directories = parse_exif(&extract(&png).exif.unwrap()).unwrap();
        directories.ifd0.push(ExifField {
            tag: 0xEA1C,
            value: ExifValue::Bytes(vec![0x5A; 70_000]),
        });
        let oversized = build_exif(&directories);
        let image = image::load_from_memory(&png).unwrap();
        let mut cursor = std::io::Cursor::new(Vec::new());
        let mut encoder = image::codecs::png::PngEncoder::new(&mut cursor);
        encoder.set_exif_metadata(oversized).unwrap();
        image.write_with_encoder(encoder).unwrap();

        let jpeg = convert("oversized.png", &cursor.into_inner(), OutputFormat::Jpeg);
        assert!(
            extract(&jpeg).exif.is_none(),
            "an APP1 segment holds at most 65533 bytes, so the block is dropped"
        );
        assert!(image::load_from_memory(&jpeg).is_ok());
    }

    #[test]
    fn tiff_output_icc_is_readable_by_the_tiff_reader() {
        // The image crate's TIFF decoder loses the tag once its limits are
        // applied, which `ImageReader` always does, so read through the `tiff`
        // crate that decoder is built on. This is also the path external tools
        // use to read tag 34675.
        let source = fixture("metadata-photo.jpg");
        let tiff = convert("metadata-photo.jpg", &source, OutputFormat::Tiff);
        let mut decoder = tiff::decoder::Decoder::new(std::io::Cursor::new(&tiff)).unwrap();
        let icc = decoder
            .get_tag_u8_vec(tiff::tags::Tag::IccProfile)
            .expect("the ICC tag must be readable");
        assert_eq!(icc, extract(&source).icc.unwrap());
    }

    #[test]
    fn targets_without_a_metadata_container_drop_the_blocks() {
        let source = fixture("metadata-photo.jpg");
        for target in [OutputFormat::Bmp, OutputFormat::Gif, OutputFormat::Tga] {
            let output = convert("metadata-photo.jpg", &source, target);
            assert!(
                extract(&output).is_empty(),
                "{} cannot carry EXIF or ICC",
                target.label()
            );
        }
    }

    #[test]
    fn reads_and_rebuilds_a_little_endian_exif_stream() {
        let original = ExifDirectories {
            ifd0: vec![
                field(0x010F, ExifValue::Ascii(b"Convert2it\0".to_vec())),
                field(TAG_ORIENTATION, ExifValue::Short(vec![6])),
                field(0x011A, ExifValue::Rational(vec![(72, 1)])),
            ],
            exif: vec![
                field(0x9003, ExifValue::Ascii(b"2026:11:24 10:00:00\0".to_vec())),
                field(0x829A, ExifValue::Rational(vec![(1, 250)])),
            ],
            gps: vec![field(0x0000, ExifValue::Bytes(vec![2, 3, 0, 0]))],
            little_endian: true,
        };
        let bytes = build_exif(&original);
        assert_eq!(crate::images::exif_orientation(&bytes), Some(6));
        let parsed = parse_exif(&bytes).unwrap();
        assert!(parsed.little_endian);
        assert_eq!(
            parsed
                .ifd0
                .iter()
                .find(|entry| entry.tag == 0x010F)
                .and_then(|entry| entry.value.as_str()),
            Some("Convert2it")
        );
        assert_eq!(
            parsed
                .exif
                .iter()
                .find(|entry| entry.tag == 0x829A)
                .map(|entry| entry.value.clone()),
            Some(ExifValue::Rational(vec![(1, 250)]))
        );
        assert!(!parsed.gps.is_empty());
    }

    #[test]
    fn rebuilds_a_big_endian_stream_without_swapping_its_values() {
        let tiff = [
            b"MM\0*".as_slice(),
            &8_u32.to_be_bytes(),
            &1_u16.to_be_bytes(),
            &0x0112_u16.to_be_bytes(),
            &3_u16.to_be_bytes(),
            &1_u32.to_be_bytes(),
            &6_u16.to_be_bytes(),
            &[0, 0],
            &0_u32.to_be_bytes(),
        ]
        .concat();
        let parsed = parse_exif(&tiff).unwrap();
        assert!(!parsed.little_endian);
        let rebuilt = build_exif(&parsed);
        assert!(rebuilt.starts_with(b"MM\0*"));
        assert_eq!(crate::images::exif_orientation(&rebuilt), Some(6));
    }

    #[test]
    fn round_trips_double_values_in_both_byte_orders() {
        for little_endian in [true, false] {
            let directories = ExifDirectories {
                ifd0: vec![field(0x010E, ExifValue::Ascii(b"doubles ".to_vec()))],
                exif: vec![field(0xEA1D, ExifValue::Double(vec![1.5, -2.25]))],
                gps: Vec::new(),
                little_endian,
            };
            let bytes = build_exif(&directories);
            let parsed = parse_exif(&bytes).unwrap();
            assert_eq!(
                parsed
                    .exif
                    .iter()
                    .find(|entry| entry.tag == 0xEA1D)
                    .map(|entry| entry.value.clone()),
                Some(ExifValue::Double(vec![1.5, -2.25])),
                "little endian: {little_endian}"
            );
        }
    }

    #[test]
    fn skips_maker_notes_and_pixel_dimensions_when_rebuilding() {
        let directories = ExifDirectories {
            ifd0: vec![field(TAG_ORIENTATION, ExifValue::Short(vec![1]))],
            exif: vec![
                field(TAG_MAKER_NOTE, ExifValue::Bytes(vec![1, 2, 3])),
                field(TAG_PIXEL_X_DIMENSION, ExifValue::Long(vec![640])),
                field(0x829D, ExifValue::Rational(vec![(28, 10)])),
            ],
            gps: Vec::new(),
            little_endian: true,
        };
        let bytes = build_exif(&directories);
        let parsed = parse_exif(&bytes).unwrap();
        assert!(parsed.exif.iter().all(|entry| entry.tag == 0x829D));
    }
}
