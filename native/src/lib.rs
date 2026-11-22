// SPDX-License-Identifier: GPL-2.0-or-later

use std::{
    collections::{BTreeSet, HashSet},
    io::{Cursor, Write},
    path::{Path, PathBuf},
};

use image::ImageEncoder;
use pdf_writer::{Content, Filter, Name, Pdf, Rect, Ref, Str};
use thiserror::Error;

mod input_extensions;
pub use input_extensions::supported_input_extensions;

const MAX_AUDIO_SAMPLES: usize = 120_000_000;
const MAX_SVG_DIMENSION: u32 = 8_192;
const MAX_SVG_PIXELS: u64 = 32_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFormat {
    Png,
    Jpeg,
    Webp,
    Gif,
    Bmp,
    Tiff,
    Ico,
    Ppm,
    Pbm,
    Pgm,
    Pam,
    Tga,
    Qoi,
    Hdr,
    Farbfeld,
    Pdf,
    Text,
    TextUtf8NoBom,
    TextUtf8Bom,
    TextUtf16Le,
    TextUtf16Be,
    TextUtf32Le,
    TextUtf32Be,
    Csv,
    Json,
    Yaml,
    Toml,
    Wav,
    Flac,
    Aiff,
    Au,
    Opus,
    Zip,
    Tar,
    TarGz,
    Gzip,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioChannels {
    Mono,
    Stereo,
}

impl AudioChannels {
    pub fn count(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mono => "Mono",
            Self::Stereo => "Stereo",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PdfPageSize {
    A3,
    A4,
    A5,
    Letter,
    Legal,
}

impl PdfPageSize {
    pub const ALL: [Self; 5] = [Self::A3, Self::A4, Self::A5, Self::Letter, Self::Legal];

    pub fn label(self) -> &'static str {
        match self {
            Self::A3 => "A3",
            Self::A4 => "A4",
            Self::A5 => "A5",
            Self::Letter => "LETTER",
            Self::Legal => "LEGAL",
        }
    }

    /// Page dimensions in points, matching PDFKit's page sizes.
    fn dimensions(self) -> (f32, f32) {
        match self {
            Self::A3 => (841.89, 1190.55),
            Self::A4 => (595.28, 841.89),
            Self::A5 => (419.53, 595.28),
            Self::Letter => (612.0, 792.0),
            Self::Legal => (612.0, 1008.0),
        }
    }

    fn dimensions_for(self, orientation: PdfOrientation) -> (f32, f32) {
        let (width, height) = self.dimensions();
        match orientation {
            PdfOrientation::Portrait => (width, height),
            PdfOrientation::Landscape => (height, width),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PdfOrientation {
    Portrait,
    Landscape,
}

impl PdfOrientation {
    pub const ALL: [Self; 2] = [Self::Portrait, Self::Landscape];

    pub fn label(self) -> &'static str {
        match self {
            Self::Portrait => "Portrait",
            Self::Landscape => "Landscape",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PdfImageFit {
    Contain,
    Cover,
}

impl PdfImageFit {
    pub const ALL: [Self; 2] = [Self::Contain, Self::Cover];

    pub fn label(self) -> &'static str {
        match self {
            Self::Contain => "Contain",
            Self::Cover => "Cover",
        }
    }
}

/// User-selected encoding options. `None` keeps the encoder's default.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConversionOptions {
    pub jpeg_quality: Option<u8>,
    pub png_compression: Option<u8>,
    pub archive_compression: Option<u8>,
    pub audio_sample_rate: Option<u32>,
    pub audio_channels: Option<AudioChannels>,
    pub pdf_page_size: PdfPageSize,
    pub pdf_orientation: PdfOrientation,
    pub pdf_image_fit: PdfImageFit,
    pub pdf_font_size: u32,
    pub pdf_margin: u32,
}

impl Default for ConversionOptions {
    fn default() -> Self {
        Self {
            jpeg_quality: None,
            png_compression: None,
            archive_compression: None,
            audio_sample_rate: None,
            audio_channels: None,
            pdf_page_size: Self::DEFAULT_PDF_PAGE_SIZE,
            pdf_orientation: Self::DEFAULT_PDF_ORIENTATION,
            pdf_image_fit: Self::DEFAULT_PDF_IMAGE_FIT,
            pdf_font_size: Self::DEFAULT_PDF_FONT_SIZE,
            pdf_margin: Self::DEFAULT_PDF_MARGIN,
        }
    }
}

impl ConversionOptions {
    pub const DEFAULT_JPEG_QUALITY: u8 = 75;
    pub const DEFAULT_COMPRESSION_LEVEL: u8 = 6;
    pub const DEFAULT_AUDIO_SAMPLE_RATE: u32 = 44_100;
    pub const MIN_AUDIO_SAMPLE_RATE: u32 = 1_000;
    /// FLAC's encoder accepts at most 96 kHz, so the shared audio control stays within that range.
    pub const MAX_AUDIO_SAMPLE_RATE: u32 = 96_000;
    pub const DEFAULT_PDF_PAGE_SIZE: PdfPageSize = PdfPageSize::A4;
    pub const DEFAULT_PDF_ORIENTATION: PdfOrientation = PdfOrientation::Portrait;
    pub const DEFAULT_PDF_IMAGE_FIT: PdfImageFit = PdfImageFit::Contain;
    pub const DEFAULT_PDF_FONT_SIZE: u32 = 11;
    pub const MIN_PDF_FONT_SIZE: u32 = 1;
    pub const MAX_PDF_FONT_SIZE: u32 = 200;
    pub const DEFAULT_PDF_MARGIN: u32 = 72;
    pub const MIN_PDF_MARGIN: u32 = 0;
    pub const MAX_PDF_MARGIN: u32 = 144;
}

impl OutputFormat {
    pub const ALL: [Self; 36] = [
        Self::Png,
        Self::Jpeg,
        Self::Webp,
        Self::Gif,
        Self::Bmp,
        Self::Tiff,
        Self::Ico,
        Self::Ppm,
        Self::Pbm,
        Self::Pgm,
        Self::Pam,
        Self::Tga,
        Self::Qoi,
        Self::Hdr,
        Self::Farbfeld,
        Self::Pdf,
        Self::Text,
        Self::TextUtf8NoBom,
        Self::TextUtf8Bom,
        Self::TextUtf16Le,
        Self::TextUtf16Be,
        Self::TextUtf32Le,
        Self::TextUtf32Be,
        Self::Csv,
        Self::Json,
        Self::Yaml,
        Self::Toml,
        Self::Wav,
        Self::Flac,
        Self::Aiff,
        Self::Au,
        Self::Opus,
        Self::Zip,
        Self::Tar,
        Self::TarGz,
        Self::Gzip,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG image",
            Self::Jpeg => "JPEG image",
            Self::Webp => "WebP image",
            Self::Gif => "GIF image",
            Self::Bmp => "BMP image",
            Self::Tiff => "TIFF image",
            Self::Ico => "ICO icon",
            Self::Ppm => "PPM image",
            Self::Pbm => "PBM image",
            Self::Pgm => "PGM image",
            Self::Pam => "PAM image",
            Self::Tga => "TGA image",
            Self::Qoi => "QOI image",
            Self::Hdr => "Radiance HDR image",
            Self::Farbfeld => "Farbfeld image",
            Self::Pdf => "PDF document",
            Self::Text => "Plain text",
            Self::TextUtf8NoBom => "UTF-8 without BOM",
            Self::TextUtf8Bom => "UTF-8 with BOM",
            Self::TextUtf16Le => "UTF-16 LE",
            Self::TextUtf16Be => "UTF-16 BE",
            Self::TextUtf32Le => "UTF-32 LE",
            Self::TextUtf32Be => "UTF-32 BE",
            Self::Csv => "CSV",
            Self::Json => "JSON",
            Self::Yaml => "YAML",
            Self::Toml => "TOML",
            Self::Wav => "WAV audio",
            Self::Flac => "FLAC audio",
            Self::Aiff => "AIFF audio",
            Self::Au => "AU audio",
            Self::Opus => "Opus audio",
            Self::Zip => "ZIP archive",
            Self::Tar => "TAR archive",
            Self::TarGz => "TAR.GZ archive",
            Self::Gzip => "GZIP compressed file",
        }
    }

    fn image_format(self) -> Option<image::ImageFormat> {
        match self {
            Self::Png => Some(image::ImageFormat::Png),
            Self::Jpeg => Some(image::ImageFormat::Jpeg),
            Self::Webp => Some(image::ImageFormat::WebP),
            Self::Gif => Some(image::ImageFormat::Gif),
            Self::Bmp => Some(image::ImageFormat::Bmp),
            Self::Tiff => Some(image::ImageFormat::Tiff),
            Self::Ico => Some(image::ImageFormat::Ico),
            Self::Ppm | Self::Pbm | Self::Pgm | Self::Pam => Some(image::ImageFormat::Pnm),
            Self::Tga => Some(image::ImageFormat::Tga),
            Self::Qoi => Some(image::ImageFormat::Qoi),
            Self::Hdr => Some(image::ImageFormat::Hdr),
            Self::Farbfeld => Some(image::ImageFormat::Farbfeld),
            _ => None,
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Gif => "gif",
            Self::Bmp => "bmp",
            Self::Tiff => "tiff",
            Self::Ico => "ico",
            Self::Ppm => "ppm",
            Self::Pbm => "pbm",
            Self::Pgm => "pgm",
            Self::Pam => "pam",
            Self::Tga => "tga",
            Self::Qoi => "qoi",
            Self::Hdr => "hdr",
            Self::Farbfeld => "ff",
            Self::Pdf => "pdf",
            Self::Text => "txt",
            Self::TextUtf8NoBom
            | Self::TextUtf8Bom
            | Self::TextUtf16Le
            | Self::TextUtf16Be
            | Self::TextUtf32Le
            | Self::TextUtf32Be => "txt",
            Self::Csv => "csv",
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Aiff => "aiff",
            Self::Au => "au",
            Self::Opus => "opus",
            Self::Zip => "zip",
            Self::Tar => "tar",
            Self::TarGz => "tar.gz",
            Self::Gzip => "gz",
        }
    }

    /// Whether files with this name can be used as input for the format.
    pub fn supports_input(self, file_name: &str) -> bool {
        let extension = file_extension(file_name);
        match self {
            Self::Png
            | Self::Jpeg
            | Self::Webp
            | Self::Gif
            | Self::Bmp
            | Self::Tiff
            | Self::Ico
            | Self::Ppm
            | Self::Pbm
            | Self::Pgm
            | Self::Pam
            | Self::Tga
            | Self::Qoi
            | Self::Hdr
            | Self::Farbfeld => is_image_input_extension(&extension),
            Self::Pdf => extension == "txt" || is_image_input_extension(&extension),
            Self::Text | Self::Csv | Self::Json | Self::Yaml | Self::Toml => {
                is_data_extension(&extension)
            }
            Self::TextUtf8NoBom
            | Self::TextUtf8Bom
            | Self::TextUtf16Le
            | Self::TextUtf16Be
            | Self::TextUtf32Le
            | Self::TextUtf32Be => extension == "txt",
            Self::Wav | Self::Flac | Self::Aiff | Self::Au | Self::Opus => {
                is_audio_extension(&extension)
            }
            Self::Zip | Self::Tar | Self::TarGz | Self::Gzip => true,
        }
    }
}

#[derive(Debug)]
pub struct ConvertedFile {
    pub name: PathBuf,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum ConvertError {
    #[error("cannot convert .{input_extension} to {target}")]
    Unsupported {
        input_extension: String,
        target: &'static str,
    },
    #[error("the input is not valid UTF-8 text")]
    InvalidUtf8(#[from] std::string::FromUtf8Error),
    #[error("UTF-32 input contains an invalid Unicode code point: {0:#x}")]
    InvalidUnicodeCodePoint(u32),
    #[error("JSON-to-CSV requires an array of objects with at least one column")]
    JsonNotTabular,
    #[error("CSV contains duplicate column names")]
    DuplicateCsvHeader,
    #[error("multiple input files are named {0:?}")]
    DuplicateArchiveEntryName(String),
    #[error("no input files were provided")]
    NoInputFiles,
    #[error("could not render the SVG: {0}")]
    Svg(String),
    #[error("the SVG is too large to render")]
    SvgTooLarge,
    #[error("the audio file has no decodable audio track")]
    NoAudioTrack,
    #[error("the audio file is too large to convert in memory")]
    AudioTooLarge,
    #[error("could not decode audio: {0}")]
    Audio(String),
    #[error("TOML output requires an object at the top level")]
    TomlNotTabular,
    #[error(transparent)]
    Yaml(#[from] serde_yaml_ng::Error),
    #[error(transparent)]
    TomlDeserialize(#[from] toml::de::Error),
    #[error(transparent)]
    TomlSerialize(#[from] toml::ser::Error),
    #[error(transparent)]
    Image(#[from] image::ImageError),
    #[error(transparent)]
    Csv(#[from] csv::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn convert_bytes(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    convert_bytes_with_options(input_name, input, target, ConversionOptions::default())
}

pub fn convert_bytes_with_options(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    options: ConversionOptions,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    let extension = file_extension(input_name);
    if target == OutputFormat::Zip {
        let name = safe_file_name(input_name);
        let entries = [(PathBuf::from(name), input.to_vec())];
        let bytes = create_zip(&entries, options.archive_compression)?;
        return Ok(vec![ConvertedFile {
            name: output_name(input_name, target.extension()),
            bytes,
        }]);
    }
    if target == OutputFormat::Tar || target == OutputFormat::TarGz {
        let name = safe_file_name(input_name);
        let entries = [(PathBuf::from(name), input.to_vec())];
        let gzip = target == OutputFormat::TarGz;
        let bytes = create_tar(&entries, gzip, options.archive_compression)?;
        return Ok(vec![ConvertedFile {
            name: output_name(input_name, target.extension()),
            bytes,
        }]);
    }
    if target == OutputFormat::Gzip {
        return Ok(vec![ConvertedFile {
            name: PathBuf::from(format!("{}.gz", safe_file_name(input_name))),
            bytes: gzip_bytes(input, options.archive_compression)?,
        }]);
    }
    if target == OutputFormat::Pdf {
        let bytes = if extension == "txt" {
            text_to_pdf(input, options)?
        } else if is_image_input_extension(&extension) {
            image_to_pdf(input, &extension, options)?
        } else {
            return Err(unsupported(&extension, target));
        };
        return Ok(vec![ConvertedFile {
            name: output_name(input_name, target.extension()),
            bytes,
        }]);
    }

    if target.image_format().is_some() {
        if !is_image_input_extension(&extension) {
            return Err(unsupported(&extension, target));
        }
        let image = if extension == "svg" {
            rasterize_svg(input)?
        } else {
            image::load_from_memory(input)?
        };
        let bytes = encode_image(image, target, options)?;
        return Ok(vec![ConvertedFile {
            name: output_name(input_name, target.extension()),
            bytes,
        }]);
    }

    if matches!(
        target,
        OutputFormat::Wav
            | OutputFormat::Flac
            | OutputFormat::Aiff
            | OutputFormat::Au
            | OutputFormat::Opus
    ) {
        if !is_audio_extension(&extension) {
            return Err(unsupported(&extension, target));
        }
        // Opus only accepts 8/12/16/24/48 kHz. Map the requested rate before
        // the shared audio options resample so it goes straight to the rate the
        // encoder uses instead of resampling twice.
        let audio_options = if target == OutputFormat::Opus {
            ConversionOptions {
                audio_sample_rate: options.audio_sample_rate.map(opus_sample_rate),
                ..options
            }
        } else {
            options
        };
        let audio = apply_audio_options(decode_audio(input, &extension)?, audio_options)?;
        let bytes = match target {
            OutputFormat::Wav => encode_wav(&audio)?,
            OutputFormat::Flac => encode_flac(&audio)?,
            OutputFormat::Aiff => encode_aiff(&audio)?,
            OutputFormat::Au => encode_au(&audio)?,
            OutputFormat::Opus => encode_opus(audio)?,
            _ => unreachable!("only audio targets reach this branch"),
        };
        return Ok(vec![ConvertedFile {
            name: output_name(input_name, target.extension()),
            bytes,
        }]);
    }

    convert_text(input_name, input, target)
}

pub fn convert_batch(
    inputs: &[(PathBuf, Vec<u8>)],
    target: OutputFormat,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    convert_batch_with_options(inputs, target, ConversionOptions::default())
}

pub fn convert_batch_with_options(
    inputs: &[(PathBuf, Vec<u8>)],
    target: OutputFormat,
    options: ConversionOptions,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    if inputs.is_empty() {
        return Err(ConvertError::NoInputFiles);
    }

    if matches!(
        target,
        OutputFormat::Zip | OutputFormat::Tar | OutputFormat::TarGz
    ) {
        let mut seen_names = HashSet::new();
        let mut entries = Vec::with_capacity(inputs.len());

        for (path, bytes) in inputs {
            let name = safe_file_name(path.to_string_lossy().as_ref());
            if !seen_names.insert(name.clone()) {
                return Err(ConvertError::DuplicateArchiveEntryName(name));
            }
            entries.push((PathBuf::from(name), bytes.clone()));
        }

        let (name, bytes) = match target {
            OutputFormat::Zip => (
                "converted.zip",
                create_zip(&entries, options.archive_compression)?,
            ),
            OutputFormat::Tar => (
                "converted.tar",
                create_tar(&entries, false, options.archive_compression)?,
            ),
            OutputFormat::TarGz => (
                "converted.tar.gz",
                create_tar(&entries, true, options.archive_compression)?,
            ),
            _ => unreachable!("only archive targets reach this branch"),
        };
        return Ok(vec![ConvertedFile {
            name: PathBuf::from(name),
            bytes,
        }]);
    }

    if target == OutputFormat::Gzip {
        let mut converted = Vec::with_capacity(inputs.len());
        for (path, bytes) in inputs {
            converted.push(ConvertedFile {
                name: PathBuf::from(format!(
                    "{}.gz",
                    safe_file_name(path.to_string_lossy().as_ref())
                )),
                bytes: gzip_bytes(bytes, options.archive_compression)?,
            });
        }
        return Ok(converted);
    }

    let mut converted = Vec::new();
    for (path, bytes) in inputs {
        converted.extend(convert_bytes_with_options(
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .as_ref(),
            bytes,
            target,
            options,
        )?);
    }
    Ok(converted)
}

fn convert_text(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    let extension = file_extension(input_name);
    if !is_data_extension(&extension) {
        return Err(unsupported(&extension, target));
    }

    if matches!(
        target,
        OutputFormat::TextUtf8NoBom
            | OutputFormat::TextUtf8Bom
            | OutputFormat::TextUtf16Le
            | OutputFormat::TextUtf16Be
            | OutputFormat::TextUtf32Le
            | OutputFormat::TextUtf32Be
    ) {
        if extension != "txt" {
            return Err(unsupported(&extension, target));
        }
        let text = decode_text(input)?;
        return Ok(vec![ConvertedFile {
            name: output_name(input_name, target.extension()),
            bytes: encode_text(&text, target)?,
        }]);
    }

    let text = if extension == "txt" {
        decode_text(input)?
    } else {
        String::from_utf8(input.to_vec())?
    };
    let output = match target {
        OutputFormat::Text => match extension.as_str() {
            "json" => {
                let value: serde_json::Value = serde_json::from_str(&text)?;
                match value {
                    serde_json::Value::String(value) => value.into_bytes(),
                    value => serde_json::to_vec_pretty(&value)?,
                }
            }
            "yaml" | "yml" | "toml" | "txt" => text.into_bytes(),
            _ => input.to_vec(),
        },
        OutputFormat::Json => {
            let value = parse_data_to_json(&extension, &text)?;
            serde_json::to_vec_pretty(&value)?
        }
        OutputFormat::Csv => match extension.as_str() {
            "csv" => input.to_vec(),
            "txt" => text_to_csv(&text)?,
            _ => {
                let value = parse_data_to_json(&extension, &text)?;
                json_to_csv(&value)?
            }
        },
        OutputFormat::Yaml => {
            let value = parse_data_to_json(&extension, &text)?;
            serde_yaml_ng::to_string(&value)?.into_bytes()
        }
        OutputFormat::Toml => {
            let value = parse_data_to_json(&extension, &text)?;
            json_to_toml(&value)?.into_bytes()
        }
        _ => return Err(unsupported(&extension, target)),
    };

    let output_extension = target.extension();
    Ok(vec![ConvertedFile {
        name: output_name(input_name, output_extension),
        bytes: output,
    }])
}

fn decode_text(input: &[u8]) -> Result<String, ConvertError> {
    if let Some(bytes) = input.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        return Ok(String::from_utf8_lossy(bytes).into_owned());
    }
    if let Some(bytes) = input.strip_prefix(&[0xff, 0xfe, 0x00, 0x00]) {
        return decode_utf32(bytes, true);
    }
    if let Some(bytes) = input.strip_prefix(&[0x00, 0x00, 0xfe, 0xff]) {
        return decode_utf32(bytes, false);
    }
    if let Some(bytes) = input.strip_prefix(&[0xff, 0xfe]) {
        return Ok(decode_utf16(bytes, true));
    }
    if let Some(bytes) = input.strip_prefix(&[0xfe, 0xff]) {
        return Ok(decode_utf16(bytes, false));
    }
    Ok(String::from_utf8_lossy(input).into_owned())
}

fn decode_utf16(input: &[u8], little_endian: bool) -> String {
    let words = input
        .chunks_exact(2)
        .map(|pair| {
            let bytes = [pair[0], pair[1]];
            if little_endian {
                u16::from_le_bytes(bytes)
            } else {
                u16::from_be_bytes(bytes)
            }
        })
        .collect::<Vec<_>>();
    String::from_utf16_lossy(&words)
}

fn decode_utf32(input: &[u8], little_endian: bool) -> Result<String, ConvertError> {
    input
        .chunks_exact(4)
        .map(|chunk| {
            let bytes = [chunk[0], chunk[1], chunk[2], chunk[3]];
            let code_point = if little_endian {
                u32::from_le_bytes(bytes)
            } else {
                u32::from_be_bytes(bytes)
            };
            char::from_u32(code_point).ok_or(ConvertError::InvalidUnicodeCodePoint(code_point))
        })
        .collect()
}

fn encode_text(text: &str, target: OutputFormat) -> Result<Vec<u8>, ConvertError> {
    match target {
        OutputFormat::TextUtf8Bom => {
            let mut bytes = vec![0xef, 0xbb, 0xbf];
            bytes.extend_from_slice(text.as_bytes());
            Ok(bytes)
        }
        OutputFormat::TextUtf16Le | OutputFormat::TextUtf16Be => {
            let little_endian = target == OutputFormat::TextUtf16Le;
            let mut bytes = if little_endian {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            };
            for word in text.encode_utf16() {
                let encoded = if little_endian {
                    word.to_le_bytes()
                } else {
                    word.to_be_bytes()
                };
                bytes.extend_from_slice(&encoded);
            }
            Ok(bytes)
        }
        OutputFormat::TextUtf32Le | OutputFormat::TextUtf32Be => {
            let little_endian = target == OutputFormat::TextUtf32Le;
            let mut bytes = if little_endian {
                vec![0xff, 0xfe, 0x00, 0x00]
            } else {
                vec![0x00, 0x00, 0xfe, 0xff]
            };
            for code_point in text.chars().map(u32::from) {
                let encoded = if little_endian {
                    code_point.to_le_bytes()
                } else {
                    code_point.to_be_bytes()
                };
                bytes.extend_from_slice(&encoded);
            }
            Ok(bytes)
        }
        OutputFormat::Text | OutputFormat::TextUtf8NoBom => Ok(text.as_bytes().to_vec()),
        _ => Err(unsupported("txt", target)),
    }
}

fn csv_to_json(text: &str) -> Result<serde_json::Value, ConvertError> {
    let mut reader = csv::Reader::from_reader(text.as_bytes());
    let headers = reader.headers()?.clone();
    let mut seen_headers = HashSet::new();
    for header in &headers {
        if !seen_headers.insert(header) {
            return Err(ConvertError::DuplicateCsvHeader);
        }
    }

    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record?;
        let mut object = serde_json::Map::new();
        for (header, value) in headers.iter().zip(record.iter()) {
            object.insert(
                header.to_owned(),
                serde_json::Value::String(value.to_owned()),
            );
        }
        rows.push(serde_json::Value::Object(object));
    }
    Ok(serde_json::Value::Array(rows))
}

fn json_to_csv(value: &serde_json::Value) -> Result<Vec<u8>, ConvertError> {
    let rows = value.as_array().ok_or(ConvertError::JsonNotTabular)?;
    if rows.is_empty() {
        return Err(ConvertError::JsonNotTabular);
    }

    let mut headers = BTreeSet::new();
    for row in rows {
        let object = row.as_object().ok_or(ConvertError::JsonNotTabular)?;
        headers.extend(object.keys().cloned());
    }
    if headers.is_empty() {
        return Err(ConvertError::JsonNotTabular);
    }
    let headers: Vec<_> = headers.into_iter().collect();

    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(&headers)?;
    for row in rows {
        let object = row.as_object().ok_or(ConvertError::JsonNotTabular)?;
        let record: Vec<String> = headers
            .iter()
            .map(|header| match object.get(header) {
                None | Some(serde_json::Value::Null) => String::new(),
                Some(serde_json::Value::String(value)) => value.clone(),
                Some(value) => value.to_string(),
            })
            .collect();
        writer.write_record(record)?;
    }
    writer.flush()?;
    Ok(writer.into_inner().map_err(|error| error.into_error())?)
}

fn text_to_csv(text: &str) -> Result<Vec<u8>, ConvertError> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(["text"])?;
    for line in text.lines() {
        writer.write_record([line])?;
    }
    writer.flush()?;
    Ok(writer.into_inner().map_err(|error| error.into_error())?)
}

fn create_zip(
    entries: &[(PathBuf, Vec<u8>)],
    compression: Option<u8>,
) -> Result<Vec<u8>, ConvertError> {
    let cursor = Cursor::new(Vec::new());
    let mut archive = zip::ZipWriter::new(cursor);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(compression.map(|level| i64::from(level.clamp(1, 9))));

    for (name, bytes) in entries {
        let name = name.to_string_lossy().replace('\\', "/");
        archive.start_file(name, options)?;
        archive.write_all(bytes)?;
    }

    Ok(archive.finish()?.into_inner())
}

fn safe_file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty() && name != "." && name != "..")
        .unwrap_or_else(|| "file".to_owned())
}

fn file_extension(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

fn output_name(input_name: &str, extension: &str) -> PathBuf {
    let stem = Path::new(input_name)
        .file_stem()
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("converted"));
    PathBuf::from(format!("{}.{}", stem.to_string_lossy(), extension))
}

fn unsupported(extension: &str, target: OutputFormat) -> ConvertError {
    ConvertError::Unsupported {
        input_extension: extension.to_owned(),
        target: target.label(),
    }
}

fn is_image_input_extension(extension: &str) -> bool {
    input_extensions::IMAGE_INPUT_EXTENSIONS.contains(&extension)
}

fn is_data_extension(extension: &str) -> bool {
    input_extensions::DATA_INPUT_EXTENSIONS.contains(&extension)
}

fn is_audio_extension(extension: &str) -> bool {
    input_extensions::AUDIO_INPUT_EXTENSIONS.contains(&extension)
}

fn encode_image(
    image: image::DynamicImage,
    target: OutputFormat,
    options: ConversionOptions,
) -> Result<Vec<u8>, ConvertError> {
    let format = target
        .image_format()
        .expect("encode_image is only called for image targets");

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

fn encode_pnm_image(
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

fn fit_icon(image: image::DynamicImage) -> image::RgbaImage {
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

fn rasterize_svg(input: &[u8]) -> Result<image::DynamicImage, ConvertError> {
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

fn parse_data_to_json(extension: &str, text: &str) -> Result<serde_json::Value, ConvertError> {
    match extension {
        "json" => Ok(serde_json::from_str(text)?),
        "yaml" | "yml" => Ok(serde_yaml_ng::from_str(text)?),
        "toml" => {
            let value: toml::Value = toml::from_str(text)?;
            Ok(toml_to_json(value))
        }
        "csv" => csv_to_json(text),
        _ => Ok(serde_json::Value::String(text.to_owned())),
    }
}

fn toml_to_json(value: toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(value) => serde_json::Value::String(value),
        toml::Value::Integer(value) => serde_json::Value::Number(value.into()),
        toml::Value::Float(value) => serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        toml::Value::Boolean(value) => serde_json::Value::Bool(value),
        toml::Value::Datetime(value) => serde_json::Value::String(value.to_string()),
        toml::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(toml_to_json).collect())
        }
        toml::Value::Table(table) => serde_json::Value::Object(
            table
                .into_iter()
                .map(|(key, value)| (key, toml_to_json(value)))
                .collect(),
        ),
    }
}

fn json_to_toml(value: &serde_json::Value) -> Result<String, ConvertError> {
    let object = value.as_object().ok_or(ConvertError::TomlNotTabular)?;
    Ok(toml::to_string_pretty(&json_object_to_toml(object))?)
}

fn json_object_to_toml(object: &serde_json::Map<String, serde_json::Value>) -> toml::Table {
    object
        .iter()
        .filter_map(|(key, value)| json_to_toml_value(value).map(|value| (key.to_owned(), value)))
        .collect()
}

fn json_to_toml_value(value: &serde_json::Value) -> Option<toml::Value> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(value) => Some(toml::Value::Boolean(*value)),
        serde_json::Value::Number(value) => value
            .as_i64()
            .map(toml::Value::Integer)
            .or_else(|| value.as_f64().map(toml::Value::Float)),
        serde_json::Value::String(value) => Some(toml::Value::String(value.to_owned())),
        serde_json::Value::Array(values) => Some(toml::Value::Array(
            values.iter().filter_map(json_to_toml_value).collect(),
        )),
        serde_json::Value::Object(object) => Some(toml::Value::Table(json_object_to_toml(object))),
    }
}

fn is_end_of_stream(error: &symphonia::core::errors::Error) -> bool {
    match error {
        symphonia::core::errors::Error::IoError(error)
            if error.kind() == std::io::ErrorKind::UnexpectedEof =>
        {
            true
        }
        _ => error
            .to_string()
            .to_ascii_lowercase()
            .contains("end of stream"),
    }
}

struct DecodedAudio {
    samples: Vec<f32>,
    channels: usize,
    sample_rate: u32,
}

fn decode_audio(input: &[u8], extension: &str) -> Result<DecodedAudio, ConvertError> {
    if extension == "flac" || input.starts_with(b"fLaC") {
        return decode_flac(input);
    }
    if is_ogg_opus(input) {
        return decode_ogg_opus(input);
    }

    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_OPUS};
    use symphonia::core::errors::Error as SymphoniaError;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let source = MediaSourceStream::new(Box::new(Cursor::new(input.to_vec())), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(extension);
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|error| {
            ConvertError::Audio(format!("could not open the audio stream: {error}"))
        })?;
    let mut format = probed.format;

    let (track_id, codec_params) = {
        let track = format
            .tracks()
            .iter()
            .find(|track| {
                track.codec_params.channels.is_some() || track.codec_params.codec == CODEC_TYPE_OPUS
            })
            .or_else(|| {
                format
                    .tracks()
                    .iter()
                    .find(|track| track.codec_params.sample_rate.is_some())
            })
            .ok_or(ConvertError::NoAudioTrack)?;
        (track.id, track.codec_params.clone())
    };

    if codec_params.codec == CODEC_TYPE_OPUS {
        return decode_opus_track(format.as_mut(), track_id, &codec_params);
    }

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|error| {
            ConvertError::Audio(format!("could not start the audio decoder: {error}"))
        })?;

    let mut sample_buffer: Option<SampleBuffer<f32>> = None;
    let mut samples = Vec::new();
    let mut sample_rate = codec_params.sample_rate.unwrap_or(0);
    let mut channels = codec_params
        .channels
        .map(|channels| channels.count())
        .unwrap_or(0);

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(error) if is_end_of_stream(&error) => break,
            Err(SymphoniaError::ResetRequired) => break,
            Err(error) => {
                return Err(ConvertError::Audio(format!(
                    "could not read audio packets: {error}"
                )))
            }
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(error) if is_end_of_stream(&error) => break,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(error) => {
                return Err(ConvertError::Audio(format!(
                    "could not decode audio samples: {error}"
                )))
            }
        };

        let spec = *decoded.spec();
        sample_rate = spec.rate;
        channels = spec.channels.count();
        let capacity = decoded.capacity();
        if sample_buffer
            .as_ref()
            .map_or(true, |buffer| buffer.capacity() < capacity)
        {
            sample_buffer = Some(SampleBuffer::<f32>::new(capacity as u64, spec));
        }
        let buffer = sample_buffer
            .as_mut()
            .expect("the sample buffer was just initialized");
        buffer.copy_interleaved_ref(decoded);
        samples.extend_from_slice(buffer.samples());
        if samples.len() > MAX_AUDIO_SAMPLES {
            return Err(ConvertError::AudioTooLarge);
        }
    }

    if samples.is_empty() || channels == 0 || sample_rate == 0 {
        return Err(ConvertError::NoAudioTrack);
    }
    Ok(DecodedAudio {
        samples,
        channels,
        sample_rate,
    })
}

fn is_ogg_opus(input: &[u8]) -> bool {
    input.starts_with(b"OggS") && input.get(28..36).is_some_and(|magic| magic == b"OpusHead")
}

fn opus_channels(channels: usize) -> Result<usize, ConvertError> {
    if channels > 2 {
        return Err(ConvertError::Audio(
            "multichannel Opus audio is not supported".to_owned(),
        ));
    }
    Ok(channels)
}

fn append_audio_samples(samples: &mut Vec<f32>, decoded: &[f32]) -> Result<(), ConvertError> {
    samples.extend_from_slice(decoded);
    if samples.len() > MAX_AUDIO_SAMPLES {
        return Err(ConvertError::AudioTooLarge);
    }
    Ok(())
}

fn decode_ogg_opus(input: &[u8]) -> Result<DecodedAudio, ConvertError> {
    use opus_pure::{OggOpusReader, Trim, MAX_PACKET_SAMPLES};

    let mut reader = OggOpusReader::new(Cursor::new(input))
        .map_err(|error| ConvertError::Audio(format!("could not open the Opus stream: {error}")))?;
    let head = reader.head().clone();
    let channels = opus_channels(head.channel_count as usize)?;
    let mut decoder = head.decoder(48_000).map_err(|error| {
        ConvertError::Audio(format!("could not start the Opus decoder: {error}"))
    })?;
    let mut trim = Trim::new(&head, 48_000, channels)
        .map_err(|error| ConvertError::Audio(format!("could not read the Opus stream: {error}")))?;
    let mut block = vec![0.0f32; MAX_PACKET_SAMPLES * channels];
    let mut samples = Vec::new();

    for packet in reader.packets() {
        let packet = packet.map_err(|error| {
            ConvertError::Audio(format!("could not read Opus packets: {error}"))
        })?;
        let decoded = decoder
            .decode(&packet.data, MAX_PACKET_SAMPLES, &mut block)
            .map_err(|error| {
                ConvertError::Audio(format!("could not decode Opus audio: {error}"))
            })?;
        append_audio_samples(
            &mut samples,
            trim.keep(&packet, &block[..decoded * channels]),
        )?;
    }

    if samples.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    Ok(DecodedAudio {
        samples,
        channels,
        sample_rate: 48_000,
    })
}

fn decode_opus_track(
    format: &mut dyn symphonia::core::formats::FormatReader,
    track_id: u32,
    codec_params: &symphonia::core::codecs::CodecParameters,
) -> Result<DecodedAudio, ConvertError> {
    use opus_pure::{OpusDecoder, MAX_PACKET_SAMPLES};

    let (channels, pre_skip) = opus_stream_info(codec_params)?;
    let mut decoder = OpusDecoder::new(48_000, channels).map_err(|error| {
        ConvertError::Audio(format!("could not start the Opus decoder: {error}"))
    })?;
    let mut block = vec![0.0f32; MAX_PACKET_SAMPLES * channels];
    let mut samples = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(error) if is_end_of_stream(&error) => break,
            Err(symphonia::core::errors::Error::ResetRequired) => break,
            Err(error) => {
                return Err(ConvertError::Audio(format!(
                    "could not read audio packets: {error}"
                )))
            }
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = decoder
            .decode(&packet.data, MAX_PACKET_SAMPLES, &mut block)
            .map_err(|error| {
                ConvertError::Audio(format!("could not decode Opus audio: {error}"))
            })?;
        append_audio_samples(&mut samples, &block[..decoded * channels])?;
    }

    let skip = (pre_skip as usize * channels).min(samples.len());
    samples.drain(..skip);

    if samples.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    Ok(DecodedAudio {
        samples,
        channels,
        sample_rate: 48_000,
    })
}

fn opus_stream_info(
    codec_params: &symphonia::core::codecs::CodecParameters,
) -> Result<(usize, u32), ConvertError> {
    let head = codec_params
        .extra_data
        .as_deref()
        .filter(|data| data.len() >= 19 && data.starts_with(b"OpusHead"));
    let channels = codec_params
        .channels
        .map(|channels| channels.count())
        .or_else(|| head.map(|data| usize::from(data[9])))
        .filter(|channels| *channels > 0)
        .ok_or_else(|| {
            ConvertError::Audio("the Opus track does not declare a channel count".to_owned())
        })?;
    let channels = opus_channels(channels)?;
    // The OpusHead pre-skip is the decoder delay. Container delay metadata is
    // not the same number: in MP4 it is expressed in the track timescale and
    // can be much larger, so only fall back to it when there is no header.
    // Symphonia builds MP4 extra data by prefixing "OpusHead" to the big-endian
    // dOps payload, so version 0 fields are big-endian while an Ogg OpusHead
    // (version 1) stores them little-endian.
    let pre_skip = head
        .map(|data| {
            let bytes = [data[10], data[11]];
            if data[8] == 1 {
                u32::from(u16::from_le_bytes(bytes))
            } else {
                u32::from(u16::from_be_bytes(bytes))
            }
        })
        .or(codec_params.delay)
        .unwrap_or(0);
    Ok((channels, pre_skip))
}

fn decode_flac(input: &[u8]) -> Result<DecodedAudio, ConvertError> {
    let input = strip_id3v2(input);
    let mut reader = claxon::FlacReader::new(Cursor::new(input))
        .map_err(|error| ConvertError::Audio(format!("could not open the FLAC stream: {error}")))?;
    let info = reader.streaminfo();
    let channels = info.channels as usize;
    let sample_rate = info.sample_rate;
    if channels == 0 || sample_rate == 0 || info.bits_per_sample == 0 {
        return Err(ConvertError::NoAudioTrack);
    }

    let scale = 1.0 / (1_u32 << (info.bits_per_sample - 1)) as f32;
    let mut samples = Vec::new();
    for sample in reader.samples() {
        let sample = sample.map_err(|error| {
            ConvertError::Audio(format!("could not decode FLAC samples: {error}"))
        })?;
        samples.push(sample as f32 * scale);
        if samples.len() > MAX_AUDIO_SAMPLES {
            return Err(ConvertError::AudioTooLarge);
        }
    }

    if samples.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    Ok(DecodedAudio {
        samples,
        channels,
        sample_rate,
    })
}

fn apply_audio_options(
    mut audio: DecodedAudio,
    options: ConversionOptions,
) -> Result<DecodedAudio, ConvertError> {
    if let Some(channels) = options.audio_channels {
        audio = remix_channels(audio, channels);
    }
    if let Some(sample_rate) = options.audio_sample_rate {
        audio = resample_audio(audio, sample_rate)?;
    }
    if audio.samples.len() > MAX_AUDIO_SAMPLES {
        return Err(ConvertError::AudioTooLarge);
    }
    Ok(audio)
}

fn remix_channels(audio: DecodedAudio, target: AudioChannels) -> DecodedAudio {
    let target_channels = target.count();
    if audio.channels == target_channels {
        return audio;
    }

    let frames = audio.samples.len() / audio.channels;
    let mut samples = Vec::with_capacity(frames * target_channels);
    for frame in audio.samples.chunks_exact(audio.channels) {
        match target {
            AudioChannels::Mono => {
                samples.push(frame.iter().sum::<f32>() / audio.channels as f32);
            }
            AudioChannels::Stereo => {
                if audio.channels == 1 {
                    samples.extend_from_slice(&[frame[0], frame[0]]);
                } else {
                    samples.extend_from_slice(&frame[..2]);
                }
            }
        }
    }

    DecodedAudio {
        samples,
        channels: target_channels,
        sample_rate: audio.sample_rate,
    }
}

fn resample_audio(audio: DecodedAudio, target_rate: u32) -> Result<DecodedAudio, ConvertError> {
    let target_rate = target_rate.clamp(
        ConversionOptions::MIN_AUDIO_SAMPLE_RATE,
        ConversionOptions::MAX_AUDIO_SAMPLE_RATE,
    );
    if target_rate == audio.sample_rate {
        return Ok(audio);
    }

    let channels = audio.channels;
    let source_frames = audio.samples.len() / channels;
    if source_frames == 0 {
        return Ok(audio);
    }
    let target_frames = ((source_frames as f64 * f64::from(target_rate))
        / f64::from(audio.sample_rate))
    .round()
    .max(1.0) as usize;
    let sample_count = target_frames
        .checked_mul(channels)
        .ok_or(ConvertError::AudioTooLarge)?;
    if sample_count > MAX_AUDIO_SAMPLES {
        return Err(ConvertError::AudioTooLarge);
    }

    let mut samples = Vec::with_capacity(sample_count);
    for frame in 0..target_frames {
        let source_position = frame as f64 * f64::from(audio.sample_rate) / f64::from(target_rate);
        let source_floor = source_position.floor();
        let source_index = (source_floor as usize).min(source_frames - 1);
        let next_index = (source_index + 1).min(source_frames - 1);
        let fraction = (source_position - source_floor) as f32;
        for channel in 0..channels {
            let current = audio.samples[source_index * channels + channel];
            let next = audio.samples[next_index * channels + channel];
            samples.push(current + (next - current) * fraction);
        }
    }

    Ok(DecodedAudio {
        samples,
        channels,
        sample_rate: target_rate,
    })
}

fn strip_id3v2(input: &[u8]) -> &[u8] {
    if input.len() < 10 || &input[..3] != b"ID3" {
        return input;
    }

    let size = ((input[6] as usize & 0x7f) << 21)
        | ((input[7] as usize & 0x7f) << 14)
        | ((input[8] as usize & 0x7f) << 7)
        | (input[9] as usize & 0x7f);
    let footer = if input[5] & 0x10 != 0 { 10 } else { 0 };
    let end = 10 + size + footer;
    if end <= input.len() {
        &input[end..]
    } else {
        input
    }
}

fn encode_wav(audio: &DecodedAudio) -> Result<Vec<u8>, ConvertError> {
    let channels = u16::try_from(audio.channels).map_err(|_| ConvertError::AudioTooLarge)?;
    let block_align = channels.checked_mul(2).ok_or(ConvertError::AudioTooLarge)?;
    let byte_rate = audio
        .sample_rate
        .checked_mul(u32::from(block_align))
        .ok_or(ConvertError::AudioTooLarge)?;
    let data_size = u32::try_from(
        audio
            .samples
            .len()
            .checked_mul(2)
            .ok_or(ConvertError::AudioTooLarge)?,
    )
    .map_err(|_| ConvertError::AudioTooLarge)?;
    let riff_size = 36_u32
        .checked_add(data_size)
        .ok_or(ConvertError::AudioTooLarge)?;

    let mut output = Vec::with_capacity(44 + data_size as usize);
    output.extend_from_slice(b"RIFF");
    output.extend_from_slice(&riff_size.to_le_bytes());
    output.extend_from_slice(b"WAVEfmt ");
    output.extend_from_slice(&16_u32.to_le_bytes());
    output.extend_from_slice(&1_u16.to_le_bytes());
    output.extend_from_slice(&channels.to_le_bytes());
    output.extend_from_slice(&audio.sample_rate.to_le_bytes());
    output.extend_from_slice(&byte_rate.to_le_bytes());
    output.extend_from_slice(&block_align.to_le_bytes());
    output.extend_from_slice(&16_u16.to_le_bytes());
    output.extend_from_slice(b"data");
    output.extend_from_slice(&data_size.to_le_bytes());
    for sample in &audio.samples {
        output.extend_from_slice(&sample_to_i16(*sample).to_le_bytes());
    }
    Ok(output)
}

fn encode_aiff(audio: &DecodedAudio) -> Result<Vec<u8>, ConvertError> {
    let channels = u16::try_from(audio.channels).map_err(|_| ConvertError::AudioTooLarge)?;
    let usable_samples = audio.samples.len() - audio.samples.len() % audio.channels;
    let frames =
        u32::try_from(usable_samples / audio.channels).map_err(|_| ConvertError::AudioTooLarge)?;
    let data_size = u32::try_from(usable_samples * 2).map_err(|_| ConvertError::AudioTooLarge)?;
    let form_size = 46_u32
        .checked_add(data_size)
        .ok_or(ConvertError::AudioTooLarge)?;

    let mut output = Vec::with_capacity(54 + data_size as usize);
    output.extend_from_slice(b"FORM");
    output.extend_from_slice(&form_size.to_be_bytes());
    output.extend_from_slice(b"AIFF");
    output.extend_from_slice(b"COMM");
    output.extend_from_slice(&18_u32.to_be_bytes());
    output.extend_from_slice(&channels.to_be_bytes());
    output.extend_from_slice(&frames.to_be_bytes());
    output.extend_from_slice(&16_u16.to_be_bytes());
    output.extend_from_slice(&extended_sample_rate(audio.sample_rate));
    output.extend_from_slice(b"SSND");
    output.extend_from_slice(&(8 + data_size).to_be_bytes());
    output.extend_from_slice(&0_u32.to_be_bytes());
    output.extend_from_slice(&0_u32.to_be_bytes());
    for sample in &audio.samples[..usable_samples] {
        output.extend_from_slice(&sample_to_i16(*sample).to_be_bytes());
    }
    Ok(output)
}

fn extended_sample_rate(sample_rate: u32) -> [u8; 10] {
    if sample_rate == 0 {
        return [0; 10];
    }
    let exponent = 31 - sample_rate.leading_zeros();
    let mantissa = u64::from(sample_rate) << (63 - exponent);
    let biased_exponent = (exponent + 16_383) as u16;
    let mut bytes = [0; 10];
    bytes[..2].copy_from_slice(&biased_exponent.to_be_bytes());
    bytes[2..].copy_from_slice(&mantissa.to_be_bytes());
    bytes
}

fn encode_au(audio: &DecodedAudio) -> Result<Vec<u8>, ConvertError> {
    let channels = u32::try_from(audio.channels).map_err(|_| ConvertError::AudioTooLarge)?;
    let usable_samples = audio.samples.len() - audio.samples.len() % audio.channels;
    let data_size = u32::try_from(usable_samples * 2).map_err(|_| ConvertError::AudioTooLarge)?;

    let mut output = Vec::with_capacity(24 + data_size as usize);
    output.extend_from_slice(b".snd");
    output.extend_from_slice(&24_u32.to_be_bytes());
    output.extend_from_slice(&data_size.to_be_bytes());
    output.extend_from_slice(&3_u32.to_be_bytes());
    output.extend_from_slice(&audio.sample_rate.to_be_bytes());
    output.extend_from_slice(&channels.to_be_bytes());
    for sample in &audio.samples[..usable_samples] {
        output.extend_from_slice(&sample_to_i16(*sample).to_be_bytes());
    }
    Ok(output)
}

fn opus_sample_rate(sample_rate: u32) -> u32 {
    match sample_rate {
        0..=8_000 => 8_000,
        8_001..=12_000 => 12_000,
        12_001..=16_000 => 16_000,
        16_001..=24_000 => 24_000,
        _ => 48_000,
    }
}

fn encode_opus(audio: DecodedAudio) -> Result<Vec<u8>, ConvertError> {
    use opus_pure::{Application, OggOpusWriter, OpusEncoder, OpusHead, MAX_PACKET_BYTES};

    if audio.channels == 0 || audio.sample_rate == 0 || audio.samples.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    let mut audio = if audio.channels > 2 {
        remix_channels(audio, AudioChannels::Stereo)
    } else {
        audio
    };
    let sample_rate = opus_sample_rate(audio.sample_rate);
    if audio.sample_rate != sample_rate {
        audio = resample_audio(audio, sample_rate)?;
    }

    let channels = audio.channels;
    let mut encoder =
        OpusEncoder::new(sample_rate as i32, channels, Application::Audio).map_err(|error| {
            ConvertError::Audio(format!("could not start the Opus encoder: {error}"))
        })?;
    encoder.bitrate_bps = if channels == 1 { 64_000 } else { 96_000 };
    let head = OpusHead::for_encoder(&encoder, sample_rate);
    let pre_skip = u64::from(head.pre_skip);
    let mut writer = OggOpusWriter::new(Vec::new(), head).map_err(|error| {
        ConvertError::Audio(format!("could not start the Opus stream: {error}"))
    })?;

    let total = audio.samples.len() / channels;
    let frame_size = (sample_rate / 50) as usize;
    let ticks = 48_000 / sample_rate;
    // The encoder runs `pre_skip` samples behind its input, so flush that much
    // silence and round up to whole frames.
    let flush = (pre_skip + u64::from(ticks) - 1) / u64::from(ticks);
    let frames = (total + flush as usize + frame_size - 1) / frame_size;
    let final_granule = pre_skip + total as u64 * 48_000 / u64::from(sample_rate);
    let mut packet = vec![0_u8; MAX_PACKET_BYTES];
    let mut block = vec![0.0f32; frame_size * channels];

    for i in 0..frames {
        let offset = i * frame_size;
        let end = (offset + frame_size).min(total);
        block.fill(0.0);
        if offset < total {
            block[..(end - offset) * channels]
                .copy_from_slice(&audio.samples[offset * channels..end * channels]);
        }
        let len = encoder
            .encode(&block, frame_size, &mut packet)
            .map_err(|error| {
                ConvertError::Audio(format!("could not encode Opus audio: {error}"))
            })?;
        if i + 1 == frames {
            let duration = final_granule.saturating_sub(writer.granule() as u64) as u32;
            writer
                .write_packet_with_duration(&packet[..len], duration)
                .map_err(|error| {
                    ConvertError::Audio(format!("could not write the Opus stream: {error}"))
                })?;
        } else {
            writer.write_packet(&packet[..len]).map_err(|error| {
                ConvertError::Audio(format!("could not write the Opus stream: {error}"))
            })?;
        }
    }

    writer
        .finish()
        .map_err(|error| ConvertError::Audio(format!("could not finish the Opus stream: {error}")))
}

fn sample_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

fn encode_flac(audio: &DecodedAudio) -> Result<Vec<u8>, ConvertError> {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;

    let usable_samples = audio.samples.len() - audio.samples.len() % audio.channels;
    let integer_samples: Vec<i32> = audio.samples[..usable_samples]
        .iter()
        .map(|sample| i32::from(sample_to_i16(*sample)))
        .collect();

    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, error)| ConvertError::Audio(error.to_string()))?;
    let source = flacenc::source::MemSource::from_samples(
        &integer_samples,
        audio.channels,
        16,
        audio.sample_rate as usize,
    );
    let stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|error| ConvertError::Audio(error.to_string()))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|error| ConvertError::Audio(error.to_string()))?;
    Ok(sink.as_slice().to_vec())
}

const PDF_TEXT_LINE_HEIGHT: f32 = 1.2;
/// Courier advances 0.6 em per character.
const PDF_TEXT_CHAR_WIDTH: f32 = 0.6;

fn image_to_pdf(
    input: &[u8],
    extension: &str,
    options: ConversionOptions,
) -> Result<Vec<u8>, ConvertError> {
    if let Some(components @ (1 | 3)) = jpeg_component_count(input) {
        let (width, height) = image::ImageReader::new(Cursor::new(input))
            .with_guessed_format()?
            .into_dimensions()?;
        return Ok(build_image_pdf(
            input,
            width,
            height,
            Filter::DctDecode,
            components == 1,
            options,
        ));
    }

    let image = if extension == "svg" {
        rasterize_svg(input)?
    } else {
        image::load_from_memory(input)?
    };
    let width = image.width();
    let height = image.height();
    let compressed = deflate_bytes(&flatten_to_rgb(&image))?;
    Ok(build_image_pdf(
        &compressed,
        width,
        height,
        Filter::FlateDecode,
        false,
        options,
    ))
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ImagePlacement {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

fn image_placement(
    image_width: f32,
    image_height: f32,
    page_width: f32,
    page_height: f32,
    fit: PdfImageFit,
) -> ImagePlacement {
    let scale_x = page_width / image_width;
    let scale_y = page_height / image_height;
    let scale = match fit {
        PdfImageFit::Contain => scale_x.min(scale_y),
        PdfImageFit::Cover => scale_x.max(scale_y),
    };
    let width = image_width * scale;
    let height = image_height * scale;
    ImagePlacement {
        x: (page_width - width) / 2.0,
        y: (page_height - height) / 2.0,
        width,
        height,
    }
}

fn build_image_pdf(
    data: &[u8],
    width: u32,
    height: u32,
    filter: Filter,
    grayscale: bool,
    options: ConversionOptions,
) -> Vec<u8> {
    let (page_width, page_height) = options
        .pdf_page_size
        .dimensions_for(options.pdf_orientation);
    let placement = image_placement(
        width as f32,
        height as f32,
        page_width,
        page_height,
        options.pdf_image_fit,
    );

    let catalog_id = Ref::new(1);
    let pages_id = Ref::new(2);
    let page_id = Ref::new(3);
    let content_id = Ref::new(4);
    let image_id = Ref::new(5);

    let mut content = Content::new();
    content.save_state();
    content.transform([
        placement.width,
        0.0,
        0.0,
        placement.height,
        placement.x,
        placement.y,
    ]);
    content.x_object(Name(b"Im1"));
    content.restore_state();
    let content = content.finish();

    let mut pdf = Pdf::new();
    pdf.catalog(catalog_id).pages(pages_id);
    pdf.pages(pages_id).kids([page_id]).count(1);
    {
        let mut page = pdf.page(page_id);
        page.parent(pages_id)
            .media_box(Rect::new(0.0, 0.0, page_width, page_height))
            .contents(content_id);
        page.resources().x_objects().pair(Name(b"Im1"), image_id);
    }
    pdf.stream(content_id, &content);
    {
        let mut x_object = pdf.image_xobject(image_id, data);
        x_object.width(width as i32);
        x_object.height(height as i32);
        if grayscale {
            x_object.color_space().device_gray();
        } else {
            x_object.color_space().device_rgb();
        }
        x_object.bits_per_component(8);
        x_object.filter(filter);
    }
    pdf.finish()
}

/// Reads the component count from a JPEG's SOF marker so RGB and grayscale
/// photos can be embedded without re-encoding.
fn jpeg_component_count(bytes: &[u8]) -> Option<u8> {
    if bytes.len() < 4 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut index = 2;
    while index + 4 <= bytes.len() {
        if bytes[index] != 0xff {
            return None;
        }
        let mut marker_index = index + 1;
        while marker_index < bytes.len() && bytes[marker_index] == 0xff {
            marker_index += 1;
        }
        let marker = *bytes.get(marker_index)?;
        if marker == 0x01 || marker == 0xd8 || (0xd0..=0xd7).contains(&marker) {
            index = marker_index + 1;
            continue;
        }
        if marker == 0xda || marker == 0xd9 {
            return None;
        }
        let length =
            u16::from_be_bytes([*bytes.get(marker_index + 1)?, *bytes.get(marker_index + 2)?])
                as usize;
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            return bytes.get(marker_index + 8).copied();
        }
        index = marker_index + 1 + length;
    }
    None
}

fn flatten_to_rgb(image: &image::DynamicImage) -> Vec<u8> {
    if !image.color().has_alpha() {
        return image.to_rgb8().into_raw();
    }
    let rgba = image.to_rgba8();
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for pixel in rgba.pixels() {
        let alpha = u16::from(pixel[3]);
        for channel in &pixel.0[..3] {
            let value = (u16::from(*channel) * alpha + 255 * (255 - alpha) + 127) / 255;
            rgb.push(value as u8);
        }
    }
    rgb
}

fn deflate_bytes(data: &[u8]) -> Result<Vec<u8>, ConvertError> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

fn text_to_pdf(input: &[u8], options: ConversionOptions) -> Result<Vec<u8>, ConvertError> {
    let text = decode_text(input)?;
    let (page_width, page_height) = options.pdf_page_size.dimensions();
    let font_size = options.pdf_font_size.clamp(
        ConversionOptions::MIN_PDF_FONT_SIZE,
        ConversionOptions::MAX_PDF_FONT_SIZE,
    ) as f32;
    let margin = options.pdf_margin.clamp(
        ConversionOptions::MIN_PDF_MARGIN,
        ConversionOptions::MAX_PDF_MARGIN,
    ) as f32;
    let leading = font_size * PDF_TEXT_LINE_HEIGHT;
    let max_chars =
        (((page_width - 2.0 * margin) / (font_size * PDF_TEXT_CHAR_WIDTH)).floor() as usize).max(1);
    let lines_per_page = (((page_height - 2.0 * margin) / leading).floor() as usize).max(1);
    let lines = wrap_text_lines(&text, max_chars);
    let page_count = lines.len().div_ceil(lines_per_page);

    let catalog_id = Ref::new(1);
    let pages_id = Ref::new(2);
    let font_id = Ref::new(3);

    let mut pdf = Pdf::new();
    pdf.catalog(catalog_id).pages(pages_id);
    pdf.type1_font(font_id)
        .base_font(Name(b"Courier"))
        .encoding_predefined(Name(b"WinAnsiEncoding"));
    pdf.pages(pages_id)
        .kids((0..page_count).map(|index| Ref::new(4 + (index as i32) * 2)))
        .count(page_count as i32);

    for page_index in 0..page_count {
        let page_id = Ref::new(4 + (page_index as i32) * 2);
        let content_id = Ref::new(5 + (page_index as i32) * 2);
        let start = page_index * lines_per_page;
        let end = (start + lines_per_page).min(lines.len());

        let mut content = Content::new();
        content.begin_text();
        content.set_font(Name(b"F1"), font_size);
        content.set_text_matrix([1.0, 0.0, 0.0, 1.0, margin, page_height - margin - font_size]);
        for (line_index, line) in lines[start..end].iter().enumerate() {
            if line_index > 0 {
                content.next_line(0.0, -leading);
            }
            content.show(Str(&win_ansi_bytes(line)));
        }
        content.end_text();
        let content = content.finish();

        {
            let mut page = pdf.page(page_id);
            page.parent(pages_id)
                .media_box(Rect::new(0.0, 0.0, page_width, page_height))
                .contents(content_id);
            page.resources().fonts().pair(Name(b"F1"), font_id);
        }
        pdf.stream(content_id, &content);
    }
    Ok(pdf.finish())
}

fn wrap_text_lines(text: &str, max_chars: usize) -> Vec<String> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = Vec::new();
    for raw_line in normalized.split('\n') {
        if raw_line.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in raw_line.split(' ') {
            if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > max_chars
            {
                lines.push(std::mem::take(&mut current));
            }
            if current.is_empty() {
                if word.chars().count() <= max_chars {
                    current.push_str(word);
                } else {
                    hard_wrap_word(word, &mut lines, &mut current, max_chars);
                }
            } else {
                current.push(' ');
                current.push_str(word);
            }
        }
        lines.push(current);
    }
    lines
}

fn hard_wrap_word(word: &str, lines: &mut Vec<String>, current: &mut String, max_chars: usize) {
    let characters: Vec<char> = word.chars().collect();
    let mut start = 0;
    while start + max_chars < characters.len() {
        lines.push(characters[start..start + max_chars].iter().collect());
        start += max_chars;
    }
    current.extend(characters[start..].iter());
}

fn win_ansi_bytes(text: &str) -> Vec<u8> {
    text.chars()
        .map(|character| win_ansi_byte(character).unwrap_or(b'?'))
        .collect()
}

fn win_ansi_byte(character: char) -> Option<u8> {
    match character {
        '\u{20}'..='\u{7e}' | '\u{a0}'..='\u{ff}' => Some(character as u8),
        '\u{20ac}' => Some(0x80),
        '\u{201a}' => Some(0x82),
        '\u{0192}' => Some(0x83),
        '\u{201e}' => Some(0x84),
        '\u{2026}' => Some(0x85),
        '\u{2020}' => Some(0x86),
        '\u{2021}' => Some(0x87),
        '\u{02c6}' => Some(0x88),
        '\u{2030}' => Some(0x89),
        '\u{0160}' => Some(0x8a),
        '\u{2039}' => Some(0x8b),
        '\u{0152}' => Some(0x8c),
        '\u{017d}' => Some(0x8e),
        '\u{2018}' => Some(0x91),
        '\u{2019}' => Some(0x92),
        '\u{201c}' => Some(0x93),
        '\u{201d}' => Some(0x94),
        '\u{2022}' => Some(0x95),
        '\u{2013}' => Some(0x96),
        '\u{2014}' => Some(0x97),
        '\u{02dc}' => Some(0x98),
        '\u{2122}' => Some(0x99),
        '\u{0161}' => Some(0x9a),
        '\u{203a}' => Some(0x9b),
        '\u{0153}' => Some(0x9c),
        '\u{017e}' => Some(0x9e),
        '\u{0178}' => Some(0x9f),
        '\t' => Some(b' '),
        _ => None,
    }
}

fn create_tar(
    entries: &[(PathBuf, Vec<u8>)],
    gzip: bool,
    compression: Option<u8>,
) -> Result<Vec<u8>, ConvertError> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, bytes) in entries {
        let name = name.to_string_lossy().replace('\\', "/");
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder.append_data(&mut header, name, bytes.as_slice())?;
    }
    let bytes = builder.into_inner()?;
    if gzip {
        gzip_bytes(&bytes, compression)
    } else {
        Ok(bytes)
    }
}

fn gzip_bytes(input: &[u8], compression: Option<u8>) -> Result<Vec<u8>, ConvertError> {
    let level = compression
        .unwrap_or(ConversionOptions::DEFAULT_COMPRESSION_LEVEL)
        .clamp(1, 9);
    let mut encoder =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level.into()));
    encoder.write_all(input)?;
    Ok(encoder.finish()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn png_fixture() -> Vec<u8> {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            2,
            2,
            image::Rgb([20, 40, 60]),
        ));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

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
    fn applies_archive_compression_options() {
        let inputs = vec![(
            PathBuf::from("note.txt"),
            b"the same line repeated many times\n".repeat(2_000),
        )];

        let fast_zip = convert_batch_with_options(
            &inputs,
            OutputFormat::Zip,
            ConversionOptions {
                archive_compression: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let best_zip = convert_batch_with_options(
            &inputs,
            OutputFormat::Zip,
            ConversionOptions {
                archive_compression: Some(9),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            best_zip[0].bytes.len() <= fast_zip[0].bytes.len(),
            "higher ZIP compression should not produce a larger archive"
        );
        assert!(zip::ZipArchive::new(Cursor::new(&best_zip[0].bytes)).is_ok());

        let fast_gzip = convert_batch_with_options(
            &inputs,
            OutputFormat::Gzip,
            ConversionOptions {
                archive_compression: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let best_gzip = convert_batch_with_options(
            &inputs,
            OutputFormat::Gzip,
            ConversionOptions {
                archive_compression: Some(9),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            best_gzip[0].bytes.len() <= fast_gzip[0].bytes.len(),
            "higher GZIP compression should not produce a larger file"
        );
        let mut decoder = flate2::read::GzDecoder::new(Cursor::new(&best_gzip[0].bytes));
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, inputs[0].1);
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
    fn converts_csv_to_json_and_back() {
        let csv = b"name,age\nAda,36\nLin,28\n";
        let json = convert_bytes("people.csv", csv, OutputFormat::Json).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json[0].bytes).unwrap();
        assert_eq!(value[0]["name"], "Ada");
        assert_eq!(value[1]["age"], "28");

        let round_trip = convert_bytes("people.json", &json[0].bytes, OutputFormat::Csv).unwrap();
        let mut reader = csv::Reader::from_reader(round_trip[0].bytes.as_slice());
        let headers = reader.headers().unwrap().clone();
        let name_column = headers.iter().position(|header| header == "name").unwrap();
        let age_column = headers.iter().position(|header| header == "age").unwrap();
        let first = reader.records().next().unwrap().unwrap();
        assert_eq!(&first[name_column], "Ada");
        assert_eq!(&first[age_column], "36");
    }

    #[test]
    fn converts_plain_text_to_json_string_and_back() {
        let json = convert_bytes("note.txt", b"hello\nworld", OutputFormat::Json).unwrap();
        assert_eq!(
            serde_json::from_slice::<String>(&json[0].bytes).unwrap(),
            "hello\nworld"
        );
        let text = convert_bytes("note.json", &json[0].bytes, OutputFormat::Text).unwrap();
        assert_eq!(text[0].bytes, b"hello\nworld");
    }

    #[test]
    fn converts_text_between_the_web_supported_unicode_encodings() {
        let content = "Hello 🌍";
        for target in [
            OutputFormat::TextUtf8NoBom,
            OutputFormat::TextUtf8Bom,
            OutputFormat::TextUtf16Le,
            OutputFormat::TextUtf16Be,
            OutputFormat::TextUtf32Le,
            OutputFormat::TextUtf32Be,
        ] {
            let encoded = convert_bytes("sample.txt", content.as_bytes(), target).unwrap();
            let decoded =
                convert_bytes("sample.txt", &encoded[0].bytes, OutputFormat::Text).unwrap();
            assert_eq!(decoded[0].bytes, content.as_bytes());
        }
    }

    #[test]
    fn rejects_invalid_unicode_code_points_in_utf32_text() {
        let input = [0xff, 0xfe, 0x00, 0x00, 0x00, 0xd8, 0x00, 0x00];
        assert!(matches!(
            convert_bytes("sample.txt", &input, OutputFormat::Text),
            Err(ConvertError::InvalidUnicodeCodePoint(0xd800))
        ));
    }

    #[test]
    fn archives_multiple_files() {
        let inputs = vec![
            (PathBuf::from("one.txt"), b"one".to_vec()),
            (PathBuf::from("two.txt"), b"two".to_vec()),
        ];
        let archive = convert_batch(&inputs, OutputFormat::Zip).unwrap();
        assert_eq!(archive[0].name, PathBuf::from("converted.zip"));

        let mut zip = zip::ZipArchive::new(Cursor::new(&archive[0].bytes)).unwrap();
        assert_eq!(zip.len(), 2);
        let mut one = String::new();
        zip.by_name("one.txt")
            .unwrap()
            .read_to_string(&mut one)
            .unwrap();
        assert_eq!(one, "one");
        let mut two = String::new();
        zip.by_name("two.txt")
            .unwrap()
            .read_to_string(&mut two)
            .unwrap();
        assert_eq!(two, "two");
    }

    #[test]
    fn rejects_duplicate_archive_entry_names() {
        let inputs = vec![
            (PathBuf::from("first/one.txt"), b"one".to_vec()),
            (PathBuf::from("second/one.txt"), b"two".to_vec()),
        ];
        assert!(matches!(
            convert_batch(&inputs, OutputFormat::Zip),
            Err(ConvertError::DuplicateArchiveEntryName(name)) if name == "one.txt"
        ));
    }

    #[test]
    fn rejects_empty_batches() {
        assert!(matches!(
            convert_batch(&[], OutputFormat::Json),
            Err(ConvertError::NoInputFiles)
        ));
    }

    #[test]
    fn reports_unsupported_format_pairs() {
        let error = convert_bytes("movie.mp4", b"not a movie", OutputFormat::Png).unwrap_err();
        assert!(matches!(error, ConvertError::Unsupported { .. }));
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
    fn converts_between_json_yaml_and_toml() {
        let json = br#"{"name":"Ada","age":36,"tags":["math","code"]}"#;

        let yaml = convert_bytes("person.json", json, OutputFormat::Yaml).unwrap();
        let back = convert_bytes("person.yaml", &yaml[0].bytes, OutputFormat::Json).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&back[0].bytes).unwrap();
        assert_eq!(value["name"], "Ada");
        assert_eq!(value["age"], 36);
        assert_eq!(value["tags"][1], "code");

        let toml = convert_bytes("person.json", json, OutputFormat::Toml).unwrap();
        let back = convert_bytes("person.toml", &toml[0].bytes, OutputFormat::Json).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&back[0].bytes).unwrap();
        assert_eq!(value["name"], "Ada");
        assert_eq!(value["age"], 36);
        assert_eq!(value["tags"][1], "code");
    }

    #[test]
    fn packs_tar_archives() {
        let inputs = vec![
            (PathBuf::from("one.txt"), b"one".to_vec()),
            (PathBuf::from("two.txt"), b"two".to_vec()),
        ];

        for (target, name) in [
            (OutputFormat::Tar, "converted.tar"),
            (OutputFormat::TarGz, "converted.tar.gz"),
        ] {
            let archive = convert_batch(&inputs, target).unwrap();
            assert_eq!(archive[0].name, PathBuf::from(name));

            let reader: Box<dyn Read> = if target == OutputFormat::TarGz {
                Box::new(flate2::read::GzDecoder::new(Cursor::new(&archive[0].bytes)))
            } else {
                Box::new(Cursor::new(&archive[0].bytes))
            };
            let mut tar = tar::Archive::new(reader);
            let mut entries = Vec::new();
            for entry in tar.entries().unwrap() {
                let mut entry = entry.unwrap();
                let path = entry.path().unwrap().to_string_lossy().into_owned();
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                entries.push((path, bytes));
            }
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].0, "one.txt");
            assert_eq!(entries[0].1, b"one");
            assert_eq!(entries[1].0, "two.txt");
            assert_eq!(entries[1].1, b"two");
        }
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

    #[test]
    fn compresses_gzip_files() {
        let gzip = convert_bytes("note.txt", b"hello", OutputFormat::Gzip).unwrap();
        assert_eq!(gzip[0].name, PathBuf::from("note.txt.gz"));
        assert_eq!(&gzip[0].bytes[0..2], &[0x1f, 0x8b]);

        let mut decoder = flate2::read::GzDecoder::new(Cursor::new(&gzip[0].bytes));
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, b"hello");
    }

    fn wav_fixture() -> Vec<u8> {
        let sample_rate = 8_000_u32;
        let channels = 2_u16;
        let frames = 8_000_u32;
        let mut samples = Vec::new();
        for frame in 0..frames {
            let value = ((frame as f32 / sample_rate as f32 * 440.0 * std::f32::consts::TAU).sin()
                * 0.5
                * i16::MAX as f32) as i16;
            samples.push(value);
            samples.push(value);
        }

        let data_size = (samples.len() * 2) as u32;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_size).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
        wav.extend_from_slice(&(channels * 2).to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_size.to_le_bytes());
        for sample in samples {
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }

    struct ParsedWav {
        channels: u16,
        sample_rate: u32,
        bits_per_sample: u16,
        samples: Vec<i16>,
    }

    fn parse_wav(bytes: &[u8]) -> ParsedWav {
        assert!(bytes.len() >= 44, "the WAV output is shorter than a header");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize + 8,
            bytes.len(),
            "the RIFF chunk size does not match the file length"
        );

        let mut format = None;
        let mut pcm = None;
        let mut offset = 12;
        while offset + 8 <= bytes.len() {
            let chunk_id = &bytes[offset..offset + 4];
            let chunk_size =
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let body_start = offset + 8;
            let body_end = body_start + chunk_size;
            assert!(
                body_end <= bytes.len(),
                "a WAV chunk runs past the end of the file"
            );
            let body = &bytes[body_start..body_end];

            if chunk_id == &b"fmt "[..] {
                assert_eq!(chunk_size, 16, "unexpected WAV format chunk size");
                let audio_format = u16::from_le_bytes(body[0..2].try_into().unwrap());
                let channels = u16::from_le_bytes(body[2..4].try_into().unwrap());
                let sample_rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                let byte_rate = u32::from_le_bytes(body[8..12].try_into().unwrap());
                let block_align = u16::from_le_bytes(body[12..14].try_into().unwrap());
                let bits_per_sample = u16::from_le_bytes(body[14..16].try_into().unwrap());

                assert_eq!(audio_format, 1, "expected integer PCM audio");
                assert_eq!(bits_per_sample, 16);
                assert_eq!(u32::from(block_align), u32::from(channels) * 2);
                assert_eq!(byte_rate, sample_rate * u32::from(block_align));
                format = Some((channels, sample_rate, bits_per_sample));
            } else if chunk_id == &b"data"[..] {
                assert_eq!(
                    chunk_size % 2,
                    0,
                    "16-bit PCM data must have an even length"
                );
                pcm = Some(
                    body.chunks_exact(2)
                        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                        .collect::<Vec<_>>(),
                );
            }

            offset = body_end + (chunk_size & 1);
        }

        let (channels, sample_rate, bits_per_sample) =
            format.expect("the WAV is missing its format chunk");
        let samples = pcm.expect("the WAV is missing its data chunk");
        assert!(channels > 0, "the WAV declares no channels");
        assert_eq!(
            samples.len() % usize::from(channels),
            0,
            "the sample count is not a whole number of frames"
        );
        ParsedWav {
            channels,
            sample_rate,
            bits_per_sample,
            samples,
        }
    }

    fn assert_audible(samples: &[i16]) {
        assert!(!samples.is_empty(), "the WAV contains no samples");
        let peak = samples
            .iter()
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(peak > 1_000, "the decoded audio is silent (peak {peak})");

        let energy: f64 = samples
            .iter()
            .map(|sample| f64::from(*sample) * f64::from(*sample))
            .sum();
        let rms = (energy / samples.len() as f64).sqrt();
        assert!(
            rms > 100.0,
            "the decoded audio has too little signal (RMS {rms:.1})"
        );
    }

    #[test]
    fn converts_wav_audio_to_flac_and_back() {
        let wav = wav_fixture();
        let original = parse_wav(&wav);
        assert_eq!(original.sample_rate, 8_000);
        assert_eq!(original.channels, 2);
        assert_eq!(original.bits_per_sample, 16);
        assert_eq!(original.samples.len(), 16_000);

        let same_format = convert_bytes("tone.wav", &wav, OutputFormat::Wav).unwrap();
        let reencoded = parse_wav(&same_format[0].bytes);
        assert_eq!(reencoded.sample_rate, original.sample_rate);
        assert_eq!(reencoded.channels, original.channels);
        assert_eq!(reencoded.samples, original.samples);
        assert_audible(&reencoded.samples);

        let flac = convert_bytes("tone.wav", &wav, OutputFormat::Flac).unwrap();
        assert_eq!(flac[0].name, PathBuf::from("tone.flac"));
        assert_eq!(&flac[0].bytes[0..4], b"fLaC");

        let back = convert_bytes("tone.flac", &flac[0].bytes, OutputFormat::Wav).unwrap();
        assert_eq!(back[0].name, PathBuf::from("tone.wav"));
        let decoded = parse_wav(&back[0].bytes);
        assert_eq!(decoded.sample_rate, original.sample_rate);
        assert_eq!(decoded.channels, original.channels);
        assert_eq!(decoded.samples.len(), original.samples.len());
        assert_eq!(decoded.samples, original.samples);
        assert_audible(&decoded.samples);
    }

    #[test]
    fn converts_wav_audio_to_aiff_and_au() {
        let wav = wav_fixture();
        let original = parse_wav(&wav);

        let aiff = convert_bytes("tone.wav", &wav, OutputFormat::Aiff).unwrap();
        assert_eq!(aiff[0].name, PathBuf::from("tone.aiff"));
        assert_eq!(&aiff[0].bytes[0..4], b"FORM");
        assert_eq!(&aiff[0].bytes[8..12], b"AIFF");
        let back = convert_bytes("tone.aiff", &aiff[0].bytes, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&back[0].bytes);
        assert_eq!(decoded.sample_rate, original.sample_rate);
        assert_eq!(decoded.channels, original.channels);
        assert_eq!(decoded.samples, original.samples);
        assert_audible(&decoded.samples);

        let au = convert_bytes("tone.wav", &wav, OutputFormat::Au).unwrap();
        assert_eq!(au[0].name, PathBuf::from("tone.au"));
        assert_eq!(&au[0].bytes[0..4], b".snd");
        assert_eq!(
            u32::from_be_bytes(au[0].bytes[4..8].try_into().unwrap()),
            24
        );
        assert_eq!(
            u32::from_be_bytes(au[0].bytes[8..12].try_into().unwrap()) as usize,
            au[0].bytes.len() - 24
        );
        assert_eq!(
            u32::from_be_bytes(au[0].bytes[12..16].try_into().unwrap()),
            3
        );
        assert_eq!(
            u32::from_be_bytes(au[0].bytes[16..20].try_into().unwrap()),
            original.sample_rate
        );
        assert_eq!(
            u32::from_be_bytes(au[0].bytes[20..24].try_into().unwrap()),
            u32::from(original.channels)
        );
        let au_samples: Vec<i16> = au[0].bytes[24..]
            .chunks_exact(2)
            .map(|bytes| i16::from_be_bytes([bytes[0], bytes[1]]))
            .collect();
        assert_eq!(au_samples, original.samples);
    }

    #[test]
    fn converts_wav_audio_to_opus_and_back() {
        let wav = wav_fixture();

        let opus = convert_bytes("tone.wav", &wav, OutputFormat::Opus).unwrap();
        assert_eq!(opus[0].name, PathBuf::from("tone.opus"));
        assert!(opus[0].bytes.starts_with(b"OggS"));
        let back = convert_bytes("tone.opus", &opus[0].bytes, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&back[0].bytes);
        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.samples.len(), 96_000);
        assert_audible(&decoded.samples);

        let mono = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Opus,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Mono),
                ..Default::default()
            },
        )
        .unwrap();
        let mono_back = convert_bytes("tone.opus", &mono[0].bytes, OutputFormat::Wav).unwrap();
        let mono_decoded = parse_wav(&mono_back[0].bytes);
        assert_eq!(mono_decoded.sample_rate, 48_000);
        assert_eq!(mono_decoded.channels, 1);
        assert_eq!(mono_decoded.samples.len(), 48_000);
        assert_audible(&mono_decoded.samples);

        let sixteen_k = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Opus,
            ConversionOptions {
                audio_sample_rate: Some(16_000),
                ..Default::default()
            },
        )
        .unwrap();
        let sixteen_k_back =
            convert_bytes("tone.opus", &sixteen_k[0].bytes, OutputFormat::Wav).unwrap();
        let sixteen_k_decoded = parse_wav(&sixteen_k_back[0].bytes);
        assert_eq!(sixteen_k_decoded.sample_rate, 48_000);
        assert_eq!(sixteen_k_decoded.samples.len(), 96_000);
        assert_audible(&sixteen_k_decoded.samples);

        // 44.1 kHz is the app's default sample-rate setting, but Opus encodes
        // at 48 kHz, so this covers the default-rate resampling path.
        let default_rate = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Opus,
            ConversionOptions {
                audio_sample_rate: Some(44_100),
                ..Default::default()
            },
        )
        .unwrap();
        let default_rate_back =
            convert_bytes("tone.opus", &default_rate[0].bytes, OutputFormat::Wav).unwrap();
        let default_rate_decoded = parse_wav(&default_rate_back[0].bytes);
        assert_eq!(default_rate_decoded.sample_rate, 48_000);
        assert_eq!(default_rate_decoded.channels, 2);
        assert_eq!(default_rate_decoded.samples.len(), 96_000);
        assert_audible(&default_rate_decoded.samples);
    }

    #[test]
    fn writes_the_aiff_extended_sample_rate() {
        assert_eq!(
            super::extended_sample_rate(44_100),
            [0x40, 0x0e, 0xac, 0x44, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(super::extended_sample_rate(0), [0; 10]);
    }

    #[test]
    fn applies_audio_sample_rate_and_channel_options() {
        let wav = wav_fixture();

        let upsampled = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Wav,
            ConversionOptions {
                audio_sample_rate: Some(16_000),
                ..Default::default()
            },
        )
        .unwrap();
        let parsed = parse_wav(&upsampled[0].bytes);
        assert_eq!(parsed.sample_rate, 16_000);
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.samples.len(), 32_000);
        assert_audible(&parsed.samples);

        let mono = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Wav,
            ConversionOptions {
                audio_sample_rate: Some(16_000),
                audio_channels: Some(AudioChannels::Mono),
                ..Default::default()
            },
        )
        .unwrap();
        let parsed = parse_wav(&mono[0].bytes);
        assert_eq!(parsed.sample_rate, 16_000);
        assert_eq!(parsed.channels, 1);
        assert_eq!(parsed.samples.len(), 16_000);
        assert_audible(&parsed.samples);

        let stereo = convert_bytes_with_options(
            "tone.wav",
            &mono[0].bytes,
            OutputFormat::Wav,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        )
        .unwrap();
        let parsed = parse_wav(&stereo[0].bytes);
        assert_eq!(parsed.sample_rate, 16_000);
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.samples.len(), 32_000);
        assert_audible(&parsed.samples);

        let flac = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Flac,
            ConversionOptions {
                audio_sample_rate: Some(16_000),
                audio_channels: Some(AudioChannels::Mono),
                ..Default::default()
            },
        )
        .unwrap();
        let flac_wav = convert_bytes("tone.flac", &flac[0].bytes, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&flac_wav[0].bytes);
        assert_eq!(parsed.sample_rate, 16_000);
        assert_eq!(parsed.channels, 1);
        assert_eq!(parsed.samples.len(), 16_000);
        assert_audible(&parsed.samples);

        let clamped = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Flac,
            ConversionOptions {
                audio_sample_rate: Some(384_000),
                ..Default::default()
            },
        )
        .unwrap();
        let clamped_wav = convert_bytes("tone.flac", &clamped[0].bytes, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&clamped_wav[0].bytes);
        assert_eq!(parsed.sample_rate, 96_000);
        assert_audible(&parsed.samples);
    }

    #[test]
    fn converts_real_mp3_audio_to_wav_and_flac() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../test/resources/gaster.mp3");
        let input = std::fs::read(path).unwrap();

        let wav = convert_bytes("gaster.mp3", &input, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&wav[0].bytes);
        assert!(parsed.sample_rate >= 8_000);
        assert!(parsed.channels >= 1);
        assert!(parsed.samples.len() > 1_000);
        assert_audible(&parsed.samples);

        let flac = convert_bytes("gaster.mp3", &input, OutputFormat::Flac).unwrap();
        assert_eq!(&flac[0].bytes[0..4], b"fLaC");

        let flac_wav = convert_bytes("gaster.flac", &flac[0].bytes, OutputFormat::Wav).unwrap();
        let flac_parsed = parse_wav(&flac_wav[0].bytes);
        assert_eq!(flac_parsed.sample_rate, parsed.sample_rate);
        assert_eq!(flac_parsed.channels, parsed.channels);
        assert_eq!(flac_parsed.samples.len(), parsed.samples.len());
        let max_difference = flac_parsed
            .samples
            .iter()
            .zip(&parsed.samples)
            .map(|(flac_sample, wav_sample)| {
                (i32::from(*flac_sample) - i32::from(*wav_sample)).unsigned_abs()
            })
            .max()
            .unwrap();
        assert!(
            max_difference <= 1,
            "the FLAC round-trip changed a sample by {max_difference}"
        );
        assert_audible(&flac_parsed.samples);
    }

    #[test]
    fn converts_real_ogg_audio_to_wav() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/piano.ogg");
        let input = std::fs::read(path).unwrap();
        assert_eq!(&input[0..4], b"OggS");

        let wav = convert_bytes("piano.ogg", &input, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&wav[0].bytes);
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.channels, 1);
        assert!(
            parsed.samples.len() >= 384_854,
            "decoded only {} samples",
            parsed.samples.len()
        );
        assert_audible(&parsed.samples);
    }

    #[test]
    fn converts_real_m4a_audio_to_wav() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sample.m4a");
        let input = std::fs::read(path).unwrap();

        let wav = convert_bytes("sample.m4a", &input, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&wav[0].bytes);
        assert_eq!(parsed.sample_rate, 44_100);
        assert_eq!(parsed.channels, 1);
        assert!(
            parsed.samples.len() >= 100_000,
            "decoded only {} samples",
            parsed.samples.len()
        );
        assert_audible(&parsed.samples);
    }

    #[test]
    fn extracts_audio_from_a_video_container() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sample-video.mp4");
        let input = std::fs::read(path).unwrap();

        let wav = convert_bytes("sample-video.mp4", &input, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&wav[0].bytes);
        assert_eq!(parsed.sample_rate, 44_100);
        assert_eq!(parsed.channels, 1);
        assert!(
            parsed.samples.len() >= 100_000,
            "decoded only {} samples",
            parsed.samples.len()
        );
        assert_audible(&parsed.samples);
    }

    #[test]
    fn extracts_audio_from_a_video_with_opus() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sample-opus.webm");
        let input = std::fs::read(path).unwrap();

        let wav = convert_bytes("sample-opus.webm", &input, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&wav[0].bytes);
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.channels, 2);
        assert!(
            parsed.samples.len() >= 200_000,
            "decoded only {} samples",
            parsed.samples.len()
        );
        assert_audible(&parsed.samples);
    }

    #[test]
    fn extracts_audio_from_an_mp4_with_opus() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sample-opus.mp4");
        let input = std::fs::read(path).unwrap();

        let wav = convert_bytes("sample-opus.mp4", &input, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&wav[0].bytes);
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.channels, 2);
        assert!(
            parsed.samples.len() >= 200_000,
            "decoded only {} samples",
            parsed.samples.len()
        );
        assert_audible(&parsed.samples);
    }

    #[test]
    fn extracts_audio_from_vp9_opus_webm_mp4_webm_round_trip() {
        for name in [
            "sample-vp9-opus.webm",
            "sample-vp9-opus.mp4",
            "sample-vp9-opus-roundtrip.webm",
        ] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("testdata")
                .join(name);
            let input = std::fs::read(path).unwrap();

            let wav = convert_bytes(name, &input, OutputFormat::Wav).unwrap();
            let parsed = parse_wav(&wav[0].bytes);
            assert_eq!(parsed.sample_rate, 48_000, "{name}");
            assert_eq!(parsed.channels, 1, "{name}");
            assert!(
                (parsed.samples.len() as i64 - 96_000).abs() <= 1_000,
                "{name} decoded {} samples",
                parsed.samples.len()
            );
            assert_audible(&parsed.samples);
        }
    }

    #[test]
    fn decodes_ogg_opus_audio() {
        use opus_pure::{Application, OggOpusWriter, OpusEncoder, OpusHead, MAX_PACKET_BYTES};

        let sample_rate = 48_000;
        let frame_size = 960;
        let mut encoder = OpusEncoder::new(sample_rate, 1, Application::Audio).unwrap();
        let head = OpusHead::for_encoder(&encoder, sample_rate as u32);
        let mut writer = OggOpusWriter::new(Vec::new(), head).unwrap();
        let mut packet = vec![0_u8; MAX_PACKET_BYTES];
        for frame in 0..50 {
            let pcm: Vec<f32> = (0..frame_size)
                .map(|index| {
                    let time = (frame * frame_size + index) as f32 / sample_rate as f32;
                    (time * 440.0 * std::f32::consts::TAU).sin() * 0.5
                })
                .collect();
            let len = encoder.encode(&pcm, frame_size, &mut packet).unwrap();
            writer.write_packet(&packet[..len]).unwrap();
        }
        let bytes = writer.finish().unwrap();

        let wav = convert_bytes("tone.opus", &bytes, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&wav[0].bytes);
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.channels, 1);
        assert!(
            parsed.samples.len() >= 40_000,
            "decoded only {} samples",
            parsed.samples.len()
        );
        assert_audible(&parsed.samples);
    }

    #[test]
    fn decodes_flac_with_an_id3_tag() {
        let wav = wav_fixture();
        let flac = convert_bytes("tone.wav", &wav, OutputFormat::Flac).unwrap();

        let mut tagged = b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec();
        tagged.extend_from_slice(&flac[0].bytes);
        let back = convert_bytes("tone.flac", &tagged, OutputFormat::Wav).unwrap();
        assert_eq!(&back[0].bytes[0..4], b"RIFF");
    }

    #[test]
    fn recognizes_the_new_input_formats() {
        assert!(OutputFormat::Png.supports_input("icon.svg"));
        assert!(OutputFormat::Ico.supports_input("icon.ico"));
        assert!(OutputFormat::Hdr.supports_input("photo.dds"));
        assert!(OutputFormat::Wav.supports_input("song.mp3"));
        assert!(OutputFormat::Flac.supports_input("song.m4a"));
        assert!(OutputFormat::Aiff.supports_input("song.aiff"));
        assert!(OutputFormat::Opus.supports_input("song.wav"));
        assert!(OutputFormat::Wav.supports_input("movie.mkv"));
        assert!(OutputFormat::Pbm.supports_input("photo.png"));
        assert!(OutputFormat::Yaml.supports_input("config.toml"));
    }

    #[test]
    fn file_picker_extensions_stay_in_sync_with_conversion_inputs() {
        let extensions = supported_input_extensions();
        let unique: std::collections::HashSet<_> = extensions.iter().copied().collect();
        assert_eq!(
            unique.len(),
            extensions.len(),
            "the file picker lists a duplicate extension"
        );

        for extension in &extensions {
            assert_eq!(
                *extension,
                extension.to_ascii_lowercase().as_str(),
                "file picker extensions must be lowercase"
            );
            assert!(
                OutputFormat::ALL
                    .iter()
                    .any(|format| format.supports_input(&format!("sample.{extension}"))),
                "the file picker offers .{extension}, but no output format accepts it"
            );
        }

        for format in OutputFormat::ALL.iter() {
            if matches!(
                *format,
                OutputFormat::Zip | OutputFormat::Tar | OutputFormat::TarGz | OutputFormat::Gzip
            ) {
                continue;
            }
            assert!(
                extensions
                    .iter()
                    .any(|extension| format.supports_input(&format!("sample.{extension}"))),
                "{} has no input extension in the file picker",
                format.label()
            );
        }

        for extension in ["zip", "tar", "tar.gz", "tgz", "gz"] {
            assert!(
                !extensions.contains(&extension),
                "archives must not be in the default Supported files filter"
            );
        }

        for extension in ["ogg", "m4a", "svg"] {
            assert!(
                extensions.contains(&extension),
                "the file picker is missing .{extension}"
            );
        }
    }

    fn jpeg_fixture() -> Vec<u8> {
        let image = image::RgbImage::from_pixel(4, 3, image::Rgb([180, 90, 40]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn converts_images_to_pdf() {
        let converted = convert_bytes("sample.png", &png_fixture(), OutputFormat::Pdf).unwrap();
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].name, PathBuf::from("sample.pdf"));
        let pdf = String::from_utf8_lossy(&converted[0].bytes);
        assert!(pdf.starts_with("%PDF-"));
        assert!(pdf.contains("/Subtype /Image"));
        assert!(pdf.contains("/Filter /FlateDecode"));
        assert!(pdf.contains("/MediaBox [0 0 595.28 841.89]"));
    }

    #[test]
    fn embeds_jpeg_input_without_reencoding() {
        let jpeg = jpeg_fixture();
        let converted = convert_bytes("photo.jpg", &jpeg, OutputFormat::Pdf).unwrap();
        assert!(converted[0]
            .bytes
            .windows(jpeg.len())
            .any(|window| window == jpeg));
        let pdf = String::from_utf8_lossy(&converted[0].bytes);
        assert!(pdf.contains("/Filter /DCTDecode"));
        assert!(pdf.contains("/MediaBox [0 0 595.28 841.89]"));
    }

    #[test]
    fn image_fit_math_centers_and_crops() {
        let contain = super::image_placement(64.0, 48.0, 595.28, 841.89, PdfImageFit::Contain);
        assert!((contain.width - 595.28).abs() < 0.01);
        assert!((contain.height - 446.46).abs() < 0.01);
        assert!(contain.x.abs() < 0.01);
        assert!((contain.y - 197.715).abs() < 0.01);

        let cover = super::image_placement(64.0, 48.0, 792.0, 612.0, PdfImageFit::Cover);
        assert!((cover.width - 816.0).abs() < 0.01);
        assert!((cover.height - 612.0).abs() < 0.01);
        assert!((cover.x + 12.0).abs() < 0.01);
        assert!(cover.y.abs() < 0.01);
    }

    #[test]
    fn applies_pdf_page_orientation_and_image_fit() {
        let converted = convert_bytes_with_options(
            "sample.png",
            &png_fixture(),
            OutputFormat::Pdf,
            ConversionOptions {
                pdf_page_size: PdfPageSize::Letter,
                pdf_orientation: PdfOrientation::Landscape,
                pdf_image_fit: PdfImageFit::Cover,
                ..Default::default()
            },
        )
        .unwrap();
        let pdf = String::from_utf8_lossy(&converted[0].bytes);
        assert!(pdf.contains("/MediaBox [0 0 792 612]"));
        assert!(pdf.contains("792 0 0 792 0 -90 cm"));
    }

    #[test]
    fn converts_text_to_a_paginated_pdf() {
        let text = "Hello, PDF!\n".repeat(120);
        let converted = convert_bytes("notes.txt", text.as_bytes(), OutputFormat::Pdf).unwrap();
        let pdf = String::from_utf8_lossy(&converted[0].bytes);
        assert!(pdf.contains("(Hello, PDF!) Tj"));
        assert!(pdf.contains("/MediaBox [0 0 595.28 841.89]"));
        assert!(pdf.contains("/F1 11 Tf"));
        let pages = pdf.matches("/Type /Page").count() - pdf.matches("/Type /Pages").count();
        assert_eq!(pages, 3);
    }

    #[test]
    fn applies_pdf_page_font_and_margin_options() {
        let text = "Hello, PDF!\n".repeat(120);
        let converted = convert_bytes_with_options(
            "notes.txt",
            text.as_bytes(),
            OutputFormat::Pdf,
            ConversionOptions {
                pdf_page_size: PdfPageSize::Letter,
                pdf_font_size: 20,
                pdf_margin: 100,
                ..Default::default()
            },
        )
        .unwrap();
        let pdf = String::from_utf8_lossy(&converted[0].bytes);
        assert!(pdf.contains("/MediaBox [0 0 612 792]"));
        assert!(pdf.contains("/F1 20 Tf"));
        let pages = pdf.matches("/Type /Page").count() - pdf.matches("/Type /Pages").count();
        assert_eq!(pages, 6);
    }

    #[test]
    fn clamps_pdf_options_to_supported_ranges() {
        let converted = convert_bytes_with_options(
            "notes.txt",
            b"Hello\n",
            OutputFormat::Pdf,
            ConversionOptions {
                pdf_font_size: 0,
                pdf_margin: 10_000,
                ..Default::default()
            },
        )
        .unwrap();
        let pdf = String::from_utf8_lossy(&converted[0].bytes);
        assert!(pdf.contains("/F1 1 Tf"));
        assert!(pdf.contains("1 0 0 1 144 "));
    }

    #[test]
    fn rejects_pdf_output_for_unstructured_data() {
        let error = convert_bytes("data.csv", b"name\nvalue\n", OutputFormat::Pdf).unwrap_err();
        assert!(matches!(error, ConvertError::Unsupported { .. }));
    }
}
