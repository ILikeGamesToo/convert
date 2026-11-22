// SPDX-License-Identifier: GPL-2.0-or-later

use std::{
    collections::{BTreeSet, HashSet},
    fs::File,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use image::ImageEncoder;
use pdf_writer::{Content, Filter, Name, Pdf, Rect, Ref, Str};
use thiserror::Error;

mod input_extensions;
mod streaming;
pub use input_extensions::supported_input_extensions;
pub use streaming::{convert_audio_file_to_writer, convert_audio_file_to_writer_with_cancel};

const MAX_AUDIO_SAMPLES: usize = 120_000_000;
const MAX_SVG_DIMENSION: u32 = 8_192;
const MAX_SVG_PIXELS: u64 = 32_000_000;
const MAX_IMAGE_DIMENSION: u32 = 32_768;
const MAX_IMAGE_ALLOC: u64 = 512 * 1024 * 1024;
const MAX_PDF_PAGES: usize = 5_000;
const MAX_ARCHIVE_ENTRIES: usize = 10_000;

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
    Mp3,
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

/// Sample format for WAV and FLAC output.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AudioBitDepth {
    #[default]
    Bit16,
    Bit24,
    Float32,
}

impl AudioBitDepth {
    pub const ALL: [Self; 3] = [Self::Bit16, Self::Bit24, Self::Float32];

    pub fn label(self) -> &'static str {
        match self {
            Self::Bit16 => "16-bit",
            Self::Bit24 => "24-bit",
            Self::Float32 => "32-bit float",
        }
    }

    /// Bits used to store one sample.
    pub fn bits(self) -> u16 {
        match self {
            Self::Bit16 => 16,
            Self::Bit24 => 24,
            Self::Float32 => 32,
        }
    }
}

/// One audio track that can be decoded from an input file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackInfo {
    /// Zero-based position in the selectable audio-track list.
    pub index: usize,
    /// Container track ID.
    pub id: u32,
    /// Human-readable codec name, such as `AAC` or `AC-3`.
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub channels: usize,
    pub sample_rate: u32,
    /// The first audio track, used when no track is selected.
    pub default: bool,
}

impl AudioTrackInfo {
    /// A compact label for pickers: `#2 · eng · AC-3 · 5.1 · 48 kHz`.
    /// The leading number is the selectable index; the container stream ID is
    /// included when it differs, so an unsupported stream cannot silently
    /// shift the displayed numbering.
    pub fn label(&self) -> String {
        let mut parts = vec![format!("#{}", self.index + 1)];
        if self.index + 1 != self.id as usize {
            parts.push(format!("stream {}", self.id));
        }
        if let Some(language) = self.language.as_deref().filter(|value| *value != "und") {
            parts.push(language.to_owned());
        }
        parts.push(self.codec.clone());
        parts.push(channel_label(self.channels).to_owned());
        if self.sample_rate > 0 {
            parts.push(format!("{:.1} kHz", f64::from(self.sample_rate) / 1000.0));
        }
        parts.join(" · ")
    }
}

fn channel_label(channels: usize) -> &'static str {
    match channels {
        0 => "unknown",
        1 => "mono",
        2 => "stereo",
        6 => "5.1",
        8 => "7.1",
        _ => "multichannel",
    }
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
    /// Zero-based index of the audio track to decode. `None` uses the first.
    pub audio_track: Option<usize>,
    pub audio_bit_depth: AudioBitDepth,
    /// Background used when an image with transparency is saved to a format
    /// without an alpha channel.
    pub image_background: [u8; 3],
    /// Apply the EXIF orientation tag when decoding photos.
    pub image_apply_exif_orientation: bool,
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
            audio_track: None,
            audio_bit_depth: AudioBitDepth::Bit16,
            image_background: Self::DEFAULT_IMAGE_BACKGROUND,
            image_apply_exif_orientation: true,
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
    pub const DEFAULT_IMAGE_BACKGROUND: [u8; 3] = [255, 255, 255];
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
    pub const ALL: [Self; 37] = [
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
        Self::Mp3,
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
            Self::Mp3 => "MP3 audio",
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
            Self::Mp3 => "mp3",
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
            Self::Wav | Self::Flac | Self::Aiff | Self::Au | Self::Opus | Self::Mp3 => {
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
    #[error("the text needs {pages} PDF pages, more than the {max}-page limit")]
    PdfTooManyPages { pages: usize, max: usize },
    #[error("the audio file has no decodable audio track")]
    NoAudioTrack,
    #[error("audio track {requested} does not exist; the file has {available} audio track(s)")]
    AudioTrackNotFound { requested: usize, available: usize },
    #[error("the audio file is too large to convert in memory")]
    AudioTooLarge,
    #[error("conversion cancelled")]
    Cancelled,
    #[error("the archive would contain {count} entries, more than the {max}-entry limit")]
    ArchiveTooManyEntries { count: usize, max: usize },
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
            load_image(input, options)?
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
            | OutputFormat::Mp3
    ) {
        if !is_audio_extension(&extension) {
            return Err(unsupported(&extension, target));
        }
        // Opus only accepts 8/12/16/24/48 kHz. Map the requested rate before
        // the shared audio options resample so it goes straight to the rate the
        // encoder uses instead of resampling twice.
        let audio_options = match target {
            OutputFormat::Opus => ConversionOptions {
                audio_sample_rate: options.audio_sample_rate.map(opus_sample_rate),
                ..options
            },
            OutputFormat::Mp3 => ConversionOptions {
                audio_sample_rate: options.audio_sample_rate.map(mp3_sample_rate),
                ..options
            },
            _ => options,
        };
        let audio = apply_audio_options(
            decode_audio(input, &extension, options.audio_track)?,
            audio_options,
        )?;
        let bytes = match target {
            OutputFormat::Wav => encode_wav(&audio, options.audio_bit_depth)?,
            OutputFormat::Flac => encode_flac(&audio, options.audio_bit_depth)?,
            OutputFormat::Aiff => encode_aiff(&audio)?,
            OutputFormat::Au => encode_au(&audio)?,
            OutputFormat::Opus => encode_opus(audio)?,
            OutputFormat::Mp3 => encode_mp3(audio)?,
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
        if inputs.len() > MAX_ARCHIVE_ENTRIES {
            return Err(ConvertError::ArchiveTooManyEntries {
                count: inputs.len(),
                max: MAX_ARCHIVE_ENTRIES,
            });
        }
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
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes(*pair)
            } else {
                u16::from_be_bytes(*pair)
            }
        })
        .collect::<Vec<_>>();
    String::from_utf16_lossy(&words)
}

fn decode_utf32(input: &[u8], little_endian: bool) -> Result<String, ConvertError> {
    input
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| {
            let bytes = *chunk;
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

fn load_image(
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
fn exif_orientation(input: &[u8]) -> Option<u16> {
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
                let segment = input.get(position + 4..position + 2 + length)?;
                if let Some(tiff) = segment.strip_prefix(b"Exif\0\0") {
                    if let Some(orientation) = tiff_orientation(tiff) {
                        return Some(orientation);
                    }
                }
            }
            position += 2 + length;
        }
        return None;
    }
    if input.starts_with(b"II*\0") || input.starts_with(b"MM\0*") {
        return tiff_orientation(input);
    }
    if input.starts_with(b"\x89PNG\r\n\x1a\n") {
        let mut position = 8;
        while position + 8 <= input.len() {
            let length =
                u32::from_be_bytes(input[position..position + 4].try_into().ok()?) as usize;
            let kind = &input[position + 4..position + 8];
            let body = input.get(position + 8..position + 8 + length)?;
            if kind == b"eXIf" {
                return tiff_orientation(body);
            }
            if kind == b"IEND" {
                break;
            }
            position = position.checked_add(12 + length)?;
        }
    }
    None
}

fn tiff_orientation(tiff: &[u8]) -> Option<u16> {
    let little = tiff.starts_with(b"II");
    if !little && !tiff.starts_with(b"MM") {
        return None;
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
    let ifd = read32(4)? as usize;
    let entries = read16(ifd)? as usize;
    for index in 0..entries {
        let entry = ifd + 2 + index * 12;
        if read16(entry)? == 0x0112 {
            return read16(entry + 8);
        }
    }
    None
}

fn apply_orientation(image: image::DynamicImage, orientation: u16) -> image::DynamicImage {
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

/// Composites transparent pixels onto an opaque background.
fn flatten_image(image: image::DynamicImage, background: [u8; 3]) -> image::DynamicImage {
    if !image.color().has_alpha() {
        return image;
    }
    let rgba = image.to_rgba8();
    let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
    for (pixel, output) in rgba.pixels().zip(rgb.pixels_mut()) {
        let alpha = u16::from(pixel[3]);
        let inverse = 255 - alpha;
        for channel in 0..3 {
            output[channel] = ((u16::from(pixel[channel]) * alpha
                + u16::from(background[channel]) * inverse)
                / 255) as u8;
        }
    }
    image::DynamicImage::ImageRgb8(rgb)
}

fn encode_image(
    image: image::DynamicImage,
    target: OutputFormat,
    options: ConversionOptions,
) -> Result<Vec<u8>, ConvertError> {
    let format = target
        .image_format()
        .expect("encode_image is only called for image targets");

    let image = if matches!(
        target,
        OutputFormat::Jpeg
            | OutputFormat::Bmp
            | OutputFormat::Hdr
            | OutputFormat::Ppm
            | OutputFormat::Pbm
            | OutputFormat::Pgm
    ) {
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

#[derive(Clone, Debug)]
struct DecodedAudio {
    samples: Vec<f32>,
    channels: usize,
    sample_rate: u32,
    /// Precision of the source samples when known. Used to decide whether
    /// reducing to 16-bit output needs dither.
    source_bits: Option<u16>,
}

impl DecodedAudio {
    fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AudioCodecKind {
    Symphonia,
    Opus,
    Ac3,
}

struct AudioTrackEntry {
    info: AudioTrackInfo,
    kind: AudioCodecKind,
    /// Position of the track in the container's track list. MP4 metadata
    /// boxes are indexed by this value, not by the audio-only ordinal.
    track_index: usize,
}

/// Lists the audio tracks in a file so callers can choose one before
/// converting. The first entry is the default track.
pub fn audio_tracks(input: &[u8], extension: &str) -> Result<Vec<AudioTrackInfo>, ConvertError> {
    Ok(audio_track_entries(input, extension)?
        .into_iter()
        .map(|entry| entry.info)
        .collect())
}

/// Lists tracks from a file without loading the whole file. Only container
/// headers and the first packets needed to identify the codec are read.
pub fn audio_tracks_from_path(path: &Path) -> Result<Vec<AudioTrackInfo>, ConvertError> {
    Ok(audio_track_entries_from_path(path)?
        .into_iter()
        .map(|entry| entry.info)
        .collect())
}

fn audio_track_entries_from_path(path: &Path) -> Result<Vec<AudioTrackEntry>, ConvertError> {
    let extension = file_extension(&path.to_string_lossy());
    let mut file = File::open(path)?;
    let mut head = [0_u8; 64 * 1024];
    let read = file.read(&mut head)?;
    let head = &head[..read];

    if extension == "flac" || head.starts_with(b"fLaC") {
        let mut file = File::open(path)?;
        skip_id3v2_file(&mut file)?;
        let reader = claxon::FlacReader::new(file).map_err(|error| {
            ConvertError::Audio(format!("could not open the FLAC stream: {error}"))
        })?;
        let info = reader.streaminfo();
        return Ok(vec![AudioTrackEntry {
            info: AudioTrackInfo {
                index: 0,
                id: 1,
                codec: "FLAC".to_owned(),
                language: None,
                title: None,
                channels: info.channels as usize,
                sample_rate: info.sample_rate,
                default: true,
            },
            kind: AudioCodecKind::Symphonia,
            track_index: 0,
        }]);
    }
    if is_ogg_opus(head) {
        use opus_pure::OggOpusReader;
        let reader = OggOpusReader::new(File::open(path)?).map_err(|error| {
            ConvertError::Audio(format!("could not open the Opus stream: {error}"))
        })?;
        let head = reader.head();
        return Ok(vec![AudioTrackEntry {
            info: AudioTrackInfo {
                index: 0,
                id: 1,
                codec: "Opus".to_owned(),
                language: None,
                title: None,
                channels: head.channel_count as usize,
                sample_rate: 48_000,
                default: true,
            },
            kind: AudioCodecKind::Opus,
            track_index: 0,
        }]);
    }
    if is_raw_ac3(head, &extension) {
        let header = filmcraft_ac3::parse_header(head).map_err(|error| {
            ConvertError::Audio(format!("could not read the AC-3 stream: {error}"))
        })?;
        return Ok(vec![AudioTrackEntry {
            info: AudioTrackInfo {
                index: 0,
                id: 1,
                codec: ac3_codec_label(&header).to_owned(),
                language: None,
                title: None,
                channels: header.channels(),
                sample_rate: header.sample_rate,
                default: true,
            },
            kind: AudioCodecKind::Ac3,
            track_index: 0,
        }]);
    }

    let format = probe_symphonia_file(File::open(path)?, &extension)?;
    let moov = if is_mp4_extension(&extension) {
        read_mp4_moov(path)
    } else {
        None
    };
    audio_track_entries_from_format(format, moov.as_deref())
}

/// Skips an ID3v2 tag at the start of a file, leaving the reader positioned at
/// the first audio byte. Non-ID3 files are left at the start.
fn skip_id3v2_file(file: &mut File) -> Result<(), ConvertError> {
    let mut header = [0_u8; 10];
    if file.read_exact(&mut header).is_err() {
        file.seek(SeekFrom::Start(0))?;
        return Ok(());
    }
    if &header[..3] != b"ID3" {
        file.seek(SeekFrom::Start(0))?;
        return Ok(());
    }
    let size = ((header[6] as u64 & 0x7f) << 21)
        | ((header[7] as u64 & 0x7f) << 14)
        | ((header[8] as u64 & 0x7f) << 7)
        | (header[9] as u64 & 0x7f);
    let footer = if header[5] & 0x10 != 0 { 10 } else { 0 };
    file.seek(SeekFrom::Start(10 + size + footer))?;
    Ok(())
}

fn audio_track_entries(
    input: &[u8],
    extension: &str,
) -> Result<Vec<AudioTrackEntry>, ConvertError> {
    if extension == "flac" || input.starts_with(b"fLaC") {
        let input = strip_id3v2(input);
        let reader = claxon::FlacReader::new(Cursor::new(input)).map_err(|error| {
            ConvertError::Audio(format!("could not open the FLAC stream: {error}"))
        })?;
        let info = reader.streaminfo();
        return Ok(vec![AudioTrackEntry {
            info: AudioTrackInfo {
                index: 0,
                id: 1,
                codec: "FLAC".to_owned(),
                language: None,
                title: None,
                channels: info.channels as usize,
                sample_rate: info.sample_rate,
                default: true,
            },
            kind: AudioCodecKind::Symphonia,
            track_index: 0,
        }]);
    }
    if is_ogg_opus(input) {
        use opus_pure::OggOpusReader;
        let reader = OggOpusReader::new(Cursor::new(input)).map_err(|error| {
            ConvertError::Audio(format!("could not open the Opus stream: {error}"))
        })?;
        let head = reader.head();
        return Ok(vec![AudioTrackEntry {
            info: AudioTrackInfo {
                index: 0,
                id: 1,
                codec: "Opus".to_owned(),
                language: None,
                title: None,
                channels: head.channel_count as usize,
                sample_rate: 48_000,
                default: true,
            },
            kind: AudioCodecKind::Opus,
            track_index: 0,
        }]);
    }
    if is_raw_ac3(input, extension) {
        let header = filmcraft_ac3::parse_header(input).map_err(|error| {
            ConvertError::Audio(format!("could not read the AC-3 stream: {error}"))
        })?;
        return Ok(vec![AudioTrackEntry {
            info: AudioTrackInfo {
                index: 0,
                id: 1,
                codec: ac3_codec_label(&header).to_owned(),
                language: None,
                title: None,
                channels: header.channels(),
                sample_rate: header.sample_rate,
                default: true,
            },
            kind: AudioCodecKind::Ac3,
            track_index: 0,
        }]);
    }
    symphonia_audio_track_entries(input, extension)
}

fn symphonia_audio_track_entries(
    input: &[u8],
    extension: &str,
) -> Result<Vec<AudioTrackEntry>, ConvertError> {
    let format = probe_symphonia(input, extension)?;
    let moov = if is_mp4_container(input) {
        mp4_find(input, b"moov")
    } else {
        None
    };
    audio_track_entries_from_format(format, moov)
}

fn audio_track_entries_from_format(
    mut format: Box<dyn symphonia::core::formats::FormatReader>,
    moov: Option<&[u8]>,
) -> Result<Vec<AudioTrackEntry>, ConvertError> {
    use symphonia::core::codecs::CODEC_TYPE_NULL;
    let tracks: Vec<(
        u32,
        symphonia::core::codecs::CodecParameters,
        Option<String>,
    )> = format
        .tracks()
        .iter()
        .map(|track| (track.id, track.codec_params.clone(), track.language.clone()))
        .collect();

    // Unknown codecs (AC-3/E-AC-3 in MKV and MP4 are reported as NULL) are
    // identified from their first packet. Scanning stops once every track has
    // been seen, so a long file is not read just to list its tracks.
    let mut first_packets: Vec<(u32, Vec<u8>)> = Vec::new();
    for _ in 0..1_024 {
        if first_packets.len() == tracks.len() {
            break;
        }
        match format.next_packet() {
            Ok(packet) => {
                if !first_packets.iter().any(|(id, _)| *id == packet.track_id()) {
                    first_packets.push((
                        packet.track_id(),
                        packet.data[..packet.data.len().min(16)].to_vec(),
                    ));
                }
            }
            Err(_) => break,
        }
    }

    let mut entries = Vec::new();
    for (track_index, (id, codec_params, mut language)) in tracks.into_iter().enumerate() {
        if language.is_none() {
            if let Some(moov) = moov {
                language = mp4_track_language(moov, track_index);
            }
        }
        let first_packet = first_packets
            .iter()
            .find(|(track, _)| *track == id)
            .map(|(_, data)| data.as_slice());
        let known_audio = is_known_audio_codec(codec_params.codec);
        let ac3_header = first_packet
            .filter(|data| data.starts_with(&[0x0B, 0x77]))
            .and_then(|data| filmcraft_ac3::parse_header(data).ok());

        let (kind, codec, channels, sample_rate) =
            if codec_params.codec == symphonia::core::codecs::CODEC_TYPE_OPUS {
                let (channels, _) = opus_stream_info(&codec_params).unwrap_or((0, 0));
                (
                    AudioCodecKind::Opus,
                    "Opus".to_owned(),
                    channels,
                    codec_params.sample_rate.unwrap_or(48_000),
                )
            } else if let Some(header) = ac3_header {
                (
                    AudioCodecKind::Ac3,
                    ac3_codec_label(&header).to_owned(),
                    header.channels(),
                    header.sample_rate,
                )
            } else if known_audio {
                let mut channels = codec_params
                    .channels
                    .map(|channels| channels.count())
                    .unwrap_or(0);
                let mut sample_rate = codec_params.sample_rate.unwrap_or(0);
                if channels == 0 || sample_rate == 0 {
                    if let Some((mp4_channels, mp4_rate)) =
                        moov.and_then(|moov| mp4_track_audio_layout(moov, track_index))
                    {
                        if channels == 0 {
                            channels = mp4_channels;
                        }
                        if sample_rate == 0 {
                            sample_rate = mp4_rate;
                        }
                    }
                }
                (
                    AudioCodecKind::Symphonia,
                    symphonia_codec_label(codec_params.codec).to_owned(),
                    channels,
                    sample_rate,
                )
            } else if codec_params.codec == CODEC_TYPE_NULL
                && (codec_params.sample_rate.is_some() || codec_params.channels.is_some())
            {
                // A container that declares audio metadata but uses a codec
                // Symphonia does not know still gets listed; decoding reports a
                // clear error if it cannot handle the packets.
                (
                    AudioCodecKind::Symphonia,
                    "Unknown".to_owned(),
                    codec_params
                        .channels
                        .map(|channels| channels.count())
                        .unwrap_or(0),
                    codec_params.sample_rate.unwrap_or(0),
                )
            } else {
                continue;
            };

        entries.push(AudioTrackEntry {
            info: AudioTrackInfo {
                index: entries.len(),
                id,
                codec,
                language,
                title: None,
                channels,
                sample_rate,
                default: entries.is_empty(),
            },
            kind,
            track_index,
        });
    }

    if entries.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    Ok(entries)
}

fn is_known_audio_codec(codec: symphonia::core::codecs::CodecType) -> bool {
    use symphonia::core::codecs::*;
    matches!(
        codec,
        CODEC_TYPE_AAC
            | CODEC_TYPE_ALAC
            | CODEC_TYPE_FLAC
            | CODEC_TYPE_MP1
            | CODEC_TYPE_MP2
            | CODEC_TYPE_MP3
            | CODEC_TYPE_OPUS
            | CODEC_TYPE_VORBIS
            | CODEC_TYPE_PCM_S32LE
            | CODEC_TYPE_PCM_S32LE_PLANAR
            | CODEC_TYPE_PCM_S32BE
            | CODEC_TYPE_PCM_S32BE_PLANAR
            | CODEC_TYPE_PCM_S24LE
            | CODEC_TYPE_PCM_S24LE_PLANAR
            | CODEC_TYPE_PCM_S24BE
            | CODEC_TYPE_PCM_S24BE_PLANAR
            | CODEC_TYPE_PCM_S16LE
            | CODEC_TYPE_PCM_S16LE_PLANAR
            | CODEC_TYPE_PCM_S16BE
            | CODEC_TYPE_PCM_S16BE_PLANAR
            | CODEC_TYPE_PCM_S8
            | CODEC_TYPE_PCM_S8_PLANAR
            | CODEC_TYPE_PCM_U32LE
            | CODEC_TYPE_PCM_U32LE_PLANAR
            | CODEC_TYPE_PCM_U32BE
            | CODEC_TYPE_PCM_U32BE_PLANAR
            | CODEC_TYPE_PCM_U24LE
            | CODEC_TYPE_PCM_U24LE_PLANAR
            | CODEC_TYPE_PCM_U24BE
            | CODEC_TYPE_PCM_U24BE_PLANAR
            | CODEC_TYPE_PCM_U16LE
            | CODEC_TYPE_PCM_U16LE_PLANAR
            | CODEC_TYPE_PCM_U16BE
            | CODEC_TYPE_PCM_U16BE_PLANAR
            | CODEC_TYPE_PCM_U8
            | CODEC_TYPE_PCM_U8_PLANAR
            | CODEC_TYPE_PCM_F32LE
            | CODEC_TYPE_PCM_F32LE_PLANAR
            | CODEC_TYPE_PCM_F32BE
            | CODEC_TYPE_PCM_F32BE_PLANAR
            | CODEC_TYPE_PCM_F64LE
            | CODEC_TYPE_PCM_F64LE_PLANAR
            | CODEC_TYPE_PCM_F64BE
            | CODEC_TYPE_PCM_F64BE_PLANAR
            | CODEC_TYPE_PCM_ALAW
            | CODEC_TYPE_PCM_MULAW
            | CODEC_TYPE_ADPCM_G722
            | CODEC_TYPE_ADPCM_G726LE
            | CODEC_TYPE_ADPCM_MS
            | CODEC_TYPE_ADPCM_IMA_WAV
    )
}

fn symphonia_codec_label(codec: symphonia::core::codecs::CodecType) -> &'static str {
    use symphonia::core::codecs::*;
    match codec {
        CODEC_TYPE_AAC => "AAC",
        CODEC_TYPE_ALAC => "ALAC",
        CODEC_TYPE_FLAC => "FLAC",
        CODEC_TYPE_MP1 => "MP1",
        CODEC_TYPE_MP2 => "MP2",
        CODEC_TYPE_MP3 => "MP3",
        CODEC_TYPE_OPUS => "Opus",
        CODEC_TYPE_VORBIS => "Vorbis",
        CODEC_TYPE_PCM_ALAW => "A-law",
        CODEC_TYPE_PCM_MULAW => "µ-law",
        _ => "Audio",
    }
}

fn ac3_codec_label(header: &filmcraft_ac3::Header) -> &'static str {
    if header.is_eac3() {
        "E-AC-3"
    } else {
        "AC-3"
    }
}

fn is_raw_ac3(input: &[u8], extension: &str) -> bool {
    matches!(extension, "ac3" | "eac3" | "ec3") && input.starts_with(&[0x0B, 0x77])
}

fn probe_symphonia(
    input: &[u8],
    extension: &str,
) -> Result<Box<dyn symphonia::core::formats::FormatReader>, ConvertError> {
    probe_symphonia_source(Box::new(Cursor::new(input.to_vec())), extension)
}

fn probe_symphonia_file(
    mut file: File,
    extension: &str,
) -> Result<Box<dyn symphonia::core::formats::FormatReader>, ConvertError> {
    skip_id3v2_file(&mut file)?;
    probe_symphonia_source(Box::new(file), extension)
}

fn probe_symphonia_source(
    source: Box<dyn symphonia::core::io::MediaSource>,
    extension: &str,
) -> Result<Box<dyn symphonia::core::formats::FormatReader>, ConvertError> {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let source = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    hint.with_extension(extension);
    symphonia::default::get_probe()
        .format(
            &hint,
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map(|probed| probed.format)
        .map_err(|error| ConvertError::Audio(format!("could not open the audio stream: {error}")))
}

fn decode_audio(
    input: &[u8],
    extension: &str,
    track: Option<usize>,
) -> Result<DecodedAudio, ConvertError> {
    let entries = audio_track_entries(input, extension)?;
    let index = track.unwrap_or(0);
    let entry = entries.get(index).ok_or(ConvertError::AudioTrackNotFound {
        requested: index + 1,
        available: entries.len(),
    })?;
    let track_id = entry.info.id;
    let track_index = entry.track_index;
    let track_channels = entry.info.channels;
    let kind = entry.kind;

    if extension == "flac" || input.starts_with(b"fLaC") {
        return decode_flac(input);
    }
    if is_ogg_opus(input) {
        return decode_ogg_opus(input);
    }
    if is_raw_ac3(input, extension) {
        return decode_ac3_elementary(input);
    }

    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error as SymphoniaError;

    let mut format = probe_symphonia(input, extension)?;
    let codec_params = format
        .tracks()
        .iter()
        .find(|candidate| candidate.id == track_id)
        .map(|candidate| candidate.codec_params.clone())
        .ok_or(ConvertError::NoAudioTrack)?;

    if kind == AudioCodecKind::Opus {
        return decode_opus_track(
            format.as_mut(),
            track_id,
            &codec_params,
            matroska_discard_padding(input, track_id),
        );
    }
    if kind == AudioCodecKind::Ac3 {
        return decode_ac3_track(format.as_mut(), track_id);
    }

    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(|error| {
            if codec_params.codec == symphonia::core::codecs::CODEC_TYPE_AAC
                && track_channels > 2
            {
                ConvertError::Audio(
                    "multichannel AAC cannot be decoded; choose a stereo AAC track or an AC-3/E-AC-3 track"
                        .to_owned(),
                )
            } else {
                ConvertError::Audio(format!("could not start the audio decoder: {error}"))
            }
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
            .is_none_or(|buffer| buffer.capacity() < capacity)
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

    // MP4 edit lists carry the AAC encoder delay and the intended duration.
    // Symphonia exposes neither, so apply them here for non-Opus tracks.
    if let Some(moov) = mp4_find(input, b"moov") {
        if let Some((skip, total)) = mp4_gapless_info(moov, track_index) {
            let frames = samples.len() / channels;
            let skip = skip.min(frames);
            samples.drain(..skip * channels);
            if let Some(total) = total {
                let available = samples.len() / channels;
                if total < available {
                    samples.truncate(total * channels);
                }
            }
        }
    }
    if samples.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }

    let source_bits = codec_params
        .bits_per_sample
        .and_then(|bits| u16::try_from(bits).ok());
    Ok(DecodedAudio {
        samples,
        channels,
        sample_rate,
        source_bits,
    })
}

fn is_mp4_container(input: &[u8]) -> bool {
    input.get(4..8).is_some_and(|kind| kind == b"ftyp")
}

fn is_mp4_extension(extension: &str) -> bool {
    matches!(extension, "mp4" | "m4a" | "m4v" | "mov")
}

/// Reads just the `moov` box from an MP4 file, seeking over the media data.
/// Metadata such as edit lists and track languages lives in this box.
fn read_mp4_moov(path: &Path) -> Option<Vec<u8>> {
    const MAX_MOOV_BYTES: u64 = 64 * 1024 * 1024;
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let mut position = 0_u64;
    while position.checked_add(8)? <= length {
        file.seek(SeekFrom::Start(position)).ok()?;
        let mut header = [0_u8; 8];
        file.read_exact(&mut header).ok()?;
        let size32 = u32::from_be_bytes(header[..4].try_into().ok()?) as u64;
        let (header_size, size) = if size32 == 1 {
            let mut large = [0_u8; 8];
            file.read_exact(&mut large).ok()?;
            (16, u64::from_be_bytes(large))
        } else if size32 == 0 {
            (8, length - position)
        } else {
            (8, size32)
        };
        if size < header_size || position.checked_add(size)? > length {
            return None;
        }
        if &header[4..8] == b"moov" {
            if size > MAX_MOOV_BYTES {
                return None;
            }
            let mut moov = vec![0_u8; size as usize];
            file.seek(SeekFrom::Start(position)).ok()?;
            file.read_exact(&mut moov).ok()?;
            return Some(moov[header_size as usize..].to_vec());
        }
        position += size;
    }
    None
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
        source_bits: None,
    })
}

fn decode_opus_track(
    format: &mut dyn symphonia::core::formats::FormatReader,
    track_id: u32,
    codec_params: &symphonia::core::codecs::CodecParameters,
    end_padding_ns: Option<u64>,
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

    // Matroska's DiscardPadding trims the encoder's padding from the end of
    // the last block. Symphonia does not expose it, so it comes from the
    // container; round nanoseconds to the nearest 48 kHz sample.
    if let Some(padding_ns) = end_padding_ns {
        let padding = ((u128::from(padding_ns) * 48_000 + 500_000_000) / 1_000_000_000) as usize;
        let padding = (padding * channels).min(samples.len());
        samples.truncate(samples.len() - padding);
    }

    if samples.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    Ok(DecodedAudio {
        samples,
        channels,
        sample_rate: 48_000,
        source_bits: None,
    })
}

/// Decodes an AC-3 or E-AC-3 elementary stream by walking its syncframes.
fn decode_ac3_elementary(input: &[u8]) -> Result<DecodedAudio, ConvertError> {
    let mut decoder = filmcraft_ac3::Decoder::new();
    let mut audio = Ac3Accumulator::default();
    let mut offset = 0;
    while offset + 7 <= input.len() {
        if input[offset..].starts_with(&[0x0B, 0x77]) {
            let header = filmcraft_ac3::parse_header(&input[offset..]).map_err(|error| {
                ConvertError::Audio(format!("could not read the AC-3 stream: {error}"))
            })?;
            if offset + header.frame_bytes > input.len() {
                break;
            }
            audio.push_frame(&mut decoder, &input[offset..offset + header.frame_bytes])?;
            offset += header.frame_bytes;
        } else {
            offset += 1;
        }
    }
    audio.finish()
}

fn decode_ac3_track(
    format: &mut dyn symphonia::core::formats::FormatReader,
    track_id: u32,
) -> Result<DecodedAudio, ConvertError> {
    let mut decoder = filmcraft_ac3::Decoder::new();
    let mut audio = Ac3Accumulator::default();

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
        let mut data: &[u8] = &packet.data;
        while data.len() >= 7 {
            if !data.starts_with(&[0x0B, 0x77]) {
                data = &data[1..];
                continue;
            }
            let header = filmcraft_ac3::parse_header(data).map_err(|error| {
                ConvertError::Audio(format!("could not read the AC-3 stream: {error}"))
            })?;
            if data.len() < header.frame_bytes {
                break;
            }
            audio.push_frame(&mut decoder, &data[..header.frame_bytes])?;
            data = &data[header.frame_bytes..];
        }
    }
    audio.finish()
}

#[derive(Default)]
struct Ac3Accumulator {
    samples: Vec<f32>,
    channels: usize,
    sample_rate: u32,
}

impl Ac3Accumulator {
    fn push_frame(
        &mut self,
        decoder: &mut filmcraft_ac3::Decoder,
        frame: &[u8],
    ) -> Result<(), ConvertError> {
        let decoded = decoder.decode(frame).map_err(|error| {
            ConvertError::Audio(format!("could not decode AC-3 audio: {error}"))
        })?;
        if decoded.channels.is_empty() {
            return Ok(());
        }
        let channels = decoded.channels.len();
        if self.channels != 0 && self.channels != channels {
            return Err(ConvertError::Audio(
                "the AC-3 track changes its channel layout".to_owned(),
            ));
        }
        self.channels = channels;
        self.sample_rate = decoded.header.sample_rate;
        let frames = decoded.channels[0].len();
        self.samples.reserve(frames * channels);
        for index in 0..frames {
            for channel in &decoded.channels {
                self.samples.push(channel[index]);
            }
        }
        if self.samples.len() > MAX_AUDIO_SAMPLES {
            return Err(ConvertError::AudioTooLarge);
        }
        Ok(())
    }

    fn finish(self) -> Result<DecodedAudio, ConvertError> {
        if self.samples.is_empty() || self.channels == 0 || self.sample_rate == 0 {
            return Err(ConvertError::NoAudioTrack);
        }
        Ok(DecodedAudio {
            samples: self.samples,
            channels: self.channels,
            sample_rate: self.sample_rate,
            source_bits: None,
        })
    }
}

const EBML_SEGMENT_ID: u32 = 0x1853_8067;
const EBML_CLUSTER_ID: u32 = 0x1F43_B675;
const EBML_BLOCK_GROUP_ID: u32 = 0xA0;
const EBML_BLOCK_ID: u32 = 0xA1;
const EBML_DISCARD_PADDING_ID: u32 = 0x75A2;

/// Reads an EBML variable-length integer, leaving `pos` after it. IDs keep
/// their length marker; sizes and values do not.
fn read_ebml_vint(
    input: &[u8],
    pos: &mut usize,
    max_length: usize,
    keep_marker: bool,
) -> Option<(u64, usize)> {
    let first = *input.get(*pos)?;
    let length = first.leading_zeros() as usize + 1;
    if length > max_length || *pos + length > input.len() {
        return None;
    }
    let mut value = if keep_marker {
        u64::from(first)
    } else {
        u64::from(first & ((0x80_u8 >> (length - 1)) - 1))
    };
    for byte in &input[*pos + 1..*pos + length] {
        value = (value << 8) | u64::from(*byte);
    }
    *pos += length;
    Some((value, length))
}

/// Reads an EBML element header. `None` in the size slot means the element has
/// an unknown size and extends to the end of its parent.
fn read_ebml_element(input: &[u8], pos: &mut usize) -> Option<(u32, Option<u64>)> {
    let (id, _) = read_ebml_vint(input, pos, 4, true)?;
    let (size, length) = read_ebml_vint(input, pos, 8, false)?;
    let unknown_size = size == (1_u64 << (7 * length)) - 1;
    Some((id as u32, if unknown_size { None } else { Some(size) }))
}

fn skip_ebml_element(input: &[u8], pos: &mut usize, size: Option<u64>) -> Option<()> {
    let end = (*pos as u64).checked_add(size?)?;
    if end > input.len() as u64 {
        return None;
    }
    *pos = end as usize;
    Some(())
}

fn read_ebml_signed(input: &[u8], pos: usize, size: u64) -> Option<i64> {
    if size > 8 {
        return None;
    }
    let bytes = input.get(pos..pos + size as usize)?;
    let mut value = 0_i64;
    for byte in bytes {
        value = value.wrapping_shl(8) | i64::from(*byte);
    }
    if size < 8 && bytes.first().is_some_and(|byte| *byte & 0x80 != 0) {
        value -= 1_i64 << (size * 8);
    }
    Some(value)
}

/// Returns the Matroska `DiscardPadding` of the final block on `track_id`, in
/// nanoseconds. Symphonia's Matroska reader ignores the element, but it is what
/// trims the encoder's padding from the end of an Opus stream.
fn matroska_discard_padding(input: &[u8], track_id: u32) -> Option<u64> {
    if !input.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return None;
    }

    let mut pos = 0;
    let mut visited = 0usize;
    let (segment_size, segment_start) = loop {
        visited += 1;
        if visited > 1_000_000 {
            return None;
        }
        let (id, size) = read_ebml_element(input, &mut pos)?;
        if id == EBML_SEGMENT_ID {
            break (size, pos);
        }
        skip_ebml_element(input, &mut pos, size)?;
    };
    let segment_end = match segment_size {
        Some(size) => ((segment_start as u64).checked_add(size)?).min(input.len() as u64) as usize,
        None => input.len(),
    };

    let mut padding = None;
    while pos < segment_end {
        visited += 1;
        if visited > 1_000_000 {
            return None;
        }
        let (id, size) = read_ebml_element(input, &mut pos)?;
        if id != EBML_CLUSTER_ID {
            skip_ebml_element(input, &mut pos, size)?;
            continue;
        }
        // An unknown-size cluster runs to the end of the segment; scanning it
        // as one block still finds the last BlockGroup.
        let cluster_end = match size {
            Some(size) => ((pos as u64).checked_add(size)?).min(segment_end as u64) as usize,
            None => segment_end,
        };
        while pos < cluster_end {
            visited += 1;
            if visited > 1_000_000 {
                return None;
            }
            let (id, size) = read_ebml_element(input, &mut pos)?;
            if id != EBML_BLOCK_GROUP_ID {
                skip_ebml_element(input, &mut pos, size)?;
                continue;
            }
            let group_end = match size {
                Some(size) => ((pos as u64).checked_add(size)?).min(cluster_end as u64) as usize,
                None => cluster_end,
            };
            let mut block_track = None;
            let mut group_padding = None;
            while pos < group_end {
                visited += 1;
                if visited > 1_000_000 {
                    return None;
                }
                let (id, size) = read_ebml_element(input, &mut pos)?;
                if id == EBML_BLOCK_ID {
                    let mut block_pos = pos;
                    if let Some((number, _)) = read_ebml_vint(input, &mut block_pos, 8, false) {
                        block_track = Some(number);
                    }
                } else if id == EBML_DISCARD_PADDING_ID {
                    if let Some(size) = size {
                        if let Some(value) = read_ebml_signed(input, pos, size) {
                            if value > 0 {
                                group_padding = Some(value as u64);
                            }
                        }
                    }
                }
                skip_ebml_element(input, &mut pos, size)?;
            }
            if block_track == Some(u64::from(track_id)) {
                if let Some(value) = group_padding {
                    padding = Some(value);
                }
            }
            pos = group_end;
        }
        pos = cluster_end;
    }
    padding
}

fn mp4_child_boxes(data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut boxes = Vec::new();
    let mut position = 0;
    while position + 8 <= data.len() {
        let size = u32::from_be_bytes([
            data[position],
            data[position + 1],
            data[position + 2],
            data[position + 3],
        ]) as usize;
        let kind: [u8; 4] = data[position + 4..position + 8]
            .try_into()
            .expect("the box type is four bytes");
        let (header, size) = if size == 1 {
            if position + 16 > data.len() {
                break;
            }
            let large = u64::from_be_bytes(
                data[position + 8..position + 16]
                    .try_into()
                    .expect("a 64-bit box size is eight bytes"),
            ) as usize;
            (16, large)
        } else if size == 0 {
            (8, data.len() - position)
        } else {
            (8, size)
        };
        if size < header || position + size > data.len() {
            break;
        }
        boxes.push((kind, &data[position + header..position + size]));
        position += size;
    }
    boxes
}

fn mp4_find<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    mp4_child_boxes(data)
        .into_iter()
        .find(|(box_kind, _)| box_kind == kind)
        .map(|(_, body)| body)
}

/// Reads the ISO 639-2 language code from an MP4 track's media header.
fn mp4_track_language(moov: &[u8], track_index: usize) -> Option<String> {
    let traks: Vec<&[u8]> = mp4_child_boxes(moov)
        .into_iter()
        .filter(|(kind, _)| kind == b"trak")
        .map(|(_, body)| body)
        .collect();
    let mdhd = mp4_find(mp4_find(traks.get(track_index)?, b"mdia")?, b"mdhd")?;
    let offset = match mdhd.first()? {
        0 => 20,
        1 => 32,
        _ => return None,
    };
    let packed = u16::from_be_bytes(mdhd.get(offset..offset + 2)?.try_into().ok()?);
    if packed == 0 {
        return None;
    }
    let mut language = String::new();
    for shift in [10, 5, 0] {
        let value = ((packed >> shift) & 0x1F) as u8;
        language.push(char::from(value + 0x60));
    }
    Some(language)
}

/// Reads the channel count and sample rate from an MP4 audio sample entry.
/// Symphonia leaves these unset for some AAC tracks until the first frame.
fn mp4_track_audio_layout(moov: &[u8], track_index: usize) -> Option<(usize, u32)> {
    let traks: Vec<&[u8]> = mp4_child_boxes(moov)
        .into_iter()
        .filter(|(kind, _)| kind == b"trak")
        .map(|(_, body)| body)
        .collect();
    let trak = traks.get(track_index)?;
    let stsd = mp4_find(mp4_find(mp4_find(trak, b"mdia")?, b"minf")?, b"stbl")?;
    let stsd = mp4_find(stsd, b"stsd")?;
    let entries = mp4_child_boxes(stsd.get(8..)?);
    let (_, sample_entry) = entries.first()?;
    let channels = u16::from_be_bytes(sample_entry.get(16..18)?.try_into().ok()?) as usize;
    let rate = u32::from_be_bytes(sample_entry.get(24..28)?.try_into().ok()?) >> 16;
    Some((channels, rate))
}

fn mp4_timescale(mvhd_or_mdhd: &[u8]) -> Option<u32> {
    let offset = match mvhd_or_mdhd.first()? {
        0 => 12,
        1 => 20,
        _ => return None,
    };
    Some(u32::from_be_bytes(
        mvhd_or_mdhd.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Returns the AAC priming skip and intended duration from an MP4 edit list
/// (or iTunes' `iTunSMPB` tag), both in media samples.
fn mp4_gapless_info(moov: &[u8], track_index: usize) -> Option<(usize, Option<usize>)> {
    let movie_timescale = mp4_timescale(mp4_find(moov, b"mvhd")?)?;
    let traks: Vec<&[u8]> = mp4_child_boxes(moov)
        .into_iter()
        .filter(|(kind, _)| kind == b"trak")
        .map(|(_, body)| body)
        .collect();
    let trak = traks.get(track_index)?;
    let mdia = mp4_find(trak, b"mdia")?;
    let hdlr = mp4_find(mdia, b"hdlr")?;
    if hdlr.get(8..12) != Some(b"soun") {
        return None;
    }
    let media_timescale = mp4_timescale(mp4_find(mdia, b"mdhd")?)?;

    if let Some(edts) = mp4_find(trak, b"edts") {
        if let Some(elst) = mp4_find(edts, b"elst") {
            let version = *elst.first()?;
            let count = u32::from_be_bytes(elst.get(4..8)?.try_into().ok()?) as usize;
            if count > 0 {
                let (duration, media_time) = if version == 1 {
                    let duration = u64::from_be_bytes(elst.get(8..16)?.try_into().ok()?);
                    let media_time = i64::from_be_bytes(elst.get(16..24)?.try_into().ok()?);
                    (duration, media_time)
                } else {
                    let duration = u64::from(u32::from_be_bytes(elst.get(8..12)?.try_into().ok()?));
                    let media_time =
                        i64::from(i32::from_be_bytes(elst.get(12..16)?.try_into().ok()?));
                    (duration, media_time)
                };
                if media_time >= 0 && media_timescale > 0 {
                    let skip = media_time as usize;
                    let total = (u128::from(duration) * u128::from(media_timescale)
                        / u128::from(movie_timescale)) as usize;
                    return Some((skip, Some(total)));
                }
            }
        }
    }

    // iTunes stores the same numbers as hex text in a freeform tag. It is the
    // fallback for files whose muxer omitted the edit list.
    let tag = b"iTunSMPB";
    let tag_position = moov.windows(tag.len()).position(|window| window == tag)?;
    let data_position = moov[tag_position..]
        .windows(4)
        .take(256)
        .position(|window| window == b"data")?;
    let payload_start = tag_position.checked_add(data_position)?.checked_add(12)?;
    let payload_end = moov
        .get(payload_start..)?
        .iter()
        .position(|byte| *byte == 0)
        .map(|length| payload_start + length)?;
    let text = std::str::from_utf8(moov.get(payload_start..payload_end)?).ok()?;
    let fields: Vec<&str> = text.split_whitespace().collect();
    if fields.len() < 3 {
        return None;
    }
    let skip = usize::from_str_radix(fields[0], 16).ok()?;
    let original = usize::from_str_radix(fields[2], 16).ok()?;
    if original == 0 {
        return None;
    }
    Some((skip, Some(original)))
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
        source_bits: u16::try_from(info.bits_per_sample).ok(),
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

/// Remixes any common channel layout to mono or stereo using the standard
/// ITU-R BS.775 coefficients, so the centre and surround channels are not
/// discarded. The result is scaled down only when the mix would clip.
fn remix_channels(mut audio: DecodedAudio, target: AudioChannels) -> DecodedAudio {
    let target_channels = target.count();
    if audio.channels == target_channels {
        return audio;
    }

    let frames = audio.samples.len() / audio.channels;
    let mut samples = Vec::with_capacity(frames * target_channels);
    for frame in audio.samples.chunks_exact(audio.channels) {
        match target {
            AudioChannels::Mono => samples.push(downmix_mono(frame)),
            AudioChannels::Stereo => {
                if audio.channels == 1 {
                    samples.extend_from_slice(&[frame[0], frame[0]]);
                } else {
                    let (left, right) = downmix_stereo(frame);
                    samples.extend_from_slice(&[left, right]);
                }
            }
        }
    }

    // A 5.1 downmix can exceed full scale even when every source channel is
    // within it. Attenuate the whole mix rather than clipping individual peaks.
    let peak = samples
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    if peak > 1.0 {
        let scale = 1.0 / peak;
        for sample in &mut samples {
            *sample *= scale;
        }
    }

    audio.samples = samples;
    audio.channels = target_channels;
    audio
}

fn downmix_mono(frame: &[f32]) -> f32 {
    let channel = |index: usize| frame.get(index).copied().unwrap_or(0.0);
    match frame.len() {
        0 => 0.0,
        1 => frame[0],
        2 => 0.5 * (channel(0) + channel(1)),
        3 => 0.5 * (channel(0) + channel(1)) + 0.707 * channel(2),
        4 => 0.5 * (channel(0) + channel(1)) + 0.5 * (channel(2) + channel(3)),
        5 => 0.5 * (channel(0) + channel(1)) + 0.707 * channel(2) + 0.5 * (channel(3) + channel(4)),
        6 => {
            0.5 * (channel(0) + channel(1))
                + 0.707 * channel(2)
                + 0.5 * channel(3)
                + 0.5 * (channel(4) + channel(5))
        }
        7 => {
            0.5 * (channel(0) + channel(1))
                + 0.707 * channel(2)
                + 0.5 * channel(3)
                + 0.5 * (channel(4) + channel(5) + channel(6))
        }
        8 => {
            0.5 * (channel(0) + channel(1))
                + 0.707 * channel(2)
                + 0.5 * channel(3)
                + 0.5 * (channel(4) + channel(5) + channel(6) + channel(7))
        }
        channels => frame.iter().sum::<f32>() / channels as f32,
    }
}

/// Standard film-layout channel order: L, R, C, LFE, back L/R, side L/R.
fn downmix_stereo(frame: &[f32]) -> (f32, f32) {
    let channel = |index: usize| frame.get(index).copied().unwrap_or(0.0);
    let mut left = channel(0);
    let mut right = channel(1);
    match frame.len() {
        0 => return (0.0, 0.0),
        1 => return (frame[0], frame[0]),
        2 => return (left, right),
        3 => {
            left += 0.707 * channel(2);
            right += 0.707 * channel(2);
        }
        4 => {
            left += 0.707 * channel(2);
            right += 0.707 * channel(3);
        }
        5 => {
            left += 0.707 * channel(2) + 0.707 * channel(3);
            right += 0.707 * channel(2) + 0.707 * channel(4);
        }
        6 => {
            left += 0.707 * channel(2) + 0.5 * channel(3) + 0.707 * channel(4);
            right += 0.707 * channel(2) + 0.5 * channel(3) + 0.707 * channel(5);
        }
        7 => {
            left += 0.707 * channel(2) + 0.5 * channel(3) + 0.5 * channel(4) + 0.707 * channel(5);
            right += 0.707 * channel(2) + 0.5 * channel(3) + 0.5 * channel(4) + 0.707 * channel(6);
        }
        _ => {
            left += 0.707 * channel(2) + 0.5 * channel(3) + 0.707 * channel(4) + 0.707 * channel(6);
            right +=
                0.707 * channel(2) + 0.5 * channel(3) + 0.707 * channel(5) + 0.707 * channel(7);
        }
    }
    (left, right)
}

const RESAMPLE_ZERO_CROSSINGS: usize = 16;
const RESAMPLE_PHASES: usize = 256;

/// Band-limited resampling with a windowed-sinc low-pass filter. The filter
/// cutoff follows the lower of the two rates, so downsampling does not alias
/// high frequencies back into the audible band.
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

    let ratio = f64::from(target_rate) / f64::from(audio.sample_rate);
    let cutoff = ratio.min(1.0) * 0.95;
    let table = resample_kernel_table(cutoff);
    let taps = RESAMPLE_ZERO_CROSSINGS * 2;
    let mut samples = Vec::with_capacity(sample_count);
    let mut mixed = vec![0.0_f64; channels];

    for frame in 0..target_frames {
        let position = frame as f64 / ratio;
        let base = position.floor();
        let phase =
            (((position - base) * RESAMPLE_PHASES as f64) as usize).min(RESAMPLE_PHASES - 1);
        let weights = &table[phase * taps..phase * taps + taps];
        mixed.fill(0.0);
        for (tap, weight) in weights.iter().enumerate() {
            let index = base as isize + tap as isize - (RESAMPLE_ZERO_CROSSINGS as isize - 1);
            let index = index.clamp(0, source_frames as isize - 1) as usize;
            let source = &audio.samples[index * channels..(index + 1) * channels];
            for channel in 0..channels {
                mixed[channel] += f64::from(source[channel]) * weight;
            }
        }
        for value in &mixed {
            samples.push(*value as f32);
        }
    }

    Ok(DecodedAudio {
        samples,
        channels,
        sample_rate: target_rate,
        source_bits: audio.source_bits,
    })
}

fn resample_kernel_table(cutoff: f64) -> Vec<f64> {
    let taps = RESAMPLE_ZERO_CROSSINGS * 2;
    let mut table = vec![0.0; RESAMPLE_PHASES * taps];
    for phase in 0..RESAMPLE_PHASES {
        let fraction = phase as f64 / RESAMPLE_PHASES as f64;
        let mut sum = 0.0;
        for tap in 0..taps {
            let distance = tap as f64 - (RESAMPLE_ZERO_CROSSINGS as f64 - 1.0) - fraction;
            let normalized = distance / RESAMPLE_ZERO_CROSSINGS as f64;
            let window = if normalized.abs() <= 1.0 {
                0.42 + 0.5 * (std::f64::consts::PI * normalized).cos()
                    + 0.08 * (2.0 * std::f64::consts::PI * normalized).cos()
            } else {
                0.0
            };
            let argument = std::f64::consts::PI * cutoff * distance;
            let sinc = if argument.abs() < 1e-9 {
                1.0
            } else {
                argument.sin() / argument
            };
            let weight = cutoff * sinc * window;
            table[phase * taps + tap] = weight;
            sum += weight;
        }
        if sum.abs() > 1e-12 {
            for tap in 0..taps {
                table[phase * taps + tap] /= sum;
            }
        }
    }
    table
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

/// Converts an audio input and writes WAV output directly to `writer`,
/// avoiding a second full-size copy of the encoded file in memory.
pub fn convert_audio_to_wav_writer<W: Write>(
    input_name: &str,
    input: &[u8],
    options: ConversionOptions,
    writer: &mut W,
) -> Result<(), ConvertError> {
    let extension = file_extension(input_name);
    if !is_audio_extension(&extension) {
        return Err(unsupported(&extension, OutputFormat::Wav));
    }
    let audio = apply_audio_options(
        decode_audio(input, &extension, options.audio_track)?,
        options,
    )?;
    write_wav(writer, &audio, options.audio_bit_depth)
}

/// The output name used by [`convert_audio_to_wav_writer`].
pub fn wav_output_name(input_name: &str) -> PathBuf {
    output_name(input_name, "wav")
}

/// The output name for a streaming audio conversion.
pub fn audio_output_name(input_name: &str, target: OutputFormat) -> PathBuf {
    output_name(input_name, target.extension())
}

fn encode_wav(audio: &DecodedAudio, bit_depth: AudioBitDepth) -> Result<Vec<u8>, ConvertError> {
    let mut output = Vec::new();
    write_wav(&mut output, audio, bit_depth)?;
    Ok(output)
}

fn write_wav<W: Write>(
    writer: &mut W,
    audio: &DecodedAudio,
    bit_depth: AudioBitDepth,
) -> Result<(), ConvertError> {
    if audio.channels == 0 {
        return Err(ConvertError::NoAudioTrack);
    }
    let usable_samples = audio.samples.len() - audio.samples.len() % audio.channels;
    let samples = &audio.samples[..usable_samples];
    let frames = samples.len() / audio.channels;
    let channels = u16::try_from(audio.channels).map_err(|_| ConvertError::AudioTooLarge)?;
    let bytes_per_sample = match bit_depth {
        AudioBitDepth::Bit16 => 2_u16,
        AudioBitDepth::Bit24 => 3,
        AudioBitDepth::Float32 => 4,
    };
    let block_align = channels
        .checked_mul(bytes_per_sample)
        .ok_or(ConvertError::AudioTooLarge)?;
    let byte_rate = audio
        .sample_rate
        .checked_mul(u32::from(block_align))
        .ok_or(ConvertError::AudioTooLarge)?;
    let data_size = u32::try_from(
        samples
            .len()
            .checked_mul(bytes_per_sample as usize)
            .ok_or(ConvertError::AudioTooLarge)?,
    )
    .map_err(|_| ConvertError::AudioTooLarge)?;
    let is_float = bit_depth == AudioBitDepth::Float32;
    let fact_size = if is_float { 12 } else { 0 };
    let riff_size = 36_u32
        .checked_add(fact_size)
        .and_then(|size| size.checked_add(data_size))
        .ok_or(ConvertError::AudioTooLarge)?;

    let mut output = Vec::with_capacity(44 + fact_size as usize);
    output.extend_from_slice(b"RIFF");
    output.extend_from_slice(&riff_size.to_le_bytes());
    output.extend_from_slice(b"WAVEfmt ");
    output.extend_from_slice(&16_u32.to_le_bytes());
    output.extend_from_slice(&if is_float { 3_u16 } else { 1 }.to_le_bytes());
    output.extend_from_slice(&channels.to_le_bytes());
    output.extend_from_slice(&audio.sample_rate.to_le_bytes());
    output.extend_from_slice(&byte_rate.to_le_bytes());
    output.extend_from_slice(&block_align.to_le_bytes());
    output.extend_from_slice(&bit_depth.bits().to_le_bytes());
    if is_float {
        output.extend_from_slice(b"fact");
        output.extend_from_slice(&4_u32.to_le_bytes());
        output.extend_from_slice(&(frames as u32).to_le_bytes());
    }
    output.extend_from_slice(b"data");
    output.extend_from_slice(&data_size.to_le_bytes());
    writer.write_all(&output)?;

    let mut dither = Dither::new();
    let mut samples_buffer = Vec::with_capacity(64 * 1024);
    for sample in samples {
        match bit_depth {
            AudioBitDepth::Bit16 => samples_buffer.extend_from_slice(
                &dither
                    .quantize_i16(*sample, audio.source_bits)
                    .to_le_bytes(),
            ),
            AudioBitDepth::Bit24 => {
                let value = dither.quantize_i32(*sample, 24, audio.source_bits);
                samples_buffer.extend_from_slice(&value.to_le_bytes()[..3]);
            }
            AudioBitDepth::Float32 => samples_buffer.extend_from_slice(&sample.to_le_bytes()),
        }
        if samples_buffer.len() >= 32 * 1024 {
            writer.write_all(&samples_buffer)?;
            samples_buffer.clear();
        }
    }
    writer.write_all(&samples_buffer)?;
    Ok(())
}

/// Triangular-PDF dither for reductions to a lower integer bit depth. The
/// generator is deterministic so conversions are reproducible.
struct Dither {
    state: u32,
}

impl Dither {
    fn new() -> Self {
        Self { state: 0x1234_5678 }
    }

    fn next(&mut self) -> f32 {
        self.state = self
            .state
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        let first = (self.state >> 8) as f32 / 16_777_216.0;
        self.state = self
            .state
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        let second = (self.state >> 8) as f32 / 16_777_216.0;
        first - second
    }

    fn quantize_i16(&mut self, sample: f32, source_bits: Option<u16>) -> i16 {
        if source_bits.is_some_and(|bits| bits > 16) {
            let dither = self.next() / f32::from(i16::MAX);
            sample_to_i16((sample + dither).clamp(-1.0, 1.0))
        } else {
            sample_to_i16(sample)
        }
    }

    fn quantize_i32(&mut self, sample: f32, bits: u32, source_bits: Option<u16>) -> i32 {
        let scale = ((1_i64 << (bits - 1)) - 1) as f32;
        let dither = if source_bits.is_some_and(|source| u32::from(source) > bits) {
            self.next() / scale
        } else {
            0.0
        };
        ((sample + dither).clamp(-1.0, 1.0) * scale).round() as i32
    }
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
    let frames = (total + flush as usize).div_ceil(frame_size);
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

fn mp3_sample_rate(sample_rate: u32) -> u32 {
    match sample_rate {
        0..=8_000 => 8_000,
        8_001..=11_025 => 11_025,
        11_026..=12_000 => 12_000,
        12_001..=16_000 => 16_000,
        16_001..=22_050 => 22_050,
        22_051..=24_000 => 24_000,
        24_001..=32_000 => 32_000,
        32_001..=44_100 => 44_100,
        _ => 48_000,
    }
}

fn encode_mp3(audio: DecodedAudio) -> Result<Vec<u8>, ConvertError> {
    use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, MonoPcm, Quality};

    if audio.channels == 0 || audio.sample_rate == 0 || audio.samples.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    let mut audio = if audio.channels > 2 {
        remix_channels(audio, AudioChannels::Stereo)
    } else {
        audio
    };
    let sample_rate = mp3_sample_rate(audio.sample_rate);
    if audio.sample_rate != sample_rate {
        audio = resample_audio(audio, sample_rate)?;
    }
    let channels = audio.channels;

    let mut builder = Builder::new()
        .ok_or_else(|| ConvertError::Audio("could not start the MP3 encoder".to_owned()))?;
    builder
        .set_num_channels(channels as u8)
        .and_then(|_| builder.set_sample_rate(sample_rate))
        .and_then(|_| {
            builder.set_brate(if channels == 1 {
                Bitrate::Kbps96
            } else {
                Bitrate::Kbps192
            })
        })
        .and_then(|_| builder.set_quality(Quality::NearBest))
        .map_err(|error| {
            ConvertError::Audio(format!("could not configure the MP3 encoder: {error}"))
        })?;
    let mut encoder = builder.build().map_err(|error| {
        ConvertError::Audio(format!("could not start the MP3 encoder: {error}"))
    })?;

    let mut output = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(audio.frames()));
    if channels == 1 {
        encoder
            .encode_to_vec(MonoPcm(&audio.samples), &mut output)
            .map_err(|error| ConvertError::Audio(format!("could not encode MP3 audio: {error}")))?;
    } else {
        encoder
            .encode_to_vec(InterleavedPcm(&audio.samples), &mut output)
            .map_err(|error| ConvertError::Audio(format!("could not encode MP3 audio: {error}")))?;
    }
    encoder
        .flush_to_vec::<FlushNoGap>(&mut output)
        .map_err(|error| {
            ConvertError::Audio(format!("could not finish the MP3 stream: {error}"))
        })?;
    Ok(output)
}

fn encode_flac(audio: &DecodedAudio, bit_depth: AudioBitDepth) -> Result<Vec<u8>, ConvertError> {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;

    let bits = match bit_depth {
        AudioBitDepth::Bit16 => 16,
        AudioBitDepth::Bit24 => 24,
        AudioBitDepth::Float32 => {
            return Err(ConvertError::Audio(
                "FLAC cannot store 32-bit float samples; choose 16-bit or 24-bit".to_owned(),
            ))
        }
    };
    let usable_samples = audio.samples.len() - audio.samples.len() % audio.channels;
    let mut dither = Dither::new();
    let integer_samples: Vec<i32> = audio.samples[..usable_samples]
        .iter()
        .map(|sample| {
            if bits == 16 {
                i32::from(dither.quantize_i16(*sample, audio.source_bits))
            } else {
                dither.quantize_i32(*sample, 24, audio.source_bits)
            }
        })
        .collect();

    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, error)| ConvertError::Audio(error.to_string()))?;
    let source = flacenc::source::MemSource::from_samples(
        &integer_samples,
        audio.channels,
        bits as usize,
        audio.sample_rate as usize,
    );
    let mut stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|error| ConvertError::Audio(error.to_string()))?;
    // The encoder reports the shortest block (the final partial block) as the
    // minimum, but writes fixed-block frame numbers. Readers such as Symphonia
    // treat that as a variable-block stream and reject the frames, so describe
    // the stream as fixed-size. A shorter final block is still valid FLAC.
    stream
        .stream_info_mut()
        .set_block_sizes(config.block_size, config.block_size)
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
    let orientation = exif_orientation(input).unwrap_or(1);
    if orientation == 1 {
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
    }

    let image = if extension == "svg" {
        rasterize_svg(input)?
    } else {
        load_image(input, options)?
    };
    let width = image.width();
    let height = image.height();
    let compressed = deflate_bytes(&flatten_to_rgb(&image, options.image_background))?;
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

fn flatten_to_rgb(image: &image::DynamicImage, background: [u8; 3]) -> Vec<u8> {
    if !image.color().has_alpha() {
        return image.to_rgb8().into_raw();
    }
    let rgba = image.to_rgba8();
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for pixel in rgba.pixels() {
        let alpha = u16::from(pixel[3]);
        let inverse = 255 - alpha;
        for (channel, background) in pixel.0[..3].iter().zip(background) {
            let value = (u16::from(*channel) * alpha + u16::from(background) * inverse + 127) / 255;
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
    if page_count > MAX_PDF_PAGES {
        return Err(ConvertError::PdfTooManyPages {
            pages: page_count,
            max: MAX_PDF_PAGES,
        });
    }

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
    fn rejects_archives_with_too_many_entries() {
        let inputs: Vec<(PathBuf, Vec<u8>)> = (0..=MAX_ARCHIVE_ENTRIES)
            .map(|index| (PathBuf::from(format!("{index}.txt")), Vec::new()))
            .collect();
        assert!(matches!(
            convert_batch(&inputs, OutputFormat::Zip),
            Err(ConvertError::ArchiveTooManyEntries { max, .. }) if max == MAX_ARCHIVE_ENTRIES
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
        wav_fixture_at(8_000, 8_000)
    }

    fn wav_fixture_at(sample_rate: u32, frames: u32) -> Vec<u8> {
        let channels = 2_u16;
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
                    body.as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| i16::from_le_bytes(*pair))
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
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| i16::from_be_bytes(*bytes))
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

        // Default options keep the source rate, so this exercises the Opus
        // encoder's own 44.1 kHz -> 48 kHz fallback instead of the shared
        // resampler.
        let wav_44_1 = wav_fixture_at(44_100, 44_100);
        let source_44_1 = parse_wav(&wav_44_1);
        assert_eq!(source_44_1.sample_rate, 44_100);
        assert_eq!(source_44_1.samples.len(), 88_200);
        let fallback = convert_bytes("tone-44.1k.wav", &wav_44_1, OutputFormat::Opus).unwrap();
        let fallback_back =
            convert_bytes("tone.opus", &fallback[0].bytes, OutputFormat::Wav).unwrap();
        let fallback_decoded = parse_wav(&fallback_back[0].bytes);
        assert_eq!(fallback_decoded.sample_rate, 48_000);
        assert_eq!(fallback_decoded.channels, 2);
        assert_eq!(fallback_decoded.samples.len(), 96_000);
        assert_audible(&fallback_decoded.samples);
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

        let tracks = super::audio_tracks(&input, "mp4").unwrap();
        assert_eq!(tracks.len(), 1, "the video track must not be listed");
        assert_eq!(tracks[0].codec, "AAC");
        assert_eq!(tracks[0].channels, 1);

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
            if name == "sample-vp9-opus.webm" {
                // The source WebM carries the Opus end trim, so its audio is
                // exactly 2 s long.
                assert_eq!(parsed.samples.len(), 96_000, "{name}");
            } else {
                // The MP4 and remuxed WebM do not carry the end trim, so their
                // decoded audio keeps the final packet's padding.
                assert!(
                    (parsed.samples.len() as i64 - 96_000).abs() <= 1_000,
                    "{name} decoded {} samples",
                    parsed.samples.len()
                );
            }
            assert_audible(&parsed.samples);
        }
    }

    #[test]
    fn converts_vp9_opus_webm_directly_to_opus_and_back() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sample-vp9-opus.webm");
        let input = std::fs::read(path).unwrap();

        let opus = convert_bytes("sample-vp9-opus.webm", &input, OutputFormat::Opus).unwrap();
        assert_eq!(opus[0].name, PathBuf::from("sample-vp9-opus.opus"));
        assert!(opus[0].bytes.starts_with(b"OggS"));

        let back =
            convert_bytes("sample-vp9-opus.opus", &opus[0].bytes, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&back[0].bytes);
        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(decoded.channels, 1);
        // The fixture holds exactly 2 s of audio, and the WebM end trim is
        // applied, so the round trip must reproduce the exact sample count.
        assert_eq!(decoded.samples.len(), 96_000);
        assert_audible(&decoded.samples);
    }

    #[test]
    fn trims_discard_padding_for_the_selected_audio_track() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/sample-two-opus-tracks.webm");
        let input = std::fs::read(path).unwrap();

        // The tracks hold 1 s and 1.01 s of audio and carry different end
        // padding, so using the wrong track's value changes the decoded length.
        assert_eq!(super::matroska_discard_padding(&input, 1), Some(13_500_000));
        assert_eq!(super::matroska_discard_padding(&input, 2), Some(3_500_000));

        let wav = convert_bytes("sample-two-opus-tracks.webm", &input, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&wav[0].bytes);
        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.samples.len(), 48_000);
        assert_audible(&decoded.samples);
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

    fn fixture_path(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name)
    }

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(fixture_path(name)).unwrap()
    }

    /// Returns (format tag, channels, sample rate, bits, data offset, data size).
    fn wav_layout(bytes: &[u8]) -> (u16, u16, u32, u16, usize, usize) {
        assert!(bytes.len() >= 12);
        let mut offset = 12;
        let mut format: (u16, u16, u32, u16) = (0, 0, 0, 0);
        let mut data: (usize, usize) = (0, 0);
        while offset + 8 <= bytes.len() {
            let id = &bytes[offset..offset + 4];
            let size =
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let body = offset + 8;
            if id == b"fmt " {
                format = (
                    u16::from_le_bytes(bytes[body..body + 2].try_into().unwrap()),
                    u16::from_le_bytes(bytes[body + 2..body + 4].try_into().unwrap()),
                    u32::from_le_bytes(bytes[body + 4..body + 8].try_into().unwrap()),
                    u16::from_le_bytes(bytes[body + 14..body + 16].try_into().unwrap()),
                );
            } else if id == b"data" {
                data = (body, size);
            }
            offset = body + size + (size & 1);
        }
        (format.0, format.1, format.2, format.3, data.0, data.1)
    }

    #[test]
    fn writes_wav_directly_to_a_writer() {
        let input = wav_fixture();
        let mut output = Vec::new();
        convert_audio_to_wav_writer(
            "tone.wav",
            &input,
            ConversionOptions::default(),
            &mut output,
        )
        .unwrap();
        let parsed = parse_wav(&output);
        assert_eq!(parsed.bits_per_sample, 16);
        assert_eq!(parsed.samples.len(), 16_000);
        assert_audible(&parsed.samples);
        assert_eq!(
            wav_output_name("tone.wav"),
            std::path::PathBuf::from("tone.wav")
        );
    }

    fn stream_temp_dir(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("convert2it-stream-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn stream_wav(input: &Path, options: ConversionOptions) -> Vec<u8> {
        let mut output = std::io::Cursor::new(Vec::new());
        super::convert_audio_file_to_writer(
            input,
            &input.file_name().unwrap().to_string_lossy(),
            OutputFormat::Wav,
            options,
            &mut output,
        )
        .unwrap();
        output.into_inner()
    }

    #[test]
    fn streaming_cancellation_stops_the_conversion() {
        let directory = stream_temp_dir("cancel");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let mut output = std::io::Cursor::new(Vec::new());
        let error = super::convert_audio_file_to_writer_with_cancel(
            &input,
            "tone.wav",
            OutputFormat::Wav,
            ConversionOptions::default(),
            &mut output,
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(error, ConvertError::Cancelled));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_wav_to_wav_from_a_file() {
        let directory = stream_temp_dir("wav-wav");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let output = stream_wav(&input, ConversionOptions::default());
        let parsed = parse_wav(&output);
        assert_eq!(parsed.samples.len(), 16_000);
        assert_audible(&parsed.samples);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_wav_to_flac_from_a_file() {
        let directory = stream_temp_dir("wav-flac");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let mut output = std::io::Cursor::new(Vec::new());
        super::convert_audio_file_to_writer(
            &input,
            "tone.wav",
            OutputFormat::Flac,
            ConversionOptions::default(),
            &mut output,
        )
        .unwrap();
        let mut reader =
            claxon::FlacReader::new(std::io::Cursor::new(output.into_inner())).unwrap();
        let samples: Vec<i32> = reader.samples().map(Result::unwrap).collect();
        assert_eq!(samples.len(), 16_000);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_wav_to_opus_from_a_file() {
        let directory = stream_temp_dir("wav-opus");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let mut output = std::io::Cursor::new(Vec::new());
        super::convert_audio_file_to_writer(
            &input,
            "tone.wav",
            OutputFormat::Opus,
            ConversionOptions::default(),
            &mut output,
        )
        .unwrap();
        let bytes = output.into_inner();
        assert!(bytes.starts_with(b"OggS"));

        let mut reader = opus_pure::OggOpusReader::new(std::io::Cursor::new(bytes)).unwrap();
        let head = reader.head().clone();
        let channels = head.channel_count as usize;
        let mut decoder = head.decoder(48_000).unwrap();
        let mut trim = opus_pure::Trim::new(&head, 48_000, channels).unwrap();
        let mut block = vec![0.0; opus_pure::MAX_PACKET_SAMPLES * channels];
        let mut frames = 0;
        for packet in reader.packets() {
            let packet = packet.unwrap();
            let decoded = decoder
                .decode(&packet.data, opus_pure::MAX_PACKET_SAMPLES, &mut block)
                .unwrap();
            frames += trim.keep(&packet, &block[..decoded * channels]).len() / channels;
        }
        assert_eq!(frames, 48_000);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_wav_to_aiff_and_au_from_a_file() {
        let directory = stream_temp_dir("wav-aiff-au");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        for target in [OutputFormat::Aiff, OutputFormat::Au] {
            let mut output = std::io::Cursor::new(Vec::new());
            super::convert_audio_file_to_writer(
                &input,
                "tone.wav",
                target,
                ConversionOptions::default(),
                &mut output,
            )
            .unwrap();
            let bytes = output.into_inner();
            if target == OutputFormat::Aiff {
                assert_eq!(&bytes[..4], b"FORM");
                assert_eq!(
                    u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize + 8,
                    bytes.len()
                );
                assert_eq!(u32::from_be_bytes(bytes[22..26].try_into().unwrap()), 8_000);
                assert_eq!(
                    u32::from_be_bytes(bytes[42..46].try_into().unwrap()) as usize + 46,
                    bytes.len()
                );
            } else {
                assert_eq!(&bytes[..4], b".snd");
                assert_eq!(
                    u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize + 24,
                    bytes.len()
                );
            }
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_24_bit_flac_from_a_file() {
        let directory = stream_temp_dir("wav-flac-24");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let mut output = std::io::Cursor::new(Vec::new());
        super::convert_audio_file_to_writer(
            &input,
            "tone.wav",
            OutputFormat::Flac,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Bit24,
                ..Default::default()
            },
            &mut output,
        )
        .unwrap();
        let mut reader =
            claxon::FlacReader::new(std::io::Cursor::new(output.into_inner())).unwrap();
        assert_eq!(reader.streaminfo().bits_per_sample, 24);
        assert_eq!(reader.streaminfo().samples, Some(8_000));
        let samples: Vec<i32> = reader.samples().map(Result::unwrap).collect();
        assert_eq!(samples.len(), 16_000);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_wav_to_mp3_from_a_file() {
        let directory = stream_temp_dir("wav-mp3");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let mut output = std::io::Cursor::new(Vec::new());
        super::convert_audio_file_to_writer(
            &input,
            "tone.wav",
            OutputFormat::Mp3,
            ConversionOptions::default(),
            &mut output,
        )
        .unwrap();
        let bytes = output.into_inner();
        assert!(bytes.len() > 100);
        assert!(bytes.starts_with(b"ID3") || bytes[0] == 0xFF);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_resampled_wav_from_a_file() {
        let directory = stream_temp_dir("wav-resample");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let output = stream_wav(
            &input,
            ConversionOptions {
                audio_sample_rate: Some(44_100),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&output);
        assert_eq!(parsed.sample_rate, 44_100);
        assert_eq!(parsed.samples.len(), 88_200);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_flac_and_mp3_input_files() {
        let directory = stream_temp_dir("encoded-input");
        let source = wav_fixture();
        for (name, format) in [
            ("tone.flac", OutputFormat::Flac),
            ("tone.mp3", OutputFormat::Mp3),
        ] {
            let encoded = super::convert_bytes_with_options(
                "tone.wav",
                &source,
                format,
                ConversionOptions::default(),
            )
            .unwrap()
            .remove(0)
            .bytes;
            let input = directory.join(name);
            std::fs::write(&input, encoded).unwrap();
            let output = stream_wav(&input, ConversionOptions::default());
            let parsed = parse_wav(&output);
            assert_eq!(parsed.sample_rate, 8_000);
            if name == "tone.flac" {
                assert_eq!(parsed.samples.len(), 16_000);
                assert_eq!(parsed.samples, parse_wav(&source).samples);
            } else {
                assert!(parsed.samples.len() >= 15_000);
                assert!(parsed.samples.len() <= 17_000);
            }
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_raw_and_matroska_ac3_tracks() {
        for (name, expected_channels) in [("sample-ac3.ac3", 6), ("sample-eac3.mkv", 6)] {
            let input = std::path::Path::new("testdata").join(name);
            let output = stream_wav(&input, ConversionOptions::default());
            let parsed = parse_wav(&output);
            assert_eq!(parsed.channels, expected_channels);
            assert_eq!(
                parsed.samples.len(),
                96_768 * usize::from(expected_channels)
            );
            assert_audible(&parsed.samples);
        }
    }

    #[test]
    fn streams_short_and_downsampled_audio() {
        let directory = stream_temp_dir("resample-edges");
        for (source_rate, frames, target_rate, expected_frames) in [
            (44_100, 44_100, 8_000, 8_000),
            (48_000, 48_000, 44_100, 44_100),
            (8_000, 1, 44_100, 6),
            (8_000, 2, 48_000, 12),
        ] {
            let input = directory.join(format!("tone-{source_rate}-{frames}.wav"));
            std::fs::write(&input, wav_fixture_at(source_rate, frames)).unwrap();
            let output = stream_wav(
                &input,
                ConversionOptions {
                    audio_sample_rate: Some(target_rate),
                    ..Default::default()
                },
            );
            let parsed = parse_wav(&output);
            assert_eq!(parsed.sample_rate, target_rate);
            assert_eq!(parsed.samples.len(), expected_frames * 2);
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_a_selected_opus_track_from_a_file() {
        let output = stream_wav(
            &fixture_path("sample-two-opus-tracks.webm"),
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&output);
        assert_eq!(parsed.samples.len(), 48_480);
        assert_audible(&parsed.samples);

        let resampled = stream_wav(
            &fixture_path("sample-two-opus-tracks.webm"),
            ConversionOptions {
                audio_track: Some(1),
                audio_sample_rate: Some(44_100),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&resampled);
        let expected_frames = (48_480.0_f64 * 44_100.0 / 48_000.0).round() as usize;
        assert_eq!(parsed.samples.len(), expected_frames);
        assert_audible(&parsed.samples);
    }

    #[test]
    fn streams_mp4_gapless_audio_from_a_file() {
        let output = stream_wav(
            &fixture_path("sample-aac.m4a"),
            ConversionOptions::default(),
        );
        let parsed = parse_wav(&output);
        assert_eq!(parsed.samples.len(), 88_200);
        assert_audible(&parsed.samples);

        let resampled = stream_wav(
            &fixture_path("sample-aac.m4a"),
            ConversionOptions {
                audio_sample_rate: Some(48_000),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&resampled);
        let expected_frames = (88_200.0_f64 * 48_000.0 / 44_100.0).round() as usize;
        assert_eq!(
            parsed.samples.len(),
            expected_frames * usize::from(parsed.channels)
        );
        assert_audible(&parsed.samples);
    }

    #[test]
    fn streams_ac3_to_stereo_without_clipping() {
        let output = stream_wav(
            &fixture_path("sample-ac3.mkv"),
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&output);
        assert_eq!(parsed.samples.len(), 96_768 * 2);
        assert_audible(&parsed.samples);
        let saturated = parsed
            .samples
            .iter()
            .filter(|sample| **sample == i16::MAX || **sample == i16::MIN)
            .count();
        assert!(
            saturated < parsed.samples.len() / 100,
            "the downmix hard-clipped too many samples"
        );
    }

    #[test]
    fn lists_tracks_from_a_file_without_loading_it() {
        let tracks =
            super::audio_tracks_from_path(&fixture_path("sample-two-opus-tracks.webm")).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].codec, "Opus");
        assert_eq!(tracks[1].id, 2);

        let tracks = super::audio_tracks_from_path(&fixture_path("sample-video.mp4")).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].codec, "AAC");
        assert_eq!(tracks[0].channels, 1);

        let tracks = super::audio_tracks_from_path(&fixture_path("sample-ac3.mkv")).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].codec, "AC-3");
        assert_eq!(tracks[0].channels, 6);
    }

    #[test]
    fn lists_tracks_from_a_padded_mp4() {
        let directory = stream_temp_dir("padded-probe");
        let input = directory.join("padded.mp4");
        let mut padded = std::fs::read(fixture_path("sample-ac3.mp4")).unwrap();
        padded.resize(padded.len() + 16 * 1024 * 1024, 0);
        std::fs::write(&input, padded).unwrap();

        let tracks = super::audio_tracks_from_path(&input).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].codec, "AC-3");
        assert_eq!(tracks[0].channels, 6);
        assert_eq!(tracks[0].sample_rate, 48_000);

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn lists_and_selects_audio_tracks() {
        let input = fixture("sample-two-opus-tracks.webm");
        let tracks = super::audio_tracks(&input, "webm").unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].id, 1);
        assert_eq!(tracks[1].id, 2);
        assert_eq!(tracks[0].codec, "Opus");
        assert_eq!(tracks[0].channels, 1);
        assert_eq!(tracks[0].sample_rate, 48_000);
        assert!(tracks[0].default);
        assert!(!tracks[1].default);
        assert!(tracks[1].label().starts_with("#2"));

        let first =
            convert_bytes("sample-two-opus-tracks.webm", &input, OutputFormat::Wav).unwrap();
        assert_eq!(parse_wav(&first[0].bytes).samples.len(), 48_000);

        let second = convert_bytes_with_options(
            "sample-two-opus-tracks.webm",
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let second = parse_wav(&second[0].bytes);
        assert_eq!(second.samples.len(), 48_480);
        assert_audible(&second.samples);

        let error = convert_bytes_with_options(
            "sample-two-opus-tracks.webm",
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(2),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::AudioTrackNotFound {
                requested: 3,
                available: 2
            }
        ));
    }

    #[test]
    fn default_track_matches_explicit_first_in_buffered_and_streaming_paths() {
        let name = "sample-two-opus-tracks.webm";
        let input = fixture(name);
        let path = fixture_path(name);

        let default = convert_bytes(name, &input, OutputFormat::Wav).unwrap();
        let first = convert_bytes_with_options(
            name,
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        let second = convert_bytes_with_options(
            name,
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(default[0].bytes, first[0].bytes);
        assert_ne!(default[0].bytes, second[0].bytes);
        assert_eq!(parse_wav(&default[0].bytes).samples.len(), 48_000);
        assert_eq!(parse_wav(&second[0].bytes).samples.len(), 48_480);

        let default_stream = stream_wav(&path, ConversionOptions::default());
        let first_stream = stream_wav(
            &path,
            ConversionOptions {
                audio_track: Some(0),
                ..Default::default()
            },
        );
        let second_stream = stream_wav(
            &path,
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        );
        assert_eq!(default_stream, first_stream);
        assert_ne!(default_stream, second_stream);
        assert_eq!(parse_wav(&default_stream).samples.len(), 48_000);
        assert_eq!(parse_wav(&second_stream).samples.len(), 48_480);
    }

    #[test]
    fn selects_audio_tracks_after_non_audio_and_unsupported_streams() {
        let name = "sample-interleaved-tracks.mkv";
        let path = fixture_path(name);
        let input = fixture(name);

        let tracks = super::audio_tracks_from_path(&path).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].index, 0);
        assert_eq!(tracks[0].id, 3);
        assert_eq!(tracks[0].language.as_deref(), Some("eng"));
        assert_eq!(tracks[0].codec, "AC-3");
        assert_eq!(tracks[0].channels, 6);
        assert_eq!(tracks[0].sample_rate, 48_000);
        assert!(tracks[0].label().starts_with("#1 · stream 3 · eng · AC-3"));
        assert_eq!(tracks[1].index, 1);
        assert_eq!(tracks[1].id, 4);
        assert_eq!(tracks[1].language.as_deref(), Some("fra"));
        assert_eq!(tracks[1].codec, "AC-3");
        assert_eq!(tracks[1].channels, 6);
        assert_eq!(tracks[1].sample_rate, 48_000);
        assert!(tracks[1].label().starts_with("#2 · stream 4 · fra · AC-3"));

        let default = convert_bytes(name, &input, OutputFormat::Wav).unwrap();
        let first = convert_bytes_with_options(
            name,
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        let second = convert_bytes_with_options(
            name,
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(default[0].bytes, first[0].bytes);
        assert_ne!(default[0].bytes, second[0].bytes);
        let first_wav = parse_wav(&default[0].bytes);
        let second_wav = parse_wav(&second[0].bytes);
        assert_eq!(first_wav.channels, 6);
        assert_eq!(first_wav.sample_rate, 48_000);
        assert_eq!(first_wav.samples.len(), 96_768 * 6);
        assert_eq!(second_wav.channels, 6);
        assert_eq!(second_wav.sample_rate, 48_000);
        assert_eq!(second_wav.samples.len(), 153_600 * 6);

        let default_stream = stream_wav(&path, ConversionOptions::default());
        let first_stream = stream_wav(
            &path,
            ConversionOptions {
                audio_track: Some(0),
                ..Default::default()
            },
        );
        let second_stream = stream_wav(
            &path,
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        );
        assert_eq!(default_stream, default[0].bytes);
        assert_eq!(first_stream, first[0].bytes);
        assert_eq!(second_stream, second[0].bytes);
        assert_eq!(parse_wav(&second_stream).samples.len(), 153_600 * 6);

        let error = convert_bytes_with_options(
            name,
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(2),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::AudioTrackNotFound {
                requested: 3,
                available: 2
            }
        ));

        let mut output = Cursor::new(Vec::new());
        let error = super::convert_audio_file_to_writer(
            &path,
            name,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(2),
                ..Default::default()
            },
            &mut output,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::AudioTrackNotFound {
                requested: 3,
                available: 2
            }
        ));
    }

    #[test]
    fn decodes_ac3_and_eac3_audio() {
        for (name, codec) in [
            ("sample-ac3.ac3", "AC-3"),
            ("sample-ac3.mkv", "AC-3"),
            ("sample-ac3.mp4", "AC-3"),
            ("sample-eac3.mkv", "E-AC-3"),
        ] {
            let input = fixture(name);
            let extension = std::path::Path::new(name)
                .extension()
                .unwrap()
                .to_str()
                .unwrap();
            let tracks = super::audio_tracks(&input, extension).unwrap();
            assert_eq!(tracks.len(), 1, "{name}");
            assert_eq!(tracks[0].codec, codec, "{name}");
            assert_eq!(tracks[0].channels, 6, "{name}");
            assert_eq!(tracks[0].sample_rate, 48_000, "{name}");

            let wav = convert_bytes(name, &input, OutputFormat::Wav).unwrap();
            let parsed = parse_wav(&wav[0].bytes);
            assert_eq!(parsed.sample_rate, 48_000, "{name}");
            assert_eq!(parsed.channels, 6, "{name}");
            assert_eq!(parsed.samples.len(), 96_768 * 6, "{name}");
            assert_audible(&parsed.samples);

            let stereo = convert_bytes_with_options(
                name,
                &input,
                OutputFormat::Wav,
                ConversionOptions {
                    audio_channels: Some(AudioChannels::Stereo),
                    ..Default::default()
                },
            )
            .unwrap();
            let stereo = parse_wav(&stereo[0].bytes);
            assert_eq!(stereo.channels, 2, "{name}");
            assert_eq!(stereo.samples.len(), 96_768 * 2, "{name}");
            assert_audible(&stereo.samples);
        }
    }

    #[test]
    fn downmixes_centre_and_surround_channels() {
        // Only the centre channel carries audio. Keeping the first two channels
        // would produce silence, which is the bug this test guards against.
        let input = fixture("sample-ac3-center.ac3");
        let stereo = convert_bytes_with_options(
            "sample-ac3-center.ac3",
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        )
        .unwrap();
        let stereo = parse_wav(&stereo[0].bytes);
        assert_eq!(stereo.channels, 2);
        assert_audible(&stereo.samples);

        let mono = convert_bytes_with_options(
            "sample-ac3-center.ac3",
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Mono),
                ..Default::default()
            },
        )
        .unwrap();
        let mono = parse_wav(&mono[0].bytes);
        assert_eq!(mono.channels, 1);
        assert_audible(&mono.samples);
    }

    #[test]
    fn surround_downmix_uses_standard_coefficients_and_avoids_clipping() {
        let centre_only = super::DecodedAudio {
            samples: vec![0.0, 0.0, 0.5, 0.0, 0.0, 0.0],
            channels: 6,
            sample_rate: 48_000,
            source_bits: None,
        };
        let stereo = super::remix_channels(centre_only.clone(), AudioChannels::Stereo);
        assert!((stereo.samples[0] - 0.353_553).abs() < 0.001);
        assert!((stereo.samples[1] - 0.353_553).abs() < 0.001);
        let mono = super::remix_channels(centre_only, AudioChannels::Mono);
        assert!((mono.samples[0] - 0.353_553).abs() < 0.001);

        let loud = super::DecodedAudio {
            samples: vec![1.0; 6 * 100],
            channels: 6,
            sample_rate: 48_000,
            source_bits: None,
        };
        let stereo = super::remix_channels(loud, AudioChannels::Stereo);
        let peak = stereo
            .samples
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        assert!(peak <= 1.0, "the downmix clipped at {peak}");
        assert!(peak > 0.9, "the downmix was attenuated more than needed");
    }

    #[test]
    fn multichannel_aac_reports_a_clear_error() {
        let input = fixture("sample-aac-51.m4a");
        let tracks = super::audio_tracks(&input, "m4a").unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].channels, 6);
        let error = convert_bytes("sample-aac-51.m4a", &input, OutputFormat::Wav).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("multichannel AAC"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn applies_mp4_edit_list_gapless_trimming() {
        let input = fixture("sample-aac.m4a");
        let wav = convert_bytes("sample-aac.m4a", &input, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&wav[0].bytes);
        assert_eq!(parsed.sample_rate, 44_100);
        assert_eq!(parsed.channels, 1);
        assert_eq!(parsed.samples.len(), 88_200);
        assert_audible(&parsed.samples);
    }

    #[test]
    fn resampler_preserves_the_passband_and_rejects_aliases() {
        let tone = |rate: u32, frequency: f64, seconds: f64| super::DecodedAudio {
            samples: (0..(rate as f64 * seconds) as usize)
                .map(|frame| {
                    (std::f64::consts::TAU * frequency * frame as f64 / f64::from(rate)).sin()
                        as f32
                        * 0.5
                })
                .collect(),
            channels: 1,
            sample_rate: rate,
            source_bits: None,
        };
        let rms = |samples: &[f32]| {
            (samples
                .iter()
                .map(|sample| f64::from(*sample) * f64::from(*sample))
                .sum::<f64>()
                / samples.len() as f64)
                .sqrt()
        };

        let passband = super::resample_audio(tone(48_000, 1_000.0, 0.5), 24_000).unwrap();
        assert_eq!(passband.sample_rate, 24_000);
        assert_eq!(passband.samples.len(), 12_000);
        assert!(
            (rms(&passband.samples) - 0.353_553).abs() < 0.03,
            "passband RMS changed to {}",
            rms(&passband.samples)
        );
        let crossings = passband
            .samples
            .windows(2)
            .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
            .count();
        assert!(
            (crossings as i64 - 1_000).abs() < 30,
            "expected about 1000 zero crossings, got {crossings}"
        );

        // 20 kHz cannot exist at 24 kHz. A linear interpolator folds it to
        // 4 kHz; the band-limited filter must remove it instead.
        let alias = super::resample_audio(tone(48_000, 20_000.0, 0.5), 24_000).unwrap();
        assert!(
            rms(&alias.samples) < 0.01,
            "20 kHz aliased through at RMS {}",
            rms(&alias.samples)
        );
    }

    #[test]
    fn writes_24_bit_and_float_wav() {
        let wav = wav_fixture();
        let original = parse_wav(&wav);

        let pcm24 = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Wav,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Bit24,
                ..Default::default()
            },
        )
        .unwrap();
        let (format, channels, rate, bits, offset, size) = wav_layout(&pcm24[0].bytes);
        assert_eq!((format, channels, rate, bits), (1, 2, 8_000, 24));
        assert_eq!(size, 16_000 * 3);
        let sample = 300;
        let bytes = &pcm24[0].bytes[offset + sample * 3..offset + sample * 3 + 3];
        let raw = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]);
        let value = (raw << 8) >> 8;
        assert!(
            (i32::from(original.samples[sample]) * 256 - value).abs() <= 300,
            "24-bit sample {value} does not match {}",
            original.samples[sample]
        );

        let float = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Wav,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Float32,
                ..Default::default()
            },
        )
        .unwrap();
        let (format, channels, rate, bits, offset, size) = wav_layout(&float[0].bytes);
        assert_eq!((format, channels, rate, bits), (3, 2, 8_000, 32));
        assert_eq!(size, 16_000 * 4);
        assert!(
            float[0].bytes.windows(4).any(|window| window == b"fact"),
            "a float WAV should carry a fact chunk"
        );
        let value = f32::from_le_bytes(
            float[0].bytes[offset + sample * 4..offset + sample * 4 + 4]
                .try_into()
                .unwrap(),
        );
        let expected = f32::from(original.samples[sample]) / f32::from(i16::MAX);
        assert!((value - expected).abs() < 0.001);
    }

    #[test]
    fn writes_24_bit_flac_and_rejects_float_flac() {
        let wav = wav_fixture();
        let flac = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Flac,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Bit24,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(&flac[0].bytes[0..4], b"fLaC");
        let streaminfo = &flac[0].bytes[8..8 + 34];
        let packed = u64::from_be_bytes(streaminfo[10..18].try_into().unwrap());
        assert_eq!(((packed >> 36) & 0x1F) + 1, 24);

        let back = convert_bytes("tone.flac", &flac[0].bytes, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&back[0].bytes);
        assert_eq!(decoded.samples.len(), 16_000);
        assert_audible(&decoded.samples);

        let error = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Flac,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Float32,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("32-bit float"));
    }

    #[test]
    fn converts_wav_audio_to_mp3_and_back() {
        let wav = wav_fixture_at(44_100, 44_100);
        let mp3 = convert_bytes("tone.wav", &wav, OutputFormat::Mp3).unwrap();
        assert_eq!(mp3[0].name, PathBuf::from("tone.mp3"));
        assert!(
            mp3[0].bytes.starts_with(b"ID3") || mp3[0].bytes[0] == 0xFF,
            "the MP3 output has no MPEG frame or ID3 header"
        );

        let back = convert_bytes("tone.mp3", &mp3[0].bytes, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&back[0].bytes);
        assert_eq!(parsed.sample_rate, 44_100);
        assert_eq!(parsed.channels, 2);
        assert!(
            (parsed.samples.len() as i64 - 88_200).abs() < 2_500,
            "MP3 round trip produced {} samples",
            parsed.samples.len()
        );
        assert_audible(&parsed.samples);

        // LAME has to clamp its bitrate for low sample rates instead of
        // failing the conversion.
        let low_rate = convert_bytes("tone.wav", &wav_fixture(), OutputFormat::Mp3)
            .expect("8 kHz stereo MP3 output should work");
        assert!(low_rate[0].bytes.len() > 100);
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
    fn malformed_containers_return_errors_instead_of_panicking() {
        for (name, bytes) in [
            ("broken.webm", b"\x1a\x45\xdf\xa3".as_slice()),
            ("broken.mkv", &[0x1A, 0x45, 0xDF, 0xA3, 0x80][..]),
            ("broken.mp4", b"\0\0\0\x08moov".as_slice()),
            ("broken.ac3", b"\x0b\x77".as_slice()),
            ("empty.flac", b"".as_slice()),
        ] {
            let _ = convert_bytes(name, bytes, OutputFormat::Wav);
            let _ = super::audio_tracks(bytes, "webm");
        }
        assert_eq!(
            super::matroska_discard_padding(b"\x1a\x45\xdf\xa3", 1),
            None
        );
        assert_eq!(super::matroska_discard_padding(b"not matroska", 1), None);
        assert_eq!(super::mp4_gapless_info(b"\0\0\0\x08moov", 0), None);
    }
}
