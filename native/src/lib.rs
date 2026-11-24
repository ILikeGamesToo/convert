// SPDX-License-Identifier: GPL-2.0-or-later

use image::ImageEncoder;
use pdf_writer::{Content, Filter, Name, Pdf, Rect, Ref, Str};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashSet},
    ffi::{OsStr, OsString},
    fs::File,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::{Mutex, OnceLock},
    time::Duration,
};

mod error;
pub mod input_extensions;
pub mod input_format;
mod options;
mod streaming;

pub use error::ConvertError;
pub use input_extensions::supported_input_extensions;
pub use options::ConversionOptions;
pub use streaming::{convert_audio_file_to_writer, convert_audio_file_to_writer_with_cancel};

const MAX_AUDIO_SAMPLES: usize = 120_000_000;

const MAX_SVG_DIMENSION: u32 = 8_192;

const MAX_SVG_PIXELS: u64 = 32_000_000;

const MAX_IMAGE_DIMENSION: u32 = 32_768;

const MAX_IMAGE_ALLOC: u64 = 512 * 1024 * 1024;

const MAX_PDF_PAGES: usize = 5_000;

const MAX_ARCHIVE_ENTRIES: usize = 10_000;

const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

const MAX_ARCHIVE_ENTRY_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
    /// Remux a compatible video into an MP4 container without re-encoding.
    Mp4,
    /// Remux a compatible video into WebM without re-encoding. Incompatible
    /// codec/container pairs are rejected.
    Webm,
    Zip,
    Tar,
    TarGz,
    Gzip,
    /// Extract an archive into separate files instead of creating one.
    Files,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AudioChannels {
    Mono,
    Stereo,
}

/// Sample format for WAV and FLAC output.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
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

/// Bitrate for MP3 output. MP3 is lossy, so sample bit depth does not apply.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Mp3Bitrate {
    Kbps128,
    #[default]
    Kbps192,
    Kbps256,
    Kbps320,
}

impl Mp3Bitrate {
    pub const ALL: [Self; 4] = [Self::Kbps128, Self::Kbps192, Self::Kbps256, Self::Kbps320];

    pub fn kbps(self) -> u16 {
        match self {
            Self::Kbps128 => 128,
            Self::Kbps192 => 192,
            Self::Kbps256 => 256,
            Self::Kbps320 => 320,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Kbps128 => "128 kbps",
            Self::Kbps192 => "192 kbps",
            Self::Kbps256 => "256 kbps",
            Self::Kbps320 => "320 kbps",
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
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

impl OutputFormat {
    pub const ALL: [Self; 40] = [
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
        Self::Mp4,
        Self::Webm,
        Self::Zip,
        Self::Tar,
        Self::TarGz,
        Self::Gzip,
        Self::Files,
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
            Self::Mp4 => "MP4 video (stream copy)",
            Self::Webm => "WebM video (stream copy)",
            Self::Zip => "ZIP archive",
            Self::Tar => "TAR archive",
            Self::TarGz => "TAR.GZ archive",
            Self::Gzip => "GZIP compressed file",
            Self::Files => "Extracted files",
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

    /// Whether this target is a video container handled by the external
    /// ffmpeg stream-copy remux path.
    pub fn is_video_target(self) -> bool {
        matches!(self, Self::Mp4 | Self::Webm)
    }

    /// Whether this target needs transparency composited onto
    /// `ConversionOptions::image_background` instead of preserving alpha.
    pub fn uses_image_background(self) -> bool {
        matches!(
            self,
            Self::Jpeg | Self::Bmp | Self::Hdr | Self::Ppm | Self::Pbm | Self::Pgm | Self::Pdf
        )
    }

    pub fn extension(self) -> &'static str {
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
            Self::Mp4 => "mp4",
            Self::Webm => "webm",
            Self::Zip => "zip",
            Self::Tar => "tar",
            Self::TarGz => "tar.gz",
            Self::Gzip => "gz",
            Self::Files => "files",
        }
    }

    /// Resolves a user-facing extension such as `png`, `.jpg`, `tar.gz` or
    /// `tgz` to an output format. Common aliases are accepted so the CLI and
    /// GUI agree on names.
    pub fn from_extension(extension: &str) -> Option<Self> {
        let extension = extension
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase();
        let normalized = match extension.as_str() {
            "jpeg" => "jpg",
            "tif" => "tiff",
            "aif" => "aiff",
            "yml" => "yaml",
            "tgz" => "tar.gz",
            "gzip" => "gz",
            "text" => "txt",
            _ => extension.as_str(),
        };
        Self::ALL
            .iter()
            .copied()
            .find(|format| format.extension() == normalized)
    }

    /// Whether files with this name can be used as input for the format.
    pub fn supports_input(self, file_name: &str) -> bool {
        let extension = file_extension(file_name);
        match self {
            Self::Png | Self::Jpeg | Self::Webp => {
                is_image_input_extension(&extension) || is_video_input_extension(&extension)
            }
            Self::Gif
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
            Self::Text => is_data_extension(&extension) || extension == "pdf",
            Self::Csv | Self::Json | Self::Yaml | Self::Toml => is_data_extension(&extension),
            Self::TextUtf8NoBom
            | Self::TextUtf8Bom
            | Self::TextUtf16Le
            | Self::TextUtf16Be
            | Self::TextUtf32Le
            | Self::TextUtf32Be => extension == "txt" || extension == "pdf",
            Self::Wav | Self::Flac | Self::Aiff | Self::Au | Self::Opus | Self::Mp3 => {
                is_audio_extension(&extension)
            }
            Self::Mp4 | Self::Webm => is_video_input_extension(&extension),
            Self::Zip | Self::Tar | Self::TarGz | Self::Gzip => true,
            Self::Files => is_archive_name(file_name),
        }
    }
}

#[derive(Debug)]
pub struct ConvertedFile {
    pub name: PathBuf,
    pub bytes: Vec<u8>,
}

/// One progress update from a future video transcode path. `total` is `None`
/// when ffmpeg cannot determine the input duration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoProgress {
    pub out_time: Duration,
    pub total: Option<Duration>,
}

impl VideoProgress {
    pub fn fraction(self) -> Option<f32> {
        let total = self.total?.as_secs_f64();
        if total <= 0.0 {
            return None;
        }
        Some((self.out_time.as_secs_f64() / total).clamp(0.0, 1.0) as f32)
    }
}

/// Cancellation and progress hooks reserved for a future ffmpeg transcode
/// path. This slice only performs stream-copy remuxes and thumbnails, which
/// are synchronous and ignore these hooks.
pub struct VideoControl<'a> {
    pub cancel: &'a AtomicBool,
    pub on_progress: &'a mut dyn FnMut(VideoProgress),
}

/// Location of the external ffmpeg binary. An explicit path wins, then the
/// `CONVERT2IT_FFMPEG` / `FFMPEG` environment variables, then PATH.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VideoToolConfig {
    pub ffmpeg: Option<PathBuf>,
}

impl VideoToolConfig {
    pub fn with_ffmpeg(path: impl Into<PathBuf>) -> Self {
        Self {
            ffmpeg: Some(path.into()),
        }
    }

    /// Resolves the ffmpeg executable using the override chain above.
    pub fn resolve(&self) -> Option<PathBuf> {
        if let Some(path) = &self.ffmpeg {
            return Some(path.clone());
        }
        for variable in ["CONVERT2IT_FFMPEG", "FFMPEG"] {
            if let Some(path) = std::env::var_os(variable) {
                if !path.is_empty() {
                    return Some(PathBuf::from(path));
                }
            }
        }
        find_on_path("ffmpeg")
    }
}

/// The automatic route selected for a video conversion.
///
/// This is deliberately not a user-facing mode. The current video slice only
/// advertises stream-copy remuxes; the transcode variant is reserved for a
/// future slice and is never selected by the conversion entry points.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoRoute {
    Remux,
    Transcode,
}

impl VideoRoute {
    pub fn label(self) -> &'static str {
        match self {
            Self::Remux => "remux",
            Self::Transcode => "transcode",
        }
    }
}

/// One advertised video route. The same [`VideoRoute`] enum is used by the
/// registry and the CLI listing so they cannot disagree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoRouteInfo {
    pub source_extension: &'static str,
    pub target: OutputFormat,
    pub route: VideoRoute,
    pub video_codecs: &'static [&'static str],
    pub audio_codecs: &'static [&'static str],
}

/// One native input handler as exposed to the CLI, the GUI and parity tooling.
///
/// The web app has a much wider handler set; this registry is the native
/// source of truth so gaps can be reported without scraping UI code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatHandler {
    pub input: input_format::InputFormat,
    pub canonical_extension: Option<&'static str>,
    pub outputs: Vec<OutputFormat>,
    pub video_routes: Vec<VideoRouteInfo>,
}

/// Returns every selectable native input format and the outputs it accepts.
///
/// `Any` and `Mixed` are bulk-archive modes, so they report the archive targets
/// directly instead of pretending to have one canonical extension.
pub fn native_format_registry() -> Vec<FormatHandler> {
    input_format::InputFormat::SELECTABLE
        .iter()
        .copied()
        .map(|input| {
            let canonical_extension = input.canonical_extension();
            let outputs = if matches!(
                input,
                input_format::InputFormat::Any | input_format::InputFormat::Mixed
            ) {
                vec![
                    OutputFormat::Files,
                    OutputFormat::Zip,
                    OutputFormat::Tar,
                    OutputFormat::TarGz,
                    OutputFormat::Gzip,
                ]
            } else {
                let sample = canonical_extension
                    .map(|extension| format!("sample.{extension}"))
                    .unwrap_or_else(|| "sample.bin".to_owned());
                OutputFormat::ALL
                    .iter()
                    .copied()
                    .filter(|target| target.supports_input(&sample))
                    .collect()
            };
            let video_routes = if input == input_format::InputFormat::Video {
                native_video_routes()
            } else {
                Vec::new()
            };
            FormatHandler {
                input,
                canonical_extension,
                outputs,
                video_routes,
            }
        })
        .collect()
}

const REMUX_VIDEO_TO_MP4: &[&str] = &["h264", "hevc", "vp9", "av1"];

const REMUX_AUDIO_TO_MP4: &[&str] = &["aac", "opus", "mp3", "ac3", "eac3"];

const REMUX_VIDEO_TO_WEBM: &[&str] = &["vp8", "vp9", "av1"];

const REMUX_AUDIO_TO_WEBM: &[&str] = &["opus", "vorbis"];

/// Routes advertised by the native registry. Every route in this slice is a
/// stream copy; incompatible codec/container pairs are rejected rather than
/// transcoded.
pub fn native_video_routes() -> Vec<VideoRouteInfo> {
    vec![
        VideoRouteInfo {
            source_extension: "mp4",
            target: OutputFormat::Webm,
            route: VideoRoute::Remux,
            video_codecs: REMUX_VIDEO_TO_WEBM,
            audio_codecs: REMUX_AUDIO_TO_WEBM,
        },
        VideoRouteInfo {
            source_extension: "mkv",
            target: OutputFormat::Webm,
            route: VideoRoute::Remux,
            video_codecs: REMUX_VIDEO_TO_WEBM,
            audio_codecs: REMUX_AUDIO_TO_WEBM,
        },
        VideoRouteInfo {
            source_extension: "webm",
            target: OutputFormat::Mp4,
            route: VideoRoute::Remux,
            video_codecs: REMUX_VIDEO_TO_MP4,
            audio_codecs: REMUX_AUDIO_TO_MP4,
        },
        VideoRouteInfo {
            source_extension: "mkv",
            target: OutputFormat::Mp4,
            route: VideoRoute::Remux,
            video_codecs: REMUX_VIDEO_TO_MP4,
            audio_codecs: REMUX_AUDIO_TO_MP4,
        },
    ]
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
    convert_bytes_with_video_config(
        input_name,
        input,
        target,
        options,
        &VideoToolConfig::default(),
    )
}

/// Conversion entry point with an explicit ffmpeg configuration. The normal
/// entry points resolve `CONVERT2IT_FFMPEG`/`FFMPEG` and PATH themselves.
pub fn convert_bytes_with_video_config(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    options: ConversionOptions,
    video: &VideoToolConfig,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    convert_bytes_with_video_control(input_name, input, target, options, video, None)
}

/// Conversion entry point that accepts the reserved transcode progress and
/// cancellation hooks. This slice's remux and thumbnail paths are synchronous,
/// so the hooks are ignored; non-video conversions ignore them as well.
pub fn convert_bytes_with_video_control(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    options: ConversionOptions,
    video: &VideoToolConfig,
    _control: Option<&mut VideoControl<'_>>,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    let extension = file_extension(input_name);
    if target == OutputFormat::Files {
        return extract_archive(input_name, input);
    }
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
    if is_pdf_extension(&extension) {
        let text = extract_pdf_text(input)?;
        let bytes = match target {
            OutputFormat::Text
            | OutputFormat::TextUtf8NoBom
            | OutputFormat::TextUtf8Bom
            | OutputFormat::TextUtf16Le
            | OutputFormat::TextUtf16Be
            | OutputFormat::TextUtf32Le
            | OutputFormat::TextUtf32Be => encode_text(&text, target)?,
            _ => return Err(unsupported(&extension, target)),
        };
        return Ok(vec![ConvertedFile {
            name: output_name(input_name, target.extension()),
            bytes,
        }]);
    }

    if target.is_video_target() {
        if !is_video_input_extension(&extension) {
            return Err(unsupported(&extension, target));
        }
        return convert_video(input_name, input, target, video);
    }

    if target.image_format().is_some() && is_video_input_extension(&extension) {
        return extract_video_frame(input_name, input, target, options, video);
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
        let bytes = preserve_jpeg_exif(input, bytes, options);
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
            OutputFormat::Mp3 => encode_mp3(audio, options.mp3_bitrate)?,
            _ => unreachable!("only audio targets reach this branch"),
        };
        let bytes = match target {
            OutputFormat::Mp3 => preserve_mp3_id3v2(input, bytes),
            OutputFormat::Flac => preserve_flac_tags(input, bytes),
            _ => bytes,
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
    convert_batch_with_video_config(inputs, target, options, &VideoToolConfig::default())
}

pub fn convert_batch_with_video_config(
    inputs: &[(PathBuf, Vec<u8>)],
    target: OutputFormat,
    options: ConversionOptions,
    video: &VideoToolConfig,
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
        converted.extend(convert_bytes_with_video_config(
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .as_ref(),
            bytes,
            target,
            options,
            video,
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

fn extract_pdf_text(input: &[u8]) -> Result<String, ConvertError> {
    // pdf-extract is pure Rust and handles the common text operators. It is
    // deliberately wrapped because malformed PDFs can otherwise unwind through
    // the conversion API instead of returning a normal error.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem(input)
    }));
    match result {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(error)) => Err(ConvertError::Pdf(format!("{error:?}"))),
        Err(_) => Err(ConvertError::Pdf(
            "the PDF parser stopped on a malformed document".to_owned(),
        )),
    }
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

fn is_archive_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.ends_with(".zip")
        || name.ends_with(".tar")
        || name.ends_with(".tar.gz")
        || name.ends_with(".tgz")
        || name.ends_with(".gz")
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

fn is_pdf_extension(extension: &str) -> bool {
    input_extensions::PDF_INPUT_EXTENSIONS.contains(&extension)
}

fn is_video_input_extension(extension: &str) -> bool {
    matches!(extension, "mp4" | "m4v" | "mov" | "mkv" | "webm")
}

const FFMPEG_STDERR_LIMIT: usize = 8_192;

static VIDEO_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        #[cfg(windows)]
        for extension in ["exe", "cmd", "bat", "com"] {
            let candidate = directory.join(format!("{name}.{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        #[cfg(not(windows))]
        {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn ffmpeg_command(path: &Path) -> Command {
    #[cfg(windows)]
    if matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase())
            .as_deref(),
        Some("cmd" | "bat")
    ) {
        let mut command = Command::new("cmd");
        command.arg("/C").arg(path);
        return command;
    }
    Command::new(path)
}

fn ffmpeg_stderr(bytes: &[u8]) -> String {
    let text: String = String::from_utf8_lossy(bytes)
        .trim()
        .chars()
        .take(FFMPEG_STDERR_LIMIT)
        .collect();
    if text.is_empty() {
        "<no stderr>".to_owned()
    } else {
        text
    }
}

fn run_ffmpeg(command: &mut Command, ffmpeg: &Path) -> Result<Output, ConvertError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command.output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ConvertError::FfmpegNotFound {
                path: ffmpeg.display().to_string(),
            }
        } else {
            ConvertError::FfmpegSpawn(error.to_string())
        }
    })?;
    if !output.status.success() {
        let status = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "terminated by signal".to_owned());
        return Err(ConvertError::FfmpegFailed {
            status,
            stderr: ffmpeg_stderr(&output.stderr),
        });
    }
    Ok(output)
}

struct TempVideoPath {
    path: PathBuf,
}

impl TempVideoPath {
    fn new(extension: &str) -> Self {
        let sequence = VIDEO_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "convert2it-{}-{sequence}.{extension}",
            std::process::id()
        ));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempVideoPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn resolve_ffmpeg(video: &VideoToolConfig) -> Result<PathBuf, ConvertError> {
    video.resolve().ok_or_else(|| ConvertError::FfmpegNotFound {
        path: "ffmpeg".to_owned(),
    })
}

fn convert_video(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    video: &VideoToolConfig,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    remux_video(input_name, input, target, video)
}

fn remux_video(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    video: &VideoToolConfig,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    let extension = file_extension(input_name);
    validate_remux(input, &extension, target)?;
    let ffmpeg = resolve_ffmpeg(video)?;
    let input_path = TempVideoPath::new(if extension.is_empty() {
        "bin"
    } else {
        &extension
    });
    std::fs::write(input_path.path(), input)?;
    let output_path = TempVideoPath::new(target.extension());

    let mut command = ffmpeg_command(&ffmpeg);
    command
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(input_path.path())
        .args(["-map", "0:v?", "-map", "0:a?", "-c", "copy"]);
    match target {
        OutputFormat::Mp4 => {
            command.args(["-movflags", "+faststart", "-f", "mp4"]);
        }
        OutputFormat::Webm => {
            command.args(["-f", "webm"]);
        }
        _ => unreachable!("only video containers reach remux_video"),
    }
    command.arg(output_path.path());
    run_ffmpeg(&mut command, &ffmpeg)?;

    let bytes = std::fs::read(output_path.path()).map_err(|_| ConvertError::FfmpegEmptyOutput)?;
    if bytes.is_empty() {
        return Err(ConvertError::FfmpegEmptyOutput);
    }
    let valid_container = match target {
        OutputFormat::Mp4 => bytes.get(4..8) == Some(b"ftyp"),
        OutputFormat::Webm => bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]),
        _ => true,
    };
    if !valid_container {
        return Err(ConvertError::FfmpegInvalidOutput {
            container: target.extension(),
        });
    }
    Ok(vec![ConvertedFile {
        name: output_name(input_name, target.extension()),
        bytes,
    }])
}

fn extract_video_frame(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    options: ConversionOptions,
    video: &VideoToolConfig,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    let ffmpeg = resolve_ffmpeg(video)?;
    let extension = file_extension(input_name);
    let input_path = TempVideoPath::new(if extension.is_empty() {
        "bin"
    } else {
        &extension
    });
    std::fs::write(input_path.path(), input)?;

    let frame_time_ms = options
        .video_frame_time_ms
        .min(ConversionOptions::MAX_VIDEO_FRAME_TIME_MS);
    let mut command = ffmpeg_command(&ffmpeg);
    command
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-ss"])
        .arg(format!("{:.3}", frame_time_ms as f64 / 1_000.0))
        .arg("-i")
        .arg(input_path.path())
        .args(["-map", "0:v:0", "-frames:v", "1", "-an"]);
    let width = options
        .video_thumbnail_width
        .min(ConversionOptions::MAX_VIDEO_THUMBNAIL_WIDTH);
    if width > 0 {
        command.args(["-vf", &format!("scale={}:-2:flags=lanczos", width.max(16))]);
    }
    command.args(["-f", "image2pipe", "-vcodec", "png", "pipe:1"]);

    let output = run_ffmpeg(&mut command, &ffmpeg)?;
    if output.stdout.is_empty() {
        return Err(ConvertError::FfmpegEmptyOutput);
    }
    let image = image::load_from_memory(&output.stdout)?;
    let bytes = encode_image(image, target, options)?;
    Ok(vec![ConvertedFile {
        name: output_name(input_name, target.extension()),
        bytes,
    }])
}

#[derive(Default)]
struct VideoCodecs {
    video: Vec<String>,
    audio: Vec<String>,
    unknown_video: bool,
    unknown_audio: bool,
}

fn validate_remux(input: &[u8], extension: &str, target: OutputFormat) -> Result<(), ConvertError> {
    let codecs = if is_mp4_extension(extension) {
        mp4_video_codecs(input)
    } else if matches!(extension, "mkv" | "webm") {
        matroska_video_codecs(input)
    } else {
        None
    };
    let Some(codecs) = codecs else {
        return Ok(());
    };
    let (video_allowed, audio_allowed): (&[&str], &[&str]) = match target {
        OutputFormat::Mp4 => (REMUX_VIDEO_TO_MP4, REMUX_AUDIO_TO_MP4),
        OutputFormat::Webm => (REMUX_VIDEO_TO_WEBM, REMUX_AUDIO_TO_WEBM),
        _ => return Ok(()),
    };
    for codec in &codecs.video {
        if !video_allowed.contains(&codec.as_str()) {
            return Err(ConvertError::UnsupportedVideoCodec {
                stream: "video",
                codec: codec.clone(),
                container: target.extension(),
            });
        }
    }
    for codec in &codecs.audio {
        if !audio_allowed.contains(&codec.as_str()) {
            return Err(ConvertError::UnsupportedVideoCodec {
                stream: "audio",
                codec: codec.clone(),
                container: target.extension(),
            });
        }
    }
    Ok(())
}

fn mp4_video_codecs(input: &[u8]) -> Option<VideoCodecs> {
    let moov = mp4_find(input, b"moov")?;
    let mut codecs = VideoCodecs::default();
    for (kind, trak) in mp4_child_boxes(moov) {
        if kind != *b"trak" {
            continue;
        }
        let mdia = match mp4_find(trak, b"mdia") {
            Some(mdia) => mdia,
            None => continue,
        };
        let handler = mp4_find(mdia, b"hdlr")
            .and_then(|hdlr| hdlr.get(8..12))
            .unwrap_or(b"");
        let stsd = mp4_find(mdia, b"minf")
            .and_then(|minf| mp4_find(minf, b"stbl"))
            .and_then(|stbl| mp4_find(stbl, b"stsd"));
        let Some(stsd) = stsd else {
            continue;
        };
        let entries = stsd
            .get(4..8)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_be_bytes)
            .unwrap_or(0) as usize;
        let mut position = 8;
        for _ in 0..entries {
            let Some(size_bytes) = stsd.get(position..position + 4) else {
                break;
            };
            let size = u32::from_be_bytes(size_bytes.try_into().ok()?) as usize;
            if size < 8 || position + size > stsd.len() {
                break;
            }
            let format = stsd.get(position + 4..position + 8)?;
            match handler {
                b"vide" => match mp4_codec_name(format) {
                    Some(codec) => codecs.video.push(codec.to_owned()),
                    None => codecs.unknown_video = true,
                },
                b"soun" => match mp4_codec_name(format) {
                    Some(codec) => codecs.audio.push(codec.to_owned()),
                    None => codecs.unknown_audio = true,
                },
                _ => {}
            }
            position += size;
        }
    }
    Some(codecs)
}

fn mp4_codec_name(format: &[u8]) -> Option<&'static str> {
    match format {
        b"avc1" | b"avc3" => Some("h264"),
        b"hvc1" | b"hev1" => Some("hevc"),
        b"vp08" => Some("vp8"),
        b"vp09" => Some("vp9"),
        b"av01" => Some("av1"),
        b"mp4a" => Some("aac"),
        b"Opus" => Some("opus"),
        b"ac-3" => Some("ac3"),
        b"ec-3" => Some("eac3"),
        b"mp3 " => Some("mp3"),
        b"vorb" => Some("vorbis"),
        _ => None,
    }
}

fn matroska_video_codecs(input: &[u8]) -> Option<VideoCodecs> {
    let mut position = 0;
    let (segment_start, segment_end) = loop {
        let (id, size) = read_ebml_element(input, &mut position)?;
        if id == 0x1853_8067 {
            let end = ebml_element_end(position, size, input.len());
            break (position, end);
        }
        skip_ebml_element(input, &mut position, size)?;
    };
    let mut codecs = VideoCodecs::default();
    let mut scan = segment_start;
    while scan < segment_end {
        let (id, size) = read_ebml_element(input, &mut scan)?;
        let end = ebml_element_end(scan, size, segment_end);
        if id == 0x1654_AE6B {
            parse_matroska_tracks(&input[scan..end], &mut codecs);
        }
        scan = end;
    }
    Some(codecs)
}

fn parse_matroska_tracks(data: &[u8], codecs: &mut VideoCodecs) {
    let mut position = 0;
    while position < data.len() {
        let Some((id, size)) = read_ebml_element(data, &mut position) else {
            break;
        };
        let end = ebml_element_end(position, size, data.len());
        if id == 0xAE {
            parse_matroska_track(&data[position..end], codecs);
        }
        position = end;
    }
}

fn parse_matroska_track(data: &[u8], codecs: &mut VideoCodecs) {
    let mut track_type = None;
    let mut codec_id = None;
    let mut position = 0;
    while position < data.len() {
        let Some((id, size)) = read_ebml_element(data, &mut position) else {
            break;
        };
        let end = ebml_element_end(position, size, data.len());
        match id {
            0x83 => track_type = size.and_then(|size| read_ebml_uint(data, position, size)),
            0x86 => {
                codec_id = size.and_then(|size| {
                    data.get(position..position + size as usize)
                        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                })
            }
            _ => {}
        }
        position = end;
    }
    let Some(codec) = codec_id.as_deref().and_then(matroska_codec_name) else {
        match track_type {
            Some(1) => codecs.unknown_video = true,
            Some(2) => codecs.unknown_audio = true,
            _ => {}
        }
        return;
    };
    match track_type {
        Some(1) => codecs.video.push(codec.to_owned()),
        Some(2) => codecs.audio.push(codec.to_owned()),
        _ => {}
    }
}

fn matroska_codec_name(codec: &str) -> Option<&'static str> {
    match codec {
        "V_VP8" => Some("vp8"),
        "V_VP9" => Some("vp9"),
        "V_AV1" => Some("av1"),
        "V_MPEG4/ISO/AVC" => Some("h264"),
        "V_MPEGH/ISO/HEVC" => Some("hevc"),
        "A_OPUS" => Some("opus"),
        "A_VORBIS" => Some("vorbis"),
        "A_AAC" => Some("aac"),
        "A_MPEG/L3" => Some("mp3"),
        "A_AC3" => Some("ac3"),
        "A_EAC3" => Some("eac3"),
        _ => None,
    }
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
///
/// A duplicated orientation tag in the primary image IFD, or across multiple
/// primary EXIF segments, is ambiguous, so it is ignored rather than picking
/// one and potentially rotating the image the wrong way. The same is true of a
/// missing or malformed tag; callers then leave the pixels alone.
fn exif_orientation(input: &[u8]) -> Option<u16> {
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
fn tiff_orientations(tiff: &[u8]) -> Vec<u16> {
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
fn preserve_jpeg_exif(input: &[u8], output: Vec<u8>, options: ConversionOptions) -> Vec<u8> {
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

fn jpeg_exif_segments(input: &[u8]) -> Vec<Vec<u8>> {
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

fn set_tiff_orientation(tiff: &mut [u8], orientation: u16) {
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

fn composite_channel(source: u8, alpha: u8, background: u8) -> u8 {
    let alpha = u16::from(alpha);
    let inverse = 255 - alpha;
    ((u16::from(source) * alpha + u16::from(background) * inverse + 127) / 255) as u8
}

/// Composites transparent pixels onto an opaque background using straight-alpha
/// blending: `source * alpha + background * (1 - alpha)`.
fn flatten_image(image: image::DynamicImage, background: [u8; 3]) -> image::DynamicImage {
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

fn encode_image(
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

/// Frames to drop from the start, an optional exact length, and frames to
/// discard from the end. All values are in source frames after any decoder
/// delay has already been removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AudioTrim {
    skip_frames: usize,
    limit_frames: Option<usize>,
    tail_padding: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AudioCodecKind {
    Symphonia,
    Opus,
    /// AC-3 and E-AC-3 frames come from filmcraft-ac3 because Symphonia 0.5
    /// has no AC-3 codec feature. They still use the same track selection,
    /// trim and downmix code as every other codec.
    Ac3,
}

struct AudioTrackEntry {
    info: AudioTrackInfo,
    kind: AudioCodecKind,
    /// Position of the track in the container's track list. MP4 metadata
    /// boxes are indexed by this value, not by the audio-only ordinal.
    track_index: usize,
    /// Opus decoder delay declared by the codec header. MP4 edit lists are
    /// expressed on the undecoded timeline, so this must be removed from their
    /// skip value when the decoder already dropped it.
    pre_skip: u32,
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
            pre_skip: 0,
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
            pre_skip: 0,
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
            pre_skip: 0,
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
            pre_skip: 0,
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
            pre_skip: 0,
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
            pre_skip: 0,
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

        let (kind, codec, channels, sample_rate, pre_skip) =
            if codec_params.codec == symphonia::core::codecs::CODEC_TYPE_OPUS {
                let (channels, pre_skip) = opus_stream_info(&codec_params)
                    .map(|info| (info.channels, info.pre_skip))
                    .unwrap_or((0, 0));
                (
                    AudioCodecKind::Opus,
                    "Opus".to_owned(),
                    channels,
                    codec_params.sample_rate.unwrap_or(48_000),
                    pre_skip,
                )
            } else if let Some(header) = ac3_header {
                (
                    AudioCodecKind::Ac3,
                    ac3_codec_label(&header).to_owned(),
                    header.channels(),
                    header.sample_rate,
                    0,
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
                    0,
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
                    0,
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
            pre_skip,
        });
    }

    if entries.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    Ok(entries)
}

/// Precision of the decoded integer source when it is meaningful. Lossy
/// codecs decode through an f32 pipeline and do not have a source integer
/// precision, so they are reported as unknown and get 16-bit dither.
fn reported_source_bits(
    codec: symphonia::core::codecs::CodecType,
    bits_per_sample: Option<u32>,
) -> Option<u16> {
    use symphonia::core::codecs::*;
    if matches!(
        codec,
        CODEC_TYPE_AAC
            | CODEC_TYPE_MP1
            | CODEC_TYPE_MP2
            | CODEC_TYPE_MP3
            | CODEC_TYPE_VORBIS
            | CODEC_TYPE_OPUS
    ) {
        return None;
    }
    bits_per_sample.and_then(|bits| u16::try_from(bits).ok())
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

    let mut format = probe_symphonia(input, extension)?;
    let codec_params = format
        .tracks()
        .iter()
        .find(|candidate| candidate.id == track_id)
        .map(|candidate| candidate.codec_params.clone())
        .ok_or(ConvertError::NoAudioTrack)?;
    let pre_skip = if kind == AudioCodecKind::Opus {
        opus_stream_info(&codec_params)
            .map(|info| info.pre_skip)
            .unwrap_or(0)
    } else {
        0
    };
    let mp4_gapless = if is_mp4_extension(extension) {
        mp4_find(input, b"moov").and_then(|moov| mp4_gapless_info(moov, track_index))
    } else {
        None
    };

    let mut audio = if kind == AudioCodecKind::Opus {
        decode_opus_track(format.as_mut(), track_id, &codec_params)?
    } else if kind == AudioCodecKind::Ac3 {
        decode_ac3_track(format.as_mut(), track_id)?
    } else {
        decode_symphonia_track(format.as_mut(), track_id, &codec_params, track_channels)?
    };

    if let Some(info) = mp4_gapless {
        apply_audio_trim(&mut audio, mp4_trim_for_kind(info, kind, pre_skip));
    }
    if matches!(extension, "mkv" | "webm") && kind == AudioCodecKind::Opus {
        let timing = matroska_audio_timing(input, track_id);
        apply_audio_trim(&mut audio, timing.to_trim());
    }
    if audio.samples.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    Ok(audio)
}

fn decode_symphonia_track(
    format: &mut dyn symphonia::core::formats::FormatReader,
    track_id: u32,
    codec_params: &symphonia::core::codecs::CodecParameters,
    track_channels: usize,
) -> Result<DecodedAudio, ConvertError> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error as SymphoniaError;

    let mut decoder = symphonia::default::get_codecs()
        .make(codec_params, &DecoderOptions::default())
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

    let source_bits = reported_source_bits(codec_params.codec, codec_params.bits_per_sample);
    Ok(DecodedAudio {
        samples,
        channels,
        sample_rate,
        source_bits,
    })
}

fn mp4_trim_for_kind(info: Mp4Gapless, kind: AudioCodecKind, pre_skip: u32) -> AudioTrim {
    let pre_skip = pre_skip as usize;
    let skip_frames = if kind == AudioCodecKind::Opus {
        info.skip.saturating_sub(pre_skip)
    } else {
        info.skip
    };
    let limit_frames = if kind == AudioCodecKind::Opus && !info.limit_is_presentation {
        info.limit.map(|limit| limit.saturating_sub(pre_skip))
    } else {
        info.limit
    };
    AudioTrim {
        skip_frames,
        limit_frames,
        tail_padding: 0,
    }
}

fn apply_audio_trim(audio: &mut DecodedAudio, trim: AudioTrim) {
    let channels = audio.channels.max(1);
    let frames = audio.samples.len() / channels;
    let skip = trim.skip_frames.min(frames);
    if skip > 0 {
        audio.samples.drain(..skip * channels);
    }
    if let Some(limit) = trim.limit_frames {
        let available = audio.samples.len() / channels;
        if limit < available {
            audio.samples.truncate(limit * channels);
        }
    }
    let padding = trim.tail_padding.min(audio.samples.len() / channels);
    if padding > 0 {
        audio
            .samples
            .truncate(audio.samples.len() - padding * channels);
    }
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

struct OpusStreamInfo {
    channels: usize,
    pre_skip: u32,
    mapping_family: u8,
}

/// Opus mapping family 0 only defines mono and stereo. Family 1 defines the
/// Vorbis surround layouts up to 7.1, which the multistream decoder supports.
fn opus_channels(channels: usize, mapping_family: u8) -> Result<usize, ConvertError> {
    let supported = match mapping_family {
        0 => (1..=2).contains(&channels),
        1 => (1..=8).contains(&channels),
        _ => false,
    };
    if !supported {
        return Err(ConvertError::UnsupportedChannelLayout {
            codec: "Opus",
            channels,
        });
    }
    Ok(channels)
}

/// Decodes either an ordinary mono/stereo Opus stream or a multistream
/// surround packet. Keeping both variants here means the file and streaming
/// paths share the same channel handling.
enum OpusPcmDecoder {
    Single(Box<opus_pure::OpusDecoder>),
    Multi(Box<opus_pure::OpusMSDecoder>),
}

impl OpusPcmDecoder {
    fn new(channels: usize, mapping_family: u8) -> Result<Self, ConvertError> {
        if mapping_family == 0 {
            opus_pure::OpusDecoder::new(48_000, channels)
                .map(|decoder| Self::Single(Box::new(decoder)))
                .map_err(|error| {
                    ConvertError::Audio(format!("could not start the Opus decoder: {error}"))
                })
        } else {
            opus_pure::OpusMSDecoder::new(48_000, channels, mapping_family)
                .map(|decoder| Self::Multi(Box::new(decoder)))
                .map_err(|error| {
                    ConvertError::Audio(format!("could not start the Opus decoder: {error}"))
                })
        }
    }

    fn decode(
        &mut self,
        packet: &[u8],
        frame_size: usize,
        block: &mut [f32],
    ) -> Result<usize, opus_pure::Error> {
        match self {
            Self::Single(decoder) => decoder.decode(packet, frame_size, block),
            Self::Multi(decoder) => decoder.decode(packet, frame_size, block),
        }
    }
}

/// Opus mapping family 1 uses the Vorbis channel order, which differs from the
/// WAV/film order the downmixer expects. Reorder to L, R, C, LFE, surrounds.
fn reorder_opus_channels(samples: &mut [f32], channels: usize) {
    let order: &[usize] = match channels {
        3 => &[0, 2, 1],
        5 => &[0, 2, 1, 3, 4],
        6 => &[0, 2, 1, 5, 3, 4],
        7 => &[0, 2, 1, 6, 5, 3, 4],
        8 => &[0, 2, 1, 7, 5, 6, 3, 4],
        _ => return,
    };
    let mut frame = [0.0_f32; 8];
    for chunk in samples.chunks_exact_mut(channels) {
        for (output, &source) in order.iter().enumerate() {
            frame[output] = chunk[source];
        }
        chunk.copy_from_slice(&frame[..channels]);
    }
}

/// The inverse of [`reorder_opus_channels`], used before multistream encoding.
fn reorder_film_to_opus_channels(samples: &mut [f32], channels: usize) {
    let order: &[usize] = match channels {
        3 => &[0, 2, 1],
        5 => &[0, 2, 1, 3, 4],
        6 => &[0, 2, 1, 4, 5, 3],
        7 => &[0, 2, 1, 5, 6, 4, 3],
        8 => &[0, 2, 1, 6, 7, 4, 5, 3],
        _ => return,
    };
    let mut frame = [0.0_f32; 8];
    for chunk in samples.chunks_exact_mut(channels) {
        for (output, &source) in order.iter().enumerate() {
            frame[output] = chunk[source];
        }
        chunk.copy_from_slice(&frame[..channels]);
    }
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
    let channels = opus_channels(head.channel_count as usize, head.mapping_family)?;
    let mut decoder = OpusPcmDecoder::new(channels, head.mapping_family)?;
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
        reorder_opus_channels(&mut block[..decoded * channels], channels);
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
) -> Result<DecodedAudio, ConvertError> {
    use opus_pure::MAX_PACKET_SAMPLES;

    let info = opus_stream_info(codec_params)?;
    let channels = info.channels;
    let mut decoder = OpusPcmDecoder::new(channels, info.mapping_family)?;
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
        reorder_opus_channels(&mut block[..decoded * channels], channels);
        append_audio_samples(&mut samples, &block[..decoded * channels])?;
    }

    let skip = (info.pre_skip as usize * channels).min(samples.len());
    samples.drain(..skip);

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

const EBML_INFO_ID: u32 = 0x1549_A966;

const EBML_TIMECODE_SCALE_ID: u32 = 0x002A_D7B1;

const EBML_TIMECODE_ID: u32 = 0xE7;

const EBML_SIMPLE_BLOCK_ID: u32 = 0xA3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MatroskaAudioTiming {
    discard_padding_ns: Option<u64>,
    last_block_timestamp_ns: Option<u64>,
}

impl MatroskaAudioTiming {
    fn to_trim(self) -> AudioTrim {
        let mut trim = AudioTrim::default();
        if let Some(padding_ns) = self.discard_padding_ns {
            trim.tail_padding = ns_to_frames(padding_ns, 48_000);
        } else if let Some(timestamp_ns) = self.last_block_timestamp_ns.filter(|value| *value > 0) {
            // Without DiscardPadding, the last block's timestamp is the only
            // per-track presentation bound left in a remux. Do not include the
            // final packet's decoder padding in the output.
            trim.limit_frames = Some(ns_to_frames(timestamp_ns, 48_000));
        }
        trim
    }
}

fn ns_to_frames(ns: u64, sample_rate: u32) -> usize {
    ((u128::from(ns) * u128::from(sample_rate) + 500_000_000) / 1_000_000_000) as usize
}

fn ebml_element_end(start: usize, size: Option<u64>, limit: usize) -> usize {
    size.and_then(|size| (start as u64).checked_add(size))
        .map(|value| value.min(limit as u64) as usize)
        .unwrap_or(limit)
}

fn read_ebml_uint(input: &[u8], pos: usize, size: u64) -> Option<u64> {
    if size > 8 {
        return None;
    }
    let bytes = input.get(pos..pos + size as usize)?;
    let mut value = 0_u64;
    for byte in bytes {
        value = value.checked_shl(8)? | u64::from(*byte);
    }
    Some(value)
}

fn matroska_block_track_and_time(input: &[u8], start: usize, size: u64) -> Option<(u64, i64)> {
    let end = start.checked_add(size as usize)?;
    if end > input.len() {
        return None;
    }
    let mut cursor = start;
    let (track, _) = read_ebml_vint(input, &mut cursor, 8, false)?;
    let relative = i16::from_be_bytes(input.get(cursor..cursor + 2)?.try_into().ok()?);
    Some((track, i64::from(relative)))
}

fn matroska_timecode_scale(input: &[u8]) -> Option<u64> {
    if !input.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return None;
    }
    let mut pos = 0;
    let (segment_size, segment_start) = loop {
        let (id, size) = read_ebml_element(input, &mut pos)?;
        if id == EBML_SEGMENT_ID {
            break (size, pos);
        }
        skip_ebml_element(input, &mut pos, size)?;
    };
    let segment_end = ebml_element_end(segment_start, segment_size, input.len());
    let mut scan = segment_start;
    while scan < segment_end {
        let (id, size) = read_ebml_element(input, &mut scan)?;
        let end = ebml_element_end(scan, size, segment_end);
        if id == EBML_INFO_ID {
            let mut info_pos = scan;
            while info_pos < end {
                let (child_id, child_size) = read_ebml_element(input, &mut info_pos)?;
                let child_end = ebml_element_end(info_pos, child_size, end);
                if child_id == EBML_TIMECODE_SCALE_ID {
                    if let Some(value) = child_size
                        .and_then(|size| read_ebml_uint(input, info_pos, size))
                        .filter(|value| *value > 0)
                    {
                        return Some(value);
                    }
                }
                info_pos = child_end;
            }
        }
        scan = end;
    }
    None
}

fn matroska_audio_timing_from_path(path: &Path, track_id: u32) -> MatroskaAudioTiming {
    const HEAD_BYTES: u64 = 4 * 1024 * 1024;
    const TAIL_BYTES: u64 = 8 * 1024 * 1024;
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return MatroskaAudioTiming::default(),
    };
    let length = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => return MatroskaAudioTiming::default(),
    };
    let head_length = length.min(HEAD_BYTES);
    let mut head = vec![0_u8; head_length as usize];
    if file.read_exact(&mut head).is_err() {
        return MatroskaAudioTiming::default();
    }
    let timecode_scale_ns = matroska_timecode_scale(&head).unwrap_or(1_000_000);

    let tail_length = length.min(TAIL_BYTES);
    if file.seek(SeekFrom::Start(length - tail_length)).is_err() {
        return MatroskaAudioTiming::default();
    }
    let mut tail = vec![0_u8; tail_length as usize];
    if file.read_exact(&mut tail).is_err() {
        return MatroskaAudioTiming::default();
    }

    // The final cluster starts within the bounded tail for normal Matroska
    // muxes. Search backwards so the first valid candidate is the latest one;
    // the ID can also occur in compressed block data, so candidates are only
    // accepted when parsing yields timing for the selected track.
    let cluster_id = EBML_CLUSTER_ID.to_be_bytes();
    if tail.len() >= cluster_id.len() {
        for position in (0..=tail.len() - cluster_id.len()).rev() {
            if tail[position..position + cluster_id.len()] != cluster_id {
                continue;
            }
            let mut cursor = position;
            let Some((id, size)) = read_ebml_element(&tail, &mut cursor) else {
                continue;
            };
            if id != EBML_CLUSTER_ID {
                continue;
            }
            let cluster_end = ebml_element_end(cursor, size, tail.len());
            let mut timing = MatroskaAudioTiming::default();
            if parse_matroska_cluster(
                &tail,
                cursor,
                cluster_end,
                timecode_scale_ns,
                track_id,
                &mut timing,
            ) && (timing.discard_padding_ns.is_some()
                || timing.last_block_timestamp_ns.is_some())
            {
                return timing;
            }
        }
    }
    MatroskaAudioTiming::default()
}

/// Reads per-track Opus timing from Matroska. `DiscardPadding` is the precise
/// end trim when a muxer kept it. A remux often drops it, leaving only the last
/// block timestamp; that timestamp still excludes the final packet's padding.
fn matroska_audio_timing(input: &[u8], track_id: u32) -> MatroskaAudioTiming {
    let mut timing = MatroskaAudioTiming::default();
    if !input.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return timing;
    }

    let mut pos = 0;
    let mut visited = 0usize;
    let (segment_size, segment_start) = loop {
        visited += 1;
        if visited > 1_000_000 {
            return timing;
        }
        let Some((id, size)) = read_ebml_element(input, &mut pos) else {
            return timing;
        };
        if id == EBML_SEGMENT_ID {
            break (size, pos);
        }
        if skip_ebml_element(input, &mut pos, size).is_none() {
            return timing;
        }
    };
    let segment_end = ebml_element_end(segment_start, segment_size, input.len());

    let mut timecode_scale_ns = 1_000_000_u64;
    let mut scan = segment_start;
    while scan < segment_end {
        let Some((id, size)) = read_ebml_element(input, &mut scan) else {
            break;
        };
        let end = ebml_element_end(scan, size, segment_end);
        if id == EBML_INFO_ID {
            let mut info_pos = scan;
            while info_pos < end {
                let Some((child_id, child_size)) = read_ebml_element(input, &mut info_pos) else {
                    break;
                };
                let child_end = child_size
                    .and_then(|size| (info_pos as u64).checked_add(size))
                    .map(|value| value.min(end as u64) as usize)
                    .unwrap_or(end);
                if child_id == EBML_TIMECODE_SCALE_ID {
                    if let Some(value) =
                        child_size.and_then(|size| read_ebml_uint(input, info_pos, size))
                    {
                        if value > 0 {
                            timecode_scale_ns = value;
                        }
                    }
                }
                info_pos = child_end;
            }
        }
        scan = end;
    }

    scan = segment_start;
    while scan < segment_end {
        visited += 1;
        if visited > 1_000_000 {
            return timing;
        }
        let Some((id, size)) = read_ebml_element(input, &mut scan) else {
            break;
        };
        let cluster_end = ebml_element_end(scan, size, segment_end);
        if id != EBML_CLUSTER_ID {
            scan = cluster_end;
            continue;
        }

        if !parse_matroska_cluster(
            input,
            scan,
            cluster_end,
            timecode_scale_ns,
            track_id,
            &mut timing,
        ) {
            return timing;
        }
        scan = cluster_end;
    }
    timing
}

fn parse_matroska_cluster(
    input: &[u8],
    cluster_start: usize,
    cluster_end: usize,
    timecode_scale_ns: u64,
    track_id: u32,
    timing: &mut MatroskaAudioTiming,
) -> bool {
    let mut cluster_pos = cluster_start;
    let mut cluster_time_ns = 0_u64;
    let mut visited = 0usize;
    while cluster_pos < cluster_end {
        visited += 1;
        if visited > 1_000_000 {
            return false;
        }
        let Some((child_id, child_size)) = read_ebml_element(input, &mut cluster_pos) else {
            break;
        };
        let child_end = ebml_element_end(cluster_pos, child_size, cluster_end);
        match child_id {
            EBML_TIMECODE_ID => {
                if let Some(value) =
                    child_size.and_then(|size| read_ebml_uint(input, cluster_pos, size))
                {
                    cluster_time_ns = value.saturating_mul(timecode_scale_ns);
                }
            }
            EBML_SIMPLE_BLOCK_ID => {
                if let Some((track, relative)) = child_size
                    .and_then(|size| matroska_block_track_and_time(input, cluster_pos, size))
                {
                    if track == u64::from(track_id) {
                        timing.last_block_timestamp_ns = Some(block_timestamp_ns(
                            cluster_time_ns,
                            relative,
                            timecode_scale_ns,
                        ));
                        timing.discard_padding_ns = None;
                    }
                }
            }
            EBML_BLOCK_GROUP_ID => {
                let mut group_pos = cluster_pos;
                let mut group_track = None;
                let mut group_relative = None;
                let mut group_padding = None;
                while group_pos < child_end {
                    let Some((group_child_id, group_child_size)) =
                        read_ebml_element(input, &mut group_pos)
                    else {
                        break;
                    };
                    let group_child_end = ebml_element_end(group_pos, group_child_size, child_end);
                    match group_child_id {
                        EBML_BLOCK_ID => {
                            if let Some((track, relative)) = group_child_size.and_then(|size| {
                                matroska_block_track_and_time(input, group_pos, size)
                            }) {
                                group_track = Some(track);
                                group_relative = Some(relative);
                            }
                        }
                        EBML_DISCARD_PADDING_ID => {
                            if let Some(value) = group_child_size
                                .and_then(|size| read_ebml_signed(input, group_pos, size))
                                .filter(|value| *value > 0)
                            {
                                group_padding = Some(value as u64);
                            }
                        }
                        _ => {}
                    }
                    group_pos = group_child_end;
                }
                if group_track == Some(u64::from(track_id)) {
                    if let Some(relative) = group_relative {
                        timing.last_block_timestamp_ns = Some(block_timestamp_ns(
                            cluster_time_ns,
                            relative,
                            timecode_scale_ns,
                        ));
                    }
                    timing.discard_padding_ns = group_padding;
                }
            }
            _ => {}
        }
        cluster_pos = child_end;
    }
    true
}

fn block_timestamp_ns(cluster_time_ns: u64, relative: i64, timecode_scale_ns: u64) -> u64 {
    let timestamp =
        i128::from(cluster_time_ns) + i128::from(relative) * i128::from(timecode_scale_ns);
    timestamp.max(0) as u64
}

#[cfg(test)]
fn matroska_discard_padding(input: &[u8], track_id: u32) -> Option<u64> {
    matroska_audio_timing(input, track_id).discard_padding_ns
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mp4Gapless {
    /// Samples to skip on the media timeline. This includes any codec delay
    /// that a container-level edit list covers.
    skip: usize,
    /// Presentation samples after the skip, when the container declares an
    /// exact length.
    limit: Option<usize>,
    /// Whether `limit` is a presentation length (edit list / `iTunSMPB`) rather
    /// than the raw media duration from `mdhd`.
    limit_is_presentation: bool,
}

/// Returns the MP4 priming skip and intended duration from an edit list (or
/// iTunes' `iTunSMPB` tag), both in media samples. If neither is present, the
/// track's own `mdhd` duration is used rather than the movie duration.
fn mp4_gapless_info(moov: &[u8], track_index: usize) -> Option<Mp4Gapless> {
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
    let mdhd = mp4_find(mdia, b"mdhd")?;
    let media_timescale = mp4_timescale(mdhd)?;
    let media_duration = mp4_media_duration(mdhd);

    if let Some(edts) = mp4_find(trak, b"edts") {
        if let Some(elst) = mp4_find(edts, b"elst") {
            if let Some((duration, media_time)) = mp4_media_edit(elst) {
                if media_time >= 0 && media_timescale > 0 && movie_timescale > 0 {
                    let total = (duration > 0).then(|| {
                        let scaled = u128::from(duration) * u128::from(media_timescale);
                        ((scaled + u128::from(movie_timescale) / 2) / u128::from(movie_timescale))
                            as usize
                    });
                    return Some(Mp4Gapless {
                        skip: media_time as usize,
                        limit: total,
                        limit_is_presentation: true,
                    });
                }
            }
        }
    }

    // iTunes stores the same numbers as hex text in a freeform tag. It is the
    // fallback for files whose muxer omitted the edit list.
    if let Some((skip, original)) = mp4_itunes_gapless_info(moov) {
        return Some(Mp4Gapless {
            skip,
            limit: (original > 0).then_some(original),
            limit_is_presentation: true,
        });
    }

    // No edit list means the track has no priming trim, but its media header
    // still gives the correct per-track duration. A movie-level duration is not
    // usable here: a short video track or a different movie timescale would
    // change the count.
    media_duration
        .filter(|duration| *duration > 0)
        .map(|duration| Mp4Gapless {
            skip: 0,
            limit: Some(duration),
            limit_is_presentation: false,
        })
}

fn mp4_media_edit(elst: &[u8]) -> Option<(u64, i64)> {
    let version = *elst.first()?;
    let count = u32::from_be_bytes(elst.get(4..8)?.try_into().ok()?) as usize;
    let entry_size = if version == 1 { 20 } else { 12 };
    for index in 0..count {
        let start = 8usize.checked_add(index.checked_mul(entry_size)?)?;
        let (duration, media_time) = if version == 1 {
            (
                u64::from_be_bytes(elst.get(start..start + 8)?.try_into().ok()?),
                i64::from_be_bytes(elst.get(start + 8..start + 16)?.try_into().ok()?),
            )
        } else {
            (
                u64::from(u32::from_be_bytes(
                    elst.get(start..start + 4)?.try_into().ok()?,
                )),
                i64::from(i32::from_be_bytes(
                    elst.get(start + 4..start + 8)?.try_into().ok()?,
                )),
            )
        };
        // Negative media times are empty edits (initial delay), not samples.
        if media_time >= 0 {
            return Some((duration, media_time));
        }
    }
    None
}

fn mp4_itunes_gapless_info(moov: &[u8]) -> Option<(usize, usize)> {
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
    Some((skip, original))
}

fn mp4_media_duration(mdhd: &[u8]) -> Option<usize> {
    let offset = match mdhd.first()? {
        0 => 16,
        1 => 24,
        _ => return None,
    };
    let duration = match mdhd.first()? {
        0 => u64::from(u32::from_be_bytes(
            mdhd.get(offset..offset + 4)?.try_into().ok()?,
        )),
        1 => u64::from_be_bytes(mdhd.get(offset..offset + 8)?.try_into().ok()?),
        _ => return None,
    };
    usize::try_from(duration).ok()
}

fn opus_stream_info(
    codec_params: &symphonia::core::codecs::CodecParameters,
) -> Result<OpusStreamInfo, ConvertError> {
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
    let mapping_family = head.map(|data| data[18]).unwrap_or(0);
    let channels = opus_channels(channels, mapping_family)?;
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
    Ok(OpusStreamInfo {
        channels,
        pre_skip,
        mapping_family,
    })
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
///
/// Layouts follow the WAV/film channel order: 1 = mono, 2 = L/R, 3 = L/R/C,
/// 4 = quad L/R/BL/BR, 5 = L/R/C/BL/BR, 6 = L/R/C/LFE/BL/BR,
/// 7 = 6.1 L/R/C/LFE/BC/SL/SR, and 8 = 7.1 L/R/C/LFE/BL/BR/SL/SR. Centre and
/// surround channels use 0.707; quad surrounds use 0.707 on their own side.
/// LFE is included at 0.5 rather than omitted: that keeps low-frequency energy
/// in the fold-down while limiting the headroom it consumes. Layouts above
/// eight channels are treated as 7.1 plus equally weighted extra channels.
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

/// Mono fold-down for the layouts documented on [`remix_channels`]. Front
/// left/right are averaged, centre uses 0.707, and LFE and surrounds use 0.5.
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
        _ => {
            // 7.1 plus any extra channels, which are split equally so they are
            // not silently dropped.
            0.5 * (channel(0) + channel(1))
                + 0.707 * channel(2)
                + 0.5 * channel(3)
                + 0.5 * frame.iter().skip(4).sum::<f32>()
        }
    }
}

/// Stereo fold-down for the layouts documented on [`remix_channels`].
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
        8 => {
            left += 0.707 * channel(2) + 0.5 * channel(3) + 0.707 * channel(4) + 0.707 * channel(6);
            right +=
                0.707 * channel(2) + 0.5 * channel(3) + 0.707 * channel(5) + 0.707 * channel(7);
        }
        _ => {
            // 7.1 plus any extra channels, which are split equally so they are
            // not silently dropped.
            left += 0.707 * channel(2) + 0.5 * channel(3) + 0.707 * channel(4) + 0.707 * channel(6);
            right +=
                0.707 * channel(2) + 0.5 * channel(3) + 0.707 * channel(5) + 0.707 * channel(7);
            for sample in frame.iter().skip(8) {
                left += 0.5 * sample;
                right += 0.5 * sample;
            }
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

fn id3v2_len(input: &[u8]) -> Option<usize> {
    if input.len() < 10 || &input[..3] != b"ID3" {
        return None;
    }
    let size = ((input[6] as usize & 0x7f) << 21)
        | ((input[7] as usize & 0x7f) << 14)
        | ((input[8] as usize & 0x7f) << 7)
        | (input[9] as usize & 0x7f);
    let footer = if input[5] & 0x10 != 0 { 10 } else { 0 };
    let end = 10 + size + footer;
    (end <= input.len()).then_some(end)
}

fn preserve_mp3_id3v2(input: &[u8], output: Vec<u8>) -> Vec<u8> {
    let Some(tag_len) = id3v2_len(input) else {
        return output;
    };
    let audio = strip_id3v2(&output);
    let mut result = Vec::with_capacity(tag_len + audio.len());
    result.extend_from_slice(&input[..tag_len]);
    result.extend_from_slice(audio);
    result
}

fn flac_metadata_end(input: &[u8]) -> Option<usize> {
    if !input.starts_with(b"fLaC") {
        return None;
    }
    let mut position = 4;
    loop {
        let header = input.get(position..position + 4)?;
        let last = header[0] & 0x80 != 0;
        let length =
            ((header[1] as usize) << 16) | ((header[2] as usize) << 8) | header[3] as usize;
        position = position.checked_add(4 + length)?;
        if last {
            return (position <= input.len()).then_some(position);
        }
    }
}

fn flac_metadata_blocks(input: &[u8]) -> Option<Vec<(u8, Vec<u8>)>> {
    let end = flac_metadata_end(input)?;
    let mut blocks = Vec::new();
    let mut position = 4;
    while position < end {
        let header = input.get(position..position + 4)?;
        let kind = header[0] & 0x7f;
        let length =
            ((header[1] as usize) << 16) | ((header[2] as usize) << 8) | header[3] as usize;
        let data = input.get(position + 4..position + 4 + length)?.to_vec();
        blocks.push((kind, data));
        position += 4 + length;
    }
    Some(blocks)
}

fn rebuild_flac(blocks: &[(u8, Vec<u8>)], audio: &[u8]) -> Vec<u8> {
    let mut output = b"fLaC".to_vec();
    for (index, (kind, data)) in blocks.iter().enumerate() {
        let last = index + 1 == blocks.len();
        output.push(if last { 0x80 } else { 0 } | kind);
        output.extend_from_slice(&(data.len() as u32).to_be_bytes()[1..]);
        output.extend_from_slice(data);
    }
    output.extend_from_slice(audio);
    output
}

/// Copies Vorbis comments and pictures from a FLAC input to FLAC output. The
/// encoder writes fresh audio metadata, so the descriptive blocks are replaced
/// rather than appended a second time.
fn preserve_flac_tags(input: &[u8], output: Vec<u8>) -> Vec<u8> {
    let input = strip_id3v2(input);
    let Some(input_blocks) = flac_metadata_blocks(input) else {
        return output;
    };
    let Some(mut output_blocks) = flac_metadata_blocks(&output) else {
        return output;
    };
    let tags: Vec<_> = input_blocks
        .into_iter()
        .filter(|(kind, data)| (*kind == 4 || *kind == 6) && data.len() <= 0xFF_FFFF)
        .collect();
    if tags.is_empty() {
        return output;
    }
    output_blocks.retain(|(kind, _)| *kind != 4 && *kind != 6);
    let insert_at = usize::from(output_blocks.first().is_some_and(|(kind, _)| *kind == 0));
    for (offset, tag) in tags.into_iter().enumerate() {
        output_blocks.insert(insert_at + offset, tag);
    }
    let Some(audio_start) = flac_metadata_end(&output) else {
        return output;
    };
    rebuild_flac(&output_blocks, &output[audio_start..])
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
            AudioBitDepth::Float32 => {
                samples_buffer.extend_from_slice(&(*sample).clamp(-1.0, 1.0).to_le_bytes())
            }
        }
        if samples_buffer.len() >= 32 * 1024 {
            writer.write_all(&samples_buffer)?;
            samples_buffer.clear();
        }
    }
    writer.write_all(&samples_buffer)?;
    Ok(())
}

/// Triangular-PDF dither for the reduction to 16-bit output. The generator is
/// deterministic so conversions are reproducible. Higher depths are quantized
/// without dither.
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
        // A source known to be 16-bit needs no dither when it is written back
        // at 16 bits. Deeper sources and f32-decoded sources with no reported
        // integer precision are reductions and are always dithered.
        if source_bits != Some(16) {
            let dither = self.next() / f32::from(i16::MAX);
            // Clamp before adding dither so samples at or beyond full scale
            // saturate identically in every decode path.
            sample_to_i16((sample.clamp(-1.0, 1.0) + dither).clamp(-1.0, 1.0))
        } else {
            sample_to_i16(sample)
        }
    }

    fn quantize_i32(&mut self, sample: f32, bits: u32, _source_bits: Option<u16>) -> i32 {
        let scale = ((1_i64 << (bits - 1)) - 1) as f32;
        (sample.clamp(-1.0, 1.0) * scale).round() as i32
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

enum OpusPcmEncoder {
    Single(Box<opus_pure::OpusEncoder>),
    Multi(Box<opus_pure::OpusMSEncoder>),
}

impl OpusPcmEncoder {
    fn new(sample_rate: u32, channels: usize) -> Result<Self, ConvertError> {
        use opus_pure::{Application, OpusEncoder, OpusMSEncoder};

        if channels <= 2 {
            let mut encoder = OpusEncoder::new(sample_rate as i32, channels, Application::Audio)
                .map_err(|error| {
                    ConvertError::Audio(format!("could not start the Opus encoder: {error}"))
                })?;
            encoder.bitrate_bps = if channels == 1 { 64_000 } else { 96_000 };
            Ok(Self::Single(Box::new(encoder)))
        } else if channels <= 8 {
            let mut encoder =
                OpusMSEncoder::new(sample_rate as i32, channels, 1, Application::Audio).map_err(
                    |error| {
                        ConvertError::Audio(format!("could not start the Opus encoder: {error}"))
                    },
                )?;
            encoder.set_bitrate(64_000 * channels as i32);
            Ok(Self::Multi(Box::new(encoder)))
        } else {
            Err(ConvertError::UnsupportedChannelLayout {
                codec: "Opus",
                channels,
            })
        }
    }

    fn head(&self, sample_rate: u32) -> opus_pure::OpusHead {
        use opus_pure::OpusHead;
        match self {
            Self::Single(encoder) => OpusHead::for_encoder(encoder, sample_rate),
            Self::Multi(encoder) => OpusHead::for_ms_encoder(encoder, sample_rate),
        }
    }

    fn encode(
        &mut self,
        block: &[f32],
        frame_size: usize,
        packet: &mut [u8],
    ) -> Result<usize, opus_pure::Error> {
        match self {
            Self::Single(encoder) => encoder.encode(block, frame_size, packet),
            Self::Multi(encoder) => encoder.encode(block, frame_size, packet),
        }
    }
}

fn encode_opus(mut audio: DecodedAudio) -> Result<Vec<u8>, ConvertError> {
    use opus_pure::{OggOpusWriter, MAX_PACKET_BYTES};

    if audio.channels == 0 || audio.sample_rate == 0 || audio.samples.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    let sample_rate = opus_sample_rate(audio.sample_rate);
    if audio.sample_rate != sample_rate {
        audio = resample_audio(audio, sample_rate)?;
    }

    let channels = audio.channels;
    let mut encoder = OpusPcmEncoder::new(sample_rate, channels)?;
    if channels > 2 {
        reorder_film_to_opus_channels(&mut audio.samples, channels);
    }
    let head = encoder.head(sample_rate);
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

fn mp3_lame_bitrate(bitrate: Mp3Bitrate) -> mp3lame_encoder::Bitrate {
    use mp3lame_encoder::Bitrate;
    match bitrate {
        Mp3Bitrate::Kbps128 => Bitrate::Kbps128,
        Mp3Bitrate::Kbps192 => Bitrate::Kbps192,
        Mp3Bitrate::Kbps256 => Bitrate::Kbps256,
        Mp3Bitrate::Kbps320 => Bitrate::Kbps320,
    }
}

fn encode_mp3(audio: DecodedAudio, bitrate: Mp3Bitrate) -> Result<Vec<u8>, ConvertError> {
    use mp3lame_encoder::{Builder, FlushNoGap, InterleavedPcm, MonoPcm, Quality};

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
        .and_then(|_| builder.set_brate(mp3_lame_bitrate(bitrate)))
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
            return Err(ConvertError::UnsupportedBitDepth {
                format: "FLAC",
                depth: bit_depth.label(),
            })
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
        for (channel, background) in pixel.0[..3].iter().zip(background) {
            rgb.push(composite_channel(*channel, pixel[3], background));
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

/// Extracts a ZIP, TAR, TAR.GZ or GZIP archive into independent files.
/// Entry names are normalized to safe relative paths; absolute paths and
/// parent-directory components are rejected instead of being written outside
/// the output directory.
pub fn extract_archive(input_name: &str, input: &[u8]) -> Result<Vec<ConvertedFile>, ConvertError> {
    let lower_name = input_name.to_ascii_lowercase();
    let mut outputs = Vec::new();
    let mut names = HashSet::new();
    let mut total_bytes = 0_u64;

    if lower_name.ends_with(".tar.gz") || lower_name.ends_with(".tgz") {
        let decoder = flate2::read::GzDecoder::new(Cursor::new(input));
        extract_tar_entries(decoder, &mut outputs, &mut names, &mut total_bytes)?;
    } else if lower_name.ends_with(".tar") {
        extract_tar_entries(
            Cursor::new(input),
            &mut outputs,
            &mut names,
            &mut total_bytes,
        )?;
    } else if lower_name.ends_with(".zip") {
        extract_zip_entries(input, &mut outputs, &mut names, &mut total_bytes)?;
    } else if lower_name.ends_with(".gz") {
        let name = Path::new(input_name)
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".gz"))
            .filter(|name| !name.is_empty())
            .unwrap_or("extracted");
        let mut decoder = flate2::read::GzDecoder::new(Cursor::new(input));
        let data = read_archive_entry(&mut decoder, MAX_ARCHIVE_ENTRY_BYTES)?;
        push_archive_entry(&mut outputs, &mut names, &mut total_bytes, name, data)?;
    } else {
        return Err(unsupported(
            &file_extension(input_name),
            OutputFormat::Files,
        ));
    }

    if outputs.is_empty() {
        return Err(ConvertError::NoArchiveEntries);
    }
    Ok(outputs)
}

fn extract_zip_entries(
    input: &[u8],
    outputs: &mut Vec<ConvertedFile>,
    names: &mut HashSet<PathBuf>,
    total_bytes: &mut u64,
) -> Result<(), ConvertError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(input))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(ConvertError::ArchiveTooManyEntries {
            count: archive.len(),
            max: MAX_ARCHIVE_ENTRIES,
        });
    }
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        let data = read_archive_entry(&mut entry, MAX_ARCHIVE_ENTRY_BYTES)?;
        push_archive_entry(outputs, names, total_bytes, &name, data)?;
    }
    Ok(())
}

fn extract_tar_entries<R: Read>(
    reader: R,
    outputs: &mut Vec<ConvertedFile>,
    names: &mut HashSet<PathBuf>,
    total_bytes: &mut u64,
) -> Result<(), ConvertError> {
    let mut archive = tar::Archive::new(reader);
    let mut count = 0_usize;
    for entry in archive.entries()? {
        let mut entry = entry?;
        count += 1;
        if count > MAX_ARCHIVE_ENTRIES {
            return Err(ConvertError::ArchiveTooManyEntries {
                count,
                max: MAX_ARCHIVE_ENTRIES,
            });
        }
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.to_string_lossy().into_owned();
        let data = read_archive_entry(&mut entry, MAX_ARCHIVE_ENTRY_BYTES)?;
        push_archive_entry(outputs, names, total_bytes, &path, data)?;
    }
    Ok(())
}

fn read_archive_entry<R: Read>(reader: &mut R, limit: u64) -> Result<Vec<u8>, ConvertError> {
    let mut data = Vec::new();
    reader.take(limit + 1).read_to_end(&mut data)?;
    if data.len() as u64 > limit {
        return Err(ConvertError::ArchiveTooLarge {
            max: MAX_ARCHIVE_BYTES,
        });
    }
    Ok(data)
}

fn push_archive_entry(
    outputs: &mut Vec<ConvertedFile>,
    names: &mut HashSet<PathBuf>,
    total_bytes: &mut u64,
    name: &str,
    data: Vec<u8>,
) -> Result<(), ConvertError> {
    let path = safe_archive_path(name)?;
    if !names.insert(path.clone()) {
        return Err(ConvertError::DuplicateArchiveEntryName(
            path.display().to_string(),
        ));
    }
    *total_bytes = total_bytes.saturating_add(data.len() as u64);
    if *total_bytes > MAX_ARCHIVE_BYTES {
        return Err(ConvertError::ArchiveTooLarge {
            max: MAX_ARCHIVE_BYTES,
        });
    }
    outputs.push(ConvertedFile {
        name: path,
        bytes: data,
    });
    Ok(())
}

fn safe_archive_path(name: &str) -> Result<PathBuf, ConvertError> {
    let normalized = name.replace('\\', "/");
    if normalized.is_empty() || normalized.contains('\0') || normalized.starts_with('/') {
        return Err(ConvertError::UnsafeArchivePath(name.to_owned()));
    }
    let mut safe = PathBuf::new();
    for part in normalized.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains(':') {
            return Err(ConvertError::UnsafeArchivePath(name.to_owned()));
        }
        safe.push(part);
    }
    if safe.as_os_str().is_empty() {
        return Err(ConvertError::UnsafeArchivePath(name.to_owned()));
    }
    Ok(safe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::{OsStr, OsString},
        io::Read,
        sync::{Mutex, OnceLock},
    };

    static VIDEO_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn video_testdata(name: &str) -> Vec<u8> {
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("testdata")
                .join(name),
        )
        .unwrap()
    }

    fn real_ffmpeg() -> Option<PathBuf> {
        let path = VideoToolConfig::default().resolve()?;
        let output = std::process::Command::new(&path)
            .arg("-version")
            .output()
            .ok()?;
        let version = String::from_utf8_lossy(&output.stdout);
        (output.status.success() && version.contains("ffmpeg version")).then_some(path)
    }

    fn generate_real_video(ffmpeg: &Path, path: &Path) {
        let output = std::process::Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=10:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:v",
                "libvpx-vp9",
                "-b:v",
                "300k",
                "-c:a",
                "libopus",
                "-shortest",
            ])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "ffmpeg could not generate the synthetic sample: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn ffprobe_for(ffmpeg: &Path) -> Option<PathBuf> {
        let name = if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        };
        let path = ffmpeg.with_file_name(name);
        path.is_file().then_some(path)
    }

    fn ffmpeg_stub() -> &'static Path {
        static STUB: OnceLock<PathBuf> = OnceLock::new();
        STUB.get_or_init(|| {
            let directory =
                std::env::temp_dir().join(format!("convert2it-ffmpeg-stub-{}", std::process::id()));
            std::fs::create_dir_all(&directory).unwrap();
            let source = directory.join("ffmpeg_stub.rs");
            std::fs::write(
                &source,
                r#"
use std::{env, fs, io::{self, Write}, process, thread, time::Duration};
fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if let Ok(log) = env::var("STUB_LOG") {
        let _ = fs::write(log, args.join("\n"));
    }
    if let Ok(code) = env::var("STUB_EXIT") {
        if let Ok(stderr) = env::var("STUB_STDERR") {
            eprintln!("{stderr}");
        }
        process::exit(code.parse().unwrap_or(1));
    }
    if let Ok(duration) = env::var("STUB_DURATION") {
        eprintln!("Duration: {duration}");
    }
    if let Ok(progress) = env::var("STUB_PROGRESS") {
        for line in progress.split('|') {
            println!("{line}");
        }
    }
    if let Ok(milliseconds) = env::var("STUB_SLEEP_MS") {
        thread::sleep(Duration::from_millis(milliseconds.parse().unwrap_or(0)));
    }
    if args.iter().any(|arg| arg == "image2pipe") {
        let path = env::var("STUB_PNG").unwrap();
        io::stdout().write_all(&fs::read(path).unwrap()).unwrap();
        return;
    }
    if let Some(output) = args.last() {
        if env::var_os("STUB_EMPTY").is_some() {
            let _ = fs::write(output, b"");
        } else if let Ok(prepared) = env::var("STUB_OUTPUT") {
            fs::copy(prepared, output).unwrap();
        }
    }
}
"#,
            )
            .unwrap();
            let executable = directory.join(if cfg!(windows) {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            });
            let status = std::process::Command::new("rustc")
                .arg("--edition=2021")
                .arg(&source)
                .arg("-o")
                .arg(&executable)
                .status()
                .expect("rustc must be available to build the ffmpeg test stub");
            assert!(status.success(), "could not build the ffmpeg test stub");
            executable
        })
    }

    struct EnvVars(Vec<(&'static str, Option<OsString>)>);

    impl EnvVars {
        fn new() -> Self {
            Self(Vec::new())
        }

        fn set(&mut self, key: &'static str, value: impl AsRef<OsStr>) -> &mut Self {
            self.0.push((key, std::env::var_os(key)));
            std::env::set_var(key, value);
            self
        }

        fn remove(&mut self, key: &'static str) -> &mut Self {
            self.0.push((key, std::env::var_os(key)));
            std::env::remove_var(key);
            self
        }
    }

    impl Drop for EnvVars {
        fn drop(&mut self) {
            for (key, value) in self.0.drain(..).rev() {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

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
    fn extracts_zip_tar_and_gzip_archives() {
        let zip_bytes = {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            writer.start_file("nested/one.txt", options).unwrap();
            writer.write_all(b"one").unwrap();
            writer.start_file("two.txt", options).unwrap();
            writer.write_all(b"two").unwrap();
            writer.finish().unwrap().into_inner()
        };
        let files = convert_bytes("bundle.zip", &zip_bytes, OutputFormat::Files).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, PathBuf::from("nested/one.txt"));
        assert_eq!(files[0].bytes, b"one");
        assert_eq!(files[1].name, PathBuf::from("two.txt"));
        assert_eq!(files[1].bytes, b"two");

        let tar_bytes = {
            let mut builder = tar::Builder::new(Vec::new());
            for (name, content) in [("nested/one.txt", &b"one"[..]), ("two.txt", &b"two"[..])] {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, name, content).unwrap();
            }
            builder.into_inner().unwrap()
        };
        let files = convert_bytes("bundle.tar", &tar_bytes, OutputFormat::Files).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, PathBuf::from("nested/one.txt"));
        assert_eq!(files[0].bytes, b"one");

        let gzip = convert_bytes("note.txt", b"hello", OutputFormat::Gzip).unwrap();
        let files = convert_bytes("note.txt.gz", &gzip[0].bytes, OutputFormat::Files).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, PathBuf::from("note.txt"));
        assert_eq!(files[0].bytes, b"hello");
    }

    #[test]
    fn rejects_archive_paths_that_escape_the_output_directory() {
        let tar_bytes = {
            let mut builder = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_path("safe.txt").unwrap();
            header.as_mut_bytes()[..11].copy_from_slice(b"../evil.txt");
            header.set_size(3);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, &b"bad"[..]).unwrap();
            builder.into_inner().unwrap()
        };
        assert!(matches!(
            convert_bytes("bundle.tar", &tar_bytes, OutputFormat::Files),
            Err(ConvertError::UnsafeArchivePath(_))
        ));

        for name in [
            "../evil.txt",
            "nested/../../evil.txt",
            "/absolute.txt",
            "C:\\evil.txt",
        ] {
            let zip_bytes = {
                let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
                let options = zip::write::SimpleFileOptions::default();
                writer.start_file(name, options).unwrap();
                writer.write_all(b"bad").unwrap();
                writer.finish().unwrap().into_inner()
            };
            let error = convert_bytes("bundle.zip", &zip_bytes, OutputFormat::Files).unwrap_err();
            assert!(
                matches!(error, ConvertError::UnsafeArchivePath(_)),
                "{name} produced {error}"
            );
        }
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
    fn video_thumbnail_uses_the_ffmpeg_argument_pipeline() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-thumb-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let png = directory.join("thumb.png");
        let log = directory.join("args.log");
        std::fs::write(&png, png_fixture()).unwrap();
        let mut env = EnvVars::new();
        env.set("STUB_LOG", &log)
            .set("STUB_PNG", &png)
            .remove("STUB_EXIT")
            .remove("STUB_EMPTY");

        let result = convert_bytes_with_video_config(
            "movie.mp4",
            &video_testdata("sample-video.mp4"),
            OutputFormat::Png,
            ConversionOptions {
                video_frame_time_ms: 1_500,
                video_thumbnail_width: 640,
                ..Default::default()
            },
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
        )
        .unwrap();
        assert_eq!(result[0].name, PathBuf::from("movie.png"));
        assert_eq!(
            image::load_from_memory(&result[0].bytes).unwrap().width(),
            2
        );

        let args = std::fs::read_to_string(&log).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert!(args.windows(2).any(|pair| pair == ["-ss", "1.500"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-vf", "scale=640:-2:flags=lanczos"]));
        assert!(args.contains(&"image2pipe"));
        assert!(args.windows(2).any(|pair| pair == ["-frames:v", "1"]));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn remux_copies_streams_and_rejects_empty_ffmpeg_output() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-remux-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let prepared = directory.join("prepared.webm");
        let log = directory.join("args.log");
        let prepared_bytes = b"\x1aE\xdf\xa3stub-webm".to_vec();
        std::fs::write(&prepared, &prepared_bytes).unwrap();
        let mut env = EnvVars::new();
        env.set("STUB_LOG", &log)
            .set("STUB_OUTPUT", &prepared)
            .remove("STUB_EXIT")
            .remove("STUB_EMPTY");

        let result = convert_bytes_with_video_config(
            "sample-vp9-opus.mp4",
            &video_testdata("sample-vp9-opus.mp4"),
            OutputFormat::Webm,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
        )
        .unwrap();
        assert_eq!(result[0].name, PathBuf::from("sample-vp9-opus.webm"));
        assert_eq!(result[0].bytes, prepared_bytes);
        let args = std::fs::read_to_string(&log).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert!(args.windows(2).any(|pair| pair == ["-c", "copy"]));
        assert!(args.windows(2).any(|pair| pair == ["-map", "0:v?"]));
        assert!(args.windows(2).any(|pair| pair == ["-map", "0:a?"]));
        assert!(args.windows(2).any(|pair| pair == ["-f", "webm"]));

        env.set("STUB_EMPTY", "1");
        let error = convert_bytes_with_video_config(
            "sample-vp9-opus.webm",
            &video_testdata("sample-vp9-opus.webm"),
            OutputFormat::Mp4,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
        )
        .unwrap_err();
        assert!(matches!(error, ConvertError::FfmpegEmptyOutput));

        let invalid = directory.join("invalid.webm");
        std::fs::write(&invalid, b"not-a-container").unwrap();
        env.remove("STUB_EMPTY").set("STUB_OUTPUT", &invalid);
        let error = convert_bytes_with_video_config(
            "sample-vp9-opus.mp4",
            &video_testdata("sample-vp9-opus.mp4"),
            OutputFormat::Webm,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::FfmpegInvalidOutput { container: "webm" }
        ));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn remux_surfaces_ffmpeg_stderr_and_missing_binary() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-failure-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut env = EnvVars::new();
        env.set("STUB_EXIT", "7")
            .set("STUB_STDERR", "stub failure details")
            .remove("STUB_EMPTY");

        let error = convert_bytes_with_video_config(
            "sample-vp9-opus.mp4",
            &video_testdata("sample-vp9-opus.mp4"),
            OutputFormat::Webm,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
        )
        .unwrap_err();
        match error {
            ConvertError::FfmpegFailed { status, stderr } => {
                assert_eq!(status, "7");
                assert!(stderr.contains("stub failure details"), "{stderr}");
            }
            other => panic!("expected FfmpegFailed, got {other}"),
        }

        let missing = directory.join("does-not-exist");
        let error = convert_bytes_with_video_config(
            "sample-vp9-opus.mp4",
            &video_testdata("sample-vp9-opus.mp4"),
            OutputFormat::Webm,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(missing),
        )
        .unwrap_err();
        assert!(matches!(error, ConvertError::FfmpegNotFound { .. }));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn ffmpeg_is_discovered_on_path_and_incompatible_codecs_are_rejected() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-path-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let prepared = directory.join("prepared.mp4");
        let log = directory.join("args.log");
        std::fs::write(&prepared, b"\0\0\0\x18ftypstub").unwrap();
        let stub_directory = ffmpeg_stub().parent().unwrap();
        let mut env = EnvVars::new();
        env.set("PATH", stub_directory)
            .set("STUB_LOG", &log)
            .set("STUB_OUTPUT", &prepared)
            .remove("CONVERT2IT_FFMPEG")
            .remove("FFMPEG")
            .remove("STUB_EXIT")
            .remove("STUB_EMPTY");

        let result = convert_bytes_with_video_config(
            "sample-vp9-opus.webm",
            &video_testdata("sample-vp9-opus.webm"),
            OutputFormat::Mp4,
            ConversionOptions::default(),
            &VideoToolConfig::default(),
        )
        .unwrap();
        assert_eq!(result[0].bytes, b"\0\0\0\x18ftypstub");

        let error = validate_remux(
            &video_testdata("sample-video.mp4"),
            "mp4",
            OutputFormat::Webm,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::UnsupportedVideoCodec {
                stream: "video",
                codec,
                container: "webm"
            } if codec == "h264"
        ));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn incompatible_stream_copy_combinations_return_a_typed_error() {
        let missing_ffmpeg = std::env::temp_dir().join("convert2it-definitely-missing-ffmpeg");
        for name in ["sample-even-h264.mp4", "sample-multitrack-h264.mp4"] {
            let error = convert_bytes_with_video_config(
                name,
                &video_testdata(name),
                OutputFormat::Webm,
                ConversionOptions::default(),
                &VideoToolConfig::with_ffmpeg(&missing_ffmpeg),
            )
            .unwrap_err();
            match error {
                ConvertError::UnsupportedVideoCodec {
                    stream: "video",
                    codec,
                    container: "webm",
                } => assert_eq!(codec, "h264", "{name}"),
                other => panic!("{name}: expected an unsupported-codec error, got {other:?}"),
            }
        }
    }

    #[test]
    fn h264_mp4_to_mp4_remuxes_with_stream_copy() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-mp4-remux-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let prepared = directory.join("prepared.mp4");
        let log = directory.join("args.log");
        std::fs::write(&prepared, b"\0\0\0\x18ftypstub").unwrap();
        let mut env = EnvVars::new();
        env.set("STUB_LOG", &log)
            .set("STUB_OUTPUT", &prepared)
            .remove("STUB_EXIT")
            .remove("STUB_EMPTY");

        let result = convert_bytes_with_video_config(
            "sample-odd-h264.mp4",
            &video_testdata("sample-odd-h264.mp4"),
            OutputFormat::Mp4,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
        )
        .unwrap();
        assert_eq!(result[0].name, PathBuf::from("sample-odd-h264.mp4"));
        let args = std::fs::read_to_string(&log).unwrap();
        assert!(args.contains("-c\ncopy"), "{args}");
        assert!(args.contains("-map\n0:v?"), "{args}");
        assert!(args.contains("-map\n0:a?"), "{args}");
        assert!(!args.contains("libvpx"), "{args}");
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn real_ffmpeg_generated_media_remux_and_thumbnails_when_available() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let Some(ffmpeg) = real_ffmpeg() else {
            eprintln!("skipping real-ffmpeg library test: ffmpeg is not available");
            return;
        };
        let directory =
            std::env::temp_dir().join(format!("convert2it-real-video-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let input_path = directory.join("synthetic.mp4");
        generate_real_video(&ffmpeg, &input_path);
        let input = std::fs::read(&input_path).unwrap();
        let config = VideoToolConfig::with_ffmpeg(&ffmpeg);

        let webm = convert_bytes_with_video_config(
            "synthetic.mp4",
            &input,
            OutputFormat::Webm,
            ConversionOptions::default(),
            &config,
        )
        .unwrap();
        assert_eq!(webm[0].name, PathBuf::from("synthetic.webm"));
        assert!(!webm[0].bytes.is_empty());
        assert!(webm[0].bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]));
        let webm_path = directory.join("synthetic.webm");
        std::fs::write(&webm_path, &webm[0].bytes).unwrap();
        let ffprobe = ffprobe_for(&ffmpeg);
        let mut webm_streams = None;
        if let Some(ffprobe) = ffprobe.as_ref() {
            let output = std::process::Command::new(ffprobe)
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "format=format_name:stream=codec_name,codec_type,width,height,channels",
                    "-of",
                    "default=noprint_wrappers=1",
                ])
                .arg(&webm_path)
                .output()
                .unwrap();
            assert!(output.status.success());
            let probe = String::from_utf8_lossy(&output.stdout);
            assert!(probe.contains("format_name=matroska,webm"), "{probe}");
            assert!(probe.contains("codec_name=vp9"), "{probe}");
            assert!(probe.contains("codec_name=opus"), "{probe}");
            assert!(probe.contains("width=320"), "{probe}");
            assert!(probe.contains("height=240"), "{probe}");
            let streams = probe
                .lines()
                .filter(|line| !line.starts_with("format_name="))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(streams.contains("codec_type=video"), "{streams}");
            assert!(streams.contains("codec_type=audio"), "{streams}");
            webm_streams = Some(streams);
        }

        let back = convert_bytes_with_video_config(
            "synthetic.webm",
            &webm[0].bytes,
            OutputFormat::Mp4,
            ConversionOptions::default(),
            &config,
        )
        .unwrap();
        assert_eq!(back[0].name, PathBuf::from("synthetic.mp4"));
        assert_eq!(back[0].bytes.get(4..8), Some(b"ftyp".as_slice()));
        let back_path = directory.join("synthetic-roundtrip.mp4");
        std::fs::write(&back_path, &back[0].bytes).unwrap();
        if let Some(ffprobe) = ffprobe.as_ref() {
            let output = std::process::Command::new(ffprobe)
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "format=format_name:stream=codec_name,codec_type,width,height,channels",
                    "-of",
                    "default=noprint_wrappers=1",
                ])
                .arg(&back_path)
                .output()
                .unwrap();
            assert!(output.status.success());
            let probe = String::from_utf8_lossy(&output.stdout);
            assert!(
                probe.contains("format_name=") && probe.contains("mp4"),
                "{probe}"
            );
            assert!(probe.contains("codec_name=vp9"), "{probe}");
            assert!(probe.contains("codec_name=opus"), "{probe}");
            assert!(probe.contains("width=320"), "{probe}");
            assert!(probe.contains("height=240"), "{probe}");
            let streams = probe
                .lines()
                .filter(|line| !line.starts_with("format_name="))
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(webm_streams) = webm_streams.as_deref() {
                assert_eq!(webm_streams, streams);
            }
        }

        let thumbnails = [
            (OutputFormat::Png, image::ImageFormat::Png),
            (OutputFormat::Jpeg, image::ImageFormat::Jpeg),
            (OutputFormat::Webp, image::ImageFormat::WebP),
        ];
        for (target, format) in thumbnails {
            let thumbnail = convert_bytes_with_video_config(
                "synthetic.mp4",
                &input,
                target,
                ConversionOptions {
                    video_frame_time_ms: 500,
                    video_thumbnail_width: 160,
                    ..Default::default()
                },
                &config,
            )
            .unwrap();
            assert!(!thumbnail[0].bytes.is_empty());
            assert_eq!(image::guess_format(&thumbnail[0].bytes).unwrap(), format);
            let decoded = image::load_from_memory(&thumbnail[0].bytes).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (160, 120));
        }

        let at_zero = convert_bytes_with_video_config(
            "synthetic.mp4",
            &input,
            OutputFormat::Png,
            ConversionOptions {
                video_frame_time_ms: 0,
                video_thumbnail_width: 160,
                ..Default::default()
            },
            &config,
        )
        .unwrap();
        let at_half_second = convert_bytes_with_video_config(
            "synthetic.mp4",
            &input,
            OutputFormat::Png,
            ConversionOptions {
                video_frame_time_ms: 500,
                video_thumbnail_width: 160,
                ..Default::default()
            },
            &config,
        )
        .unwrap();
        assert_ne!(at_zero[0].bytes, at_half_second[0].bytes);

        let error = convert_bytes_with_video_config(
            "synthetic.mp4",
            &input,
            OutputFormat::Png,
            ConversionOptions {
                video_frame_time_ms: 10_000,
                video_thumbnail_width: 160,
                ..Default::default()
            },
            &config,
        )
        .unwrap_err();
        assert!(matches!(error, ConvertError::FfmpegEmptyOutput));
        assert_eq!(error.to_string(), "ffmpeg produced no output");

        let error = convert_bytes_with_video_config(
            "broken.mp4",
            b"not a movie",
            OutputFormat::Webm,
            ConversionOptions::default(),
            &config,
        )
        .unwrap_err();
        match error {
            ConvertError::FfmpegFailed { status, stderr } => {
                assert_ne!(status, "0");
                assert!(!stderr.is_empty());
            }
            other => panic!("expected a real ffmpeg failure, got {other}"),
        }

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn reports_unsupported_format_pairs() {
        let error = convert_bytes("movie.mp4", b"not a movie", OutputFormat::Pdf).unwrap_err();
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
        wav_tone_at(sample_rate, frames, 2, 440.0, 0.5)
    }

    fn wav_tone_at(
        sample_rate: u32,
        frames: u32,
        channels: u16,
        frequency: f64,
        amplitude: f64,
    ) -> Vec<u8> {
        let mut samples = Vec::with_capacity(frames as usize * usize::from(channels));
        for frame in 0..frames {
            let value =
                ((frame as f64 / f64::from(sample_rate) * frequency * std::f64::consts::TAU).sin()
                    * amplitude
                    * f64::from(i16::MAX)) as i16;
            for _ in 0..channels {
                samples.push(value);
            }
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

    /// Builds a WAV whose channels carry constant, independently chosen levels.
    /// Constant samples make the downmix coefficients observable directly in
    /// the converted output.
    fn wav_channels_at(sample_rate: u32, frames: usize, channels: &[f32]) -> Vec<u8> {
        let channel_count = u16::try_from(channels.len()).unwrap();
        let mut samples = Vec::with_capacity(frames * channels.len());
        for _ in 0..frames {
            for &value in channels {
                samples.push((value.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16);
            }
        }

        let data_size = u32::try_from(samples.len() * 2).unwrap();
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_size).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&channel_count.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * u32::from(channel_count) * 2).to_le_bytes());
        wav.extend_from_slice(&(channel_count * 2).to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_size.to_le_bytes());
        for sample in samples {
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }

    /// Builds a 24-bit PCM WAV from interleaved signed 24-bit samples. This
    /// lets the lossless-depth tests start from detail a 16-bit source cannot
    /// carry.
    fn wav_24_bit_at(sample_rate: u32, channels: u16, samples: &[i32]) -> Vec<u8> {
        assert!(channels > 0, "a WAV needs at least one channel");
        assert_eq!(
            samples.len() % usize::from(channels),
            0,
            "the sample count is not a whole number of frames"
        );
        let data_size = u32::try_from(samples.len() * 3).unwrap();
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_size).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&(sample_rate * u32::from(channels) * 3).to_le_bytes());
        wav.extend_from_slice(&(channels * 3).to_le_bytes());
        wav.extend_from_slice(&24_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_size.to_le_bytes());
        for sample in samples {
            wav.extend_from_slice(&sample.to_le_bytes()[..3]);
        }
        wav
    }

    fn wav_24_bit_samples(bytes: &[u8]) -> Vec<i32> {
        let (format, _, _, bits, offset, size) = wav_layout(bytes);
        assert_eq!((format, bits), (1, 24));
        bytes[offset..offset + size]
            .as_chunks::<3>()
            .0
            .iter()
            .map(|bytes| {
                let sign = if bytes[2] & 0x80 == 0 { 0 } else { 0xFF };
                i32::from_le_bytes([bytes[0], bytes[1], bytes[2], sign])
            })
            .collect()
    }

    fn wav_float_samples(bytes: &[u8]) -> Vec<f32> {
        let (format, _, _, bits, offset, size) = wav_layout(bytes);
        assert_eq!((format, bits), (3, 32));
        bytes[offset..offset + size]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| f32::from_le_bytes(*bytes))
            .collect()
    }

    fn flac_24_bit_samples(bytes: &[u8]) -> Vec<i32> {
        let mut reader = claxon::FlacReader::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(reader.streaminfo().bits_per_sample, 24);
        reader.samples().map(Result::unwrap).collect()
    }

    fn flac_16_bit_samples(bytes: &[u8]) -> Vec<i32> {
        let mut reader = claxon::FlacReader::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(reader.streaminfo().bits_per_sample, 16);
        reader.samples().map(Result::unwrap).collect()
    }

    /// The deterministic 16-bit truncation the old bypass produced: no dither,
    /// just the same f32 samples rounded and clamped.
    fn undithered_i16(samples: &[f32]) -> Vec<i16> {
        samples
            .iter()
            .map(|&sample| sample_to_i16(sample))
            .collect()
    }

    /// Encodes a 5.1 Ogg Opus stream with the Vorbis channel order the Opus
    /// mapping family expects: FL, FC, FR, RL, RR, LFE.
    fn opus_surround_fixture(channels: [f32; 6]) -> Vec<u8> {
        use opus_pure::{Application, OggOpusWriter, OpusHead, OpusMSEncoder, MAX_PACKET_BYTES};

        let frame_size = 960;
        let mut encoder = OpusMSEncoder::new(48_000, 6, 1, Application::Audio).unwrap();
        let head = OpusHead::for_ms_encoder(&encoder, 48_000);
        let mut writer = OggOpusWriter::new(Vec::new(), head).unwrap();
        let mut block = vec![0.0_f32; frame_size * 6];
        let mut packet = vec![0_u8; MAX_PACKET_BYTES];
        for _ in 0..12 {
            for frame in 0..frame_size {
                block[frame * 6..(frame + 1) * 6].copy_from_slice(&channels);
            }
            let len = encoder.encode(&block, frame_size, &mut packet).unwrap();
            writer.write_packet(&packet[..len]).unwrap();
        }
        writer.finish().unwrap()
    }

    fn mp3_first_frame_bitrate(bytes: &[u8]) -> Option<u16> {
        let mut position = 0;
        if bytes.starts_with(b"ID3") && bytes.len() >= 10 {
            let size = (usize::from(bytes[6] & 0x7F) << 21)
                | (usize::from(bytes[7] & 0x7F) << 14)
                | (usize::from(bytes[8] & 0x7F) << 7)
                | usize::from(bytes[9] & 0x7F);
            position = 10 + size;
        }
        let mpeg1 = [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
        ];
        let mpeg2 = [
            0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
        ];
        while position + 4 <= bytes.len() {
            let header = &bytes[position..position + 4];
            let version = (header[1] >> 3) & 0x03;
            let layer = (header[1] >> 1) & 0x03;
            let bitrate_index = usize::from((header[2] >> 4) & 0x0F);
            if header[0] == 0xFF && (header[1] & 0xE0) == 0xE0 && layer == 1 {
                let table: &[u16; 16] = if version == 3 { &mpeg1 } else { &mpeg2 };
                if bitrate_index > 0 && bitrate_index < 15 {
                    return Some(table[bitrate_index]);
                }
            }
            position += 1;
        }
        None
    }

    fn assert_mp3_tone(
        mp3: &[u8],
        sample_rate: u32,
        channels: u16,
        expected_frames: usize,
        expected_bitrate: Mp3Bitrate,
    ) {
        assert_eq!(
            mp3_first_frame_bitrate(mp3),
            Some(expected_bitrate.kbps()),
            "the MP3 frame header does not declare {} kbps",
            expected_bitrate.kbps()
        );
        let decoded = super::convert_bytes("tone.mp3", mp3, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&decoded[0].bytes);
        assert_eq!(parsed.sample_rate, sample_rate);
        assert_eq!(parsed.channels, channels);
        let frames = parsed.samples.len() / usize::from(channels);
        let delta = (frames as i64 - expected_frames as i64).abs();
        assert!(
            delta <= 1_152,
            "decoded {frames} frames instead of {expected_frames} (delta {delta})"
        );

        let first_channel: Vec<i16> = parsed
            .samples
            .iter()
            .step_by(usize::from(channels))
            .copied()
            .collect();
        // MP3 encoders add a short delay and padding. Measure the middle half
        // of the signal so those edges do not skew the tone metrics.
        let window = &first_channel[frames / 4..frames * 3 / 4];
        let crossings = window
            .windows(2)
            .filter(|pair| (pair[0] < 0) != (pair[1] < 0))
            .count();
        let measured = crossings as f64 * f64::from(sample_rate) / (2.0 * window.len() as f64);
        assert!(
            (measured - 1_000.0).abs() < 20.0,
            "the decoded tone is at {measured:.1} Hz"
        );
        let rms = (window
            .iter()
            .map(|sample| f64::from(*sample) * f64::from(*sample))
            .sum::<f64>()
            / window.len() as f64)
            .sqrt();
        assert!(
            (rms - 11_585.0).abs() < 2_000.0,
            "the decoded tone RMS is {rms:.1}"
        );
        assert_audible(&parsed.samples);
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

    fn decoded_tone_frequency(parsed: &ParsedWav) -> f64 {
        let channels = usize::from(parsed.channels);
        let frames = parsed.samples.len() / channels;
        assert!(frames > 4, "not enough samples to measure a tone");
        let start = frames / 4 * channels;
        let end = frames * 3 / 4 * channels;
        let first_channel: Vec<i16> = parsed.samples[start..end]
            .iter()
            .step_by(channels)
            .copied()
            .collect();
        let crossings = first_channel
            .windows(2)
            .filter(|pair| (pair[0] < 0) != (pair[1] < 0))
            .count();
        crossings as f64 * f64::from(parsed.sample_rate) / (2.0 * first_channel.len() as f64)
    }

    fn decoded_rms(parsed: &ParsedWav) -> f64 {
        (parsed
            .samples
            .iter()
            .map(|sample| f64::from(*sample) * f64::from(*sample))
            .sum::<f64>()
            / parsed.samples.len() as f64)
            .sqrt()
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
        for (name, expected) in [
            // The source WebM carries 13.5 ms of DiscardPadding after the
            // 2 s tone, so the precise trim is 96,000 samples.
            ("sample-vp9-opus.webm", 96_000),
            // The MP4 edit list starts 336 samples into the media and presents
            // 2,001 ms. The Opus header's 312-sample pre-skip is already
            // removed by the decoder, leaving a 24-sample additional skip.
            ("sample-vp9-opus.mp4", 96_048),
            // The remux dropped DiscardPadding. The final Opus block starts at
            // 2,001 ms, which is the per-track end bound, so the final packet's
            // padding must not be emitted.
            ("sample-vp9-opus-roundtrip.webm", 96_048),
        ] {
            let path = fixture_path(name);
            let input = std::fs::read(&path).unwrap();

            let wav = convert_bytes(name, &input, OutputFormat::Wav).unwrap();
            let parsed = parse_wav(&wav[0].bytes);
            assert_eq!(parsed.sample_rate, 48_000, "{name}");
            assert_eq!(parsed.channels, 1, "{name}");
            assert_eq!(parsed.samples.len(), expected, "{name} buffered");
            assert_audible(&parsed.samples);

            let streamed = parse_wav(&stream_wav(&path, ConversionOptions::default()));
            assert_eq!(streamed.sample_rate, 48_000, "{name}");
            assert_eq!(streamed.channels, 1, "{name}");
            assert_eq!(streamed.samples.len(), expected, "{name} streaming");
            assert_eq!(streamed.samples, parsed.samples, "{name} differs");
        }

        let source = fixture("sample-vp9-opus.webm");
        let source_timing = super::matroska_audio_timing(&source, 2);
        assert_eq!(source_timing.discard_padding_ns, Some(13_500_000));
        assert_eq!(source_timing.last_block_timestamp_ns, Some(2_001_000_000));

        let remux = fixture("sample-vp9-opus-roundtrip.webm");
        let remux_timing = super::matroska_audio_timing(&remux, 2);
        assert_eq!(remux_timing.discard_padding_ns, None);
        assert_eq!(remux_timing.last_block_timestamp_ns, Some(2_001_000_000));
    }

    #[test]
    fn converts_vp9_opus_webm_directly_to_opus_and_back() {
        for (name, expected) in [
            // The source WebM carries the exact DiscardPadding trim.
            ("sample-vp9-opus.webm", 96_000),
            // The remuxed WebM has no DiscardPadding, so its final block
            // timestamp supplies the 2,001 ms presentation length.
            ("sample-vp9-opus-roundtrip.webm", 96_048),
        ] {
            let input = fixture(name);
            let opus = convert_bytes(name, &input, OutputFormat::Opus).unwrap();
            assert_eq!(
                opus[0].name,
                PathBuf::from(name).with_extension("opus"),
                "{name}"
            );
            assert!(opus[0].bytes.starts_with(b"OggS"), "{name}");

            let back = convert_bytes("tone.opus", &opus[0].bytes, OutputFormat::Wav).unwrap();
            let decoded = parse_wav(&back[0].bytes);
            assert_eq!(decoded.sample_rate, 48_000, "{name}");
            assert_eq!(decoded.channels, 1, "{name}");
            // The end trim survives the decode and re-encode; no final-packet
            // padding may reappear on the round trip.
            assert_eq!(decoded.samples.len(), expected, "{name}");
            assert_audible(&decoded.samples);
        }
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
    fn native_registry_lists_pdf_archive_and_media_handlers() {
        let registry = native_format_registry();
        assert_eq!(registry.len(), input_format::InputFormat::SELECTABLE.len());
        assert!(registry.iter().all(|handler| !handler.outputs.is_empty()));

        let pdf = registry
            .iter()
            .find(|handler| handler.input == input_format::InputFormat::Pdf)
            .unwrap();
        assert!(pdf.outputs.contains(&OutputFormat::Text));
        assert!(pdf.outputs.contains(&OutputFormat::TextUtf16Le));
        assert!(!pdf.outputs.contains(&OutputFormat::Png));

        let archive = registry
            .iter()
            .find(|handler| handler.input == input_format::InputFormat::Archive)
            .unwrap();
        assert!(archive.outputs.contains(&OutputFormat::Files));
        assert!(archive.outputs.contains(&OutputFormat::Zip));

        let video = registry
            .iter()
            .find(|handler| handler.input == input_format::InputFormat::Video)
            .unwrap();
        assert!(video.outputs.contains(&OutputFormat::Png));
        assert!(video.outputs.contains(&OutputFormat::Jpeg));
        assert!(video.outputs.contains(&OutputFormat::Webp));
        assert!(video.outputs.contains(&OutputFormat::Mp4));
        assert!(video.outputs.contains(&OutputFormat::Webm));
        assert!(!video.outputs.contains(&OutputFormat::Gif));

        let any = registry
            .iter()
            .find(|handler| handler.input == input_format::InputFormat::Any)
            .unwrap();
        assert_eq!(any.outputs.first(), Some(&OutputFormat::Files));
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
        assert!(OutputFormat::Mp3.supports_input("song.wav"));
        assert!(OutputFormat::Mp3.supports_input("movie.mkv"));
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
                OutputFormat::Zip
                    | OutputFormat::Tar
                    | OutputFormat::TarGz
                    | OutputFormat::Gzip
                    | OutputFormat::Files
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

        for extension in ["zip", "tar", "tgz", "gz"] {
            assert!(
                extensions.contains(&extension),
                "archives must be selectable so they can be extracted"
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
    fn extracts_text_from_pdf_input() {
        let pdf = convert_bytes(
            "notes.txt",
            b"Hello from PDF\nSecond line\n",
            OutputFormat::Pdf,
        )
        .unwrap();
        let text = convert_bytes("notes.pdf", &pdf[0].bytes, OutputFormat::Text).unwrap();
        let text = String::from_utf8(text[0].bytes.clone()).unwrap();
        assert!(text.contains("Hello from PDF"), "extracted: {text:?}");
        assert!(text.contains("Second line"), "extracted: {text:?}");

        let utf16 = convert_bytes("notes.pdf", &pdf[0].bytes, OutputFormat::TextUtf16Le).unwrap();
        assert_eq!(&utf16[0].bytes[..2], &[0xff, 0xfe]);

        assert!(matches!(
            convert_bytes("notes.pdf", &pdf[0].bytes, OutputFormat::Png),
            Err(ConvertError::Unsupported { .. })
        ));
        assert!(matches!(
            convert_bytes(
                "broken.pdf",
                b"%PDF-1.7\nnot a document",
                OutputFormat::Text
            ),
            Err(ConvertError::Pdf(_))
        ));
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
        let cancel = std::sync::atomic::AtomicBool::new(false);
        super::convert_audio_file_to_writer_with_cancel(
            input,
            &input.file_name().unwrap().to_string_lossy(),
            OutputFormat::Wav,
            options,
            &mut output,
            &cancel,
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
    fn streams_wav_at_each_supported_depth_from_a_file() {
        let directory = stream_temp_dir("wav-depth-stream");
        let input = directory.join("quiet.wav");
        let source = vec![1_i32; 512];
        std::fs::write(&input, wav_24_bit_at(48_000, 1, &source)).unwrap();

        for (depth, expected_format) in [
            (AudioBitDepth::Bit16, 1),
            (AudioBitDepth::Bit24, 1),
            (AudioBitDepth::Float32, 3),
        ] {
            let mut output = std::io::Cursor::new(Vec::new());
            super::convert_audio_file_to_writer(
                &input,
                "quiet.wav",
                OutputFormat::Wav,
                ConversionOptions {
                    audio_bit_depth: depth,
                    ..Default::default()
                },
                &mut output,
            )
            .unwrap();
            let bytes = output.into_inner();
            let (format, channels, rate, bits, _, size) = wav_layout(&bytes);
            assert_eq!(
                (format, channels, rate, bits),
                (expected_format, 1, 48_000, depth.bits()),
                "unexpected streamed WAV header for {depth:?}"
            );
            assert_eq!(size, 512 * usize::from(depth.bits() / 8));
            match depth {
                AudioBitDepth::Bit16 => {
                    let parsed = parse_wav(&bytes);
                    let mut distinct = parsed.samples;
                    distinct.sort_unstable();
                    distinct.dedup();
                    assert!(
                        distinct.len() > 1,
                        "streamed 16-bit output was not dithered"
                    );
                }
                AudioBitDepth::Bit24 => {
                    assert!(
                        wav_24_bit_samples(&bytes).iter().all(|sample| *sample == 1),
                        "streamed 24-bit output should be quantized without dither"
                    );
                }
                AudioBitDepth::Float32 => {
                    let decoded = wav_float_samples(&bytes);
                    assert!(decoded
                        .iter()
                        .all(|sample| { (*sample - 1.0 / 8_388_608.0).abs() <= 1.0e-7 }));
                }
            }
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streaming_flac_rejects_float_depth_with_a_typed_error() {
        let directory = stream_temp_dir("wav-flac-float");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let mut output = std::io::Cursor::new(Vec::new());
        let error = super::convert_audio_file_to_writer(
            &input,
            "tone.wav",
            OutputFormat::Flac,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Float32,
                ..Default::default()
            },
            &mut output,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::UnsupportedBitDepth {
                format: "FLAC",
                depth: "32-bit float"
            }
        ));
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
    fn streams_full_scale_multichannel_without_clipping() {
        let directory = stream_temp_dir("loud-surround");
        let input = directory.join("loud.wav");
        std::fs::write(&input, wav_channels_at(48_000, 2_000, &[1.0; 6])).unwrap();
        let output = stream_wav(
            &input,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&output);
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.samples.len(), 2_000 * 2);
        assert!(
            !parsed.samples.contains(&i16::MIN),
            "a full-scale streaming sample wrapped"
        );
        let peak = parsed
            .samples
            .iter()
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(peak > 30_000, "the streaming downmix was too quiet");
        assert_audible(&parsed.samples);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_multichannel_downmix_keeps_standard_level() {
        let directory = stream_temp_dir("stream-downmix-level");
        let input = directory.join("centre.wav");
        std::fs::write(
            &input,
            wav_channels_at(48_000, 480, &[0.0, 0.0, 0.5, 0.0, 0.0, 0.0]),
        )
        .unwrap();
        let output = stream_wav(
            &input,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&output);
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.samples.len(), 480 * 2);
        let left = f32::from(parsed.samples[0]) / i16::MAX as f32;
        let right = f32::from(parsed.samples[1]) / i16::MAX as f32;
        assert!(
            (left - 0.3535).abs() < 0.002,
            "streamed centre left was {left}, expected 0.707 * 0.5"
        );
        assert!(
            (right - 0.3535).abs() < 0.002,
            "streamed centre right was {right}, expected 0.707 * 0.5"
        );

        std::fs::write(
            &input,
            wav_channels_at(48_000, 480, &[0.0, 0.0, 0.0, 0.0, 0.5, 0.0]),
        )
        .unwrap();
        let output = stream_wav(
            &input,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&output);
        let left = f32::from(parsed.samples[0]) / i16::MAX as f32;
        let right = f32::from(parsed.samples[1]) / i16::MAX as f32;
        assert!(
            (left - 0.3535).abs() < 0.002,
            "streamed surround left was {left}, expected 0.707 * 0.5"
        );
        assert!(
            right.abs() < 0.002,
            "a back-left signal leaked into the right channel: {right}"
        );
        std::fs::remove_dir_all(&directory).unwrap();
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
    fn path_and_memory_track_lists_agree() {
        for (name, extension) in [
            ("sample-two-opus-tracks.webm", "webm"),
            ("sample-interleaved-tracks.mkv", "mkv"),
            ("sample-video.mp4", "mp4"),
        ] {
            let input = fixture(name);
            let path = fixture_path(name);
            assert_eq!(
                super::audio_tracks(&input, extension).unwrap(),
                super::audio_tracks_from_path(&path).unwrap(),
                "{name}"
            );
        }
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
    fn selected_audio_track_changes_the_decoded_tone() {
        let name = "sample-two-opus-tracks.webm";
        let input = fixture(name);
        let path = fixture_path(name);

        let default = parse_wav(&convert_bytes(name, &input, OutputFormat::Wav).unwrap()[0].bytes);
        let first = parse_wav(
            &convert_bytes_with_options(
                name,
                &input,
                OutputFormat::Wav,
                ConversionOptions {
                    audio_track: Some(0),
                    ..Default::default()
                },
            )
            .unwrap()[0]
                .bytes,
        );
        let second = parse_wav(
            &convert_bytes_with_options(
                name,
                &input,
                OutputFormat::Wav,
                ConversionOptions {
                    audio_track: Some(1),
                    ..Default::default()
                },
            )
            .unwrap()[0]
                .bytes,
        );

        for parsed in [&default, &first, &second] {
            assert_eq!(parsed.channels, 1);
            assert_eq!(parsed.sample_rate, 48_000);
        }
        assert_eq!(default.samples.len(), 48_000);
        assert_eq!(first.samples.len(), 48_000);
        assert_eq!(second.samples.len(), 48_480);
        assert_eq!(default.samples, first.samples);
        assert_ne!(first.samples, second.samples);
        assert!((decoded_tone_frequency(&default) - 440.0).abs() < 5.0);
        assert!((decoded_tone_frequency(&first) - 440.0).abs() < 5.0);
        assert!((decoded_tone_frequency(&second) - 880.0).abs() < 5.0);
        for parsed in [&default, &first, &second] {
            let rms = decoded_rms(parsed);
            assert!(
                (1_000.0..5_000.0).contains(&rms),
                "decoded tone RMS is {rms:.1}"
            );
            assert_audible(&parsed.samples);
        }

        let default_stream = parse_wav(&stream_wav(&path, ConversionOptions::default()));
        let second_stream = parse_wav(&stream_wav(
            &path,
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        ));
        assert!((decoded_tone_frequency(&default_stream) - 440.0).abs() < 5.0);
        assert!((decoded_tone_frequency(&second_stream) - 880.0).abs() < 5.0);
        assert_eq!(second_stream.samples.len(), 48_480);

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
        assert!(
            output.get_ref().is_empty(),
            "an invalid track selection wrote audio before returning an error"
        );
    }

    #[test]
    fn selects_the_audio_track_in_an_mp4_video() {
        let name = "sample-video.mp4";
        let input = fixture(name);
        let tracks = super::audio_tracks(&input, "mp4").unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].codec, "AAC");
        assert_eq!(tracks[0].channels, 1);
        assert_eq!(tracks[0].sample_rate, 44_100);

        let selected = convert_bytes_with_options(
            name,
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        let parsed = parse_wav(&selected[0].bytes);
        assert_eq!(parsed.channels, 1);
        assert_audible(&parsed.samples);

        let error = convert_bytes_with_options(
            name,
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::AudioTrackNotFound {
                requested: 2,
                available: 1
            }
        ));
    }

    #[test]
    fn selected_audio_track_reaches_every_audio_encoder() {
        let name = "sample-two-opus-tracks.webm";
        let input = fixture(name);
        for target in [
            OutputFormat::Wav,
            OutputFormat::Flac,
            OutputFormat::Aiff,
            OutputFormat::Au,
            OutputFormat::Opus,
            OutputFormat::Mp3,
        ] {
            let encoded = convert_bytes_with_options(
                name,
                &input,
                target,
                ConversionOptions {
                    audio_track: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();
            let parsed = if target == OutputFormat::Au {
                // AU is an output-only format here, so inspect its PCM payload
                // directly instead of asking the decoder to read it back.
                assert_eq!(&encoded[0].bytes[0..4], b".snd", "{target:?}");
                let channels =
                    u32::from_be_bytes(encoded[0].bytes[20..24].try_into().unwrap()) as u16;
                let sample_rate = u32::from_be_bytes(encoded[0].bytes[16..20].try_into().unwrap());
                let samples = encoded[0].bytes[24..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|bytes| i16::from_be_bytes(*bytes))
                    .collect();
                ParsedWav {
                    channels,
                    sample_rate,
                    bits_per_sample: 16,
                    samples,
                }
            } else {
                let decoded = convert_bytes(
                    encoded[0].name.to_string_lossy().as_ref(),
                    &encoded[0].bytes,
                    OutputFormat::Wav,
                )
                .unwrap();
                parse_wav(&decoded[0].bytes)
            };
            assert_eq!(parsed.channels, 1, "{target:?}");
            assert_eq!(parsed.sample_rate, 48_000, "{target:?}");
            assert!(
                (decoded_tone_frequency(&parsed) - 880.0).abs() < 20.0,
                "{target:?} did not encode the second track: {} Hz",
                decoded_tone_frequency(&parsed)
            );
            assert_audible(&parsed.samples);
        }
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
            let expected_frames = if name == "sample-ac3.mp4" {
                // The MP4 edit list skips 256 samples and presents 2 s.
                96_000
            } else {
                // The elementary stream and Matroska fixture have no edit list.
                96_768
            };
            assert_eq!(parsed.samples.len(), expected_frames * 6, "{name}");
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
            assert_eq!(stereo.samples.len(), expected_frames * 2, "{name}");
            assert_audible(&stereo.samples);
        }
    }

    #[test]
    fn downmixes_centre_and_surround_channels() {
        // Only the centre channel carries audio. Keeping the first two channels
        // would produce silence, which is the bug this test guards against.
        let input = fixture("sample-ac3-center.ac3");
        let decoded = convert_bytes("sample-ac3-center.ac3", &input, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&decoded[0].bytes);
        assert_eq!(decoded.channels, 6);
        assert_eq!(decoded.sample_rate, 48_000);
        let centre_peak = decoded
            .samples
            .iter()
            .skip(2)
            .step_by(6)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(centre_peak > 1_000, "the AC-3 centre channel is silent");

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
        assert_eq!(stereo.sample_rate, 48_000);
        assert_eq!(stereo.samples.len(), decoded.samples.len() / 3);
        let left_peak = stereo
            .samples
            .iter()
            .step_by(2)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        let right_peak = stereo
            .samples
            .iter()
            .skip(1)
            .step_by(2)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(left_peak > 1_000, "the centre tone was lost from the left");
        assert!(
            right_peak > 1_000,
            "the centre tone was lost from the right"
        );
        assert!(
            left_peak.abs_diff(right_peak) < left_peak / 20,
            "the centre tone was not balanced: {left_peak} vs {right_peak}"
        );
        let ratio = f32::from(left_peak) / f32::from(centre_peak);
        assert!(
            (ratio - 0.707).abs() < 0.02,
            "the centre channel was downmixed at {ratio}, expected 0.707"
        );

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
        assert_eq!(mono.sample_rate, 48_000);
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
    fn downmixes_51_wav_with_expected_centre_and_surround_gains() {
        // Values are constant so the output is exactly the downmix coefficient
        // applied to each channel. Channel order is L, R, C, LFE, BL, BR.
        let cases: [(&str, [f32; 6], f32, f32); 7] = [
            ("left only", [0.5, 0.0, 0.0, 0.0, 0.0, 0.0], 0.5, 0.0),
            ("right only", [0.0, 0.5, 0.0, 0.0, 0.0, 0.0], 0.0, 0.5),
            (
                "centre only",
                [0.0, 0.0, 0.5, 0.0, 0.0, 0.0],
                0.353_553,
                0.353_553,
            ),
            (
                "back left only",
                [0.0, 0.0, 0.0, 0.0, 0.5, 0.0],
                0.353_553,
                0.0,
            ),
            (
                "back right only",
                [0.0, 0.0, 0.0, 0.0, 0.0, 0.5],
                0.0,
                0.353_553,
            ),
            ("LFE only", [0.0, 0.0, 0.0, 0.5, 0.0, 0.0], 0.25, 0.25),
            (
                "stereo passthrough",
                [0.25, 0.5, 0.0, 0.0, 0.0, 0.0],
                0.25,
                0.5,
            ),
        ];

        for (name, channels, expected_left, expected_right) in cases {
            let input = wav_channels_at(48_000, 128, &channels);
            let converted = convert_bytes_with_options(
                "surround.wav",
                &input,
                OutputFormat::Wav,
                ConversionOptions {
                    audio_channels: Some(AudioChannels::Stereo),
                    ..Default::default()
                },
            )
            .unwrap();
            let parsed = parse_wav(&converted[0].bytes);
            assert_eq!(parsed.channels, 2, "{name}");
            assert_eq!(parsed.sample_rate, 48_000, "{name}");
            assert_eq!(parsed.samples.len(), 128 * 2, "{name}");
            let left = f32::from(parsed.samples[0]) / i16::MAX as f32;
            let right = f32::from(parsed.samples[1]) / i16::MAX as f32;
            assert!(
                (left - expected_left).abs() < 0.002,
                "{name}: left was {left}, expected {expected_left}"
            );
            assert!(
                (right - expected_right).abs() < 0.002,
                "{name}: right was {right}, expected {expected_right}"
            );
        }
    }

    #[test]
    fn multichannel_audio_converts_to_every_audio_target() {
        let input = wav_channels_at(48_000, 48_000, &[0.0, 0.0, 0.5, 0.0, 0.0, 0.0]);
        for target in [
            OutputFormat::Wav,
            OutputFormat::Flac,
            OutputFormat::Aiff,
            OutputFormat::Au,
            OutputFormat::Opus,
            OutputFormat::Mp3,
        ] {
            let converted = convert_bytes_with_options(
                "surround.wav",
                &input,
                target,
                ConversionOptions::default(),
            )
            .unwrap();
            let bytes = &converted[0].bytes;
            assert!(!bytes.is_empty(), "{target:?} produced no output");

            match target {
                OutputFormat::Wav => {
                    let parsed = parse_wav(bytes);
                    assert_eq!(parsed.channels, 6, "{target:?}");
                    assert_eq!(parsed.sample_rate, 48_000, "{target:?}");
                    assert_eq!(parsed.samples.len(), 48_000 * 6, "{target:?}");
                    assert_audible(&parsed.samples);
                }
                OutputFormat::Flac => {
                    let wav = convert_bytes("surround.flac", bytes, OutputFormat::Wav).unwrap();
                    let parsed = parse_wav(&wav[0].bytes);
                    assert_eq!(parsed.channels, 6, "{target:?}");
                    assert_eq!(parsed.sample_rate, 48_000, "{target:?}");
                    assert_eq!(parsed.samples.len(), 48_000 * 6, "{target:?}");
                    assert_audible(&parsed.samples);
                }
                OutputFormat::Aiff => {
                    assert_eq!(&bytes[..4], b"FORM", "{target:?}");
                    assert_eq!(u16::from_be_bytes(bytes[20..22].try_into().unwrap()), 6);
                    assert_eq!(
                        u32::from_be_bytes(bytes[22..26].try_into().unwrap()),
                        48_000
                    );
                    assert_eq!(
                        u32::from_be_bytes(bytes[42..46].try_into().unwrap()),
                        48_000 * 6 * 2 + 8
                    );
                }
                OutputFormat::Au => {
                    assert_eq!(&bytes[..4], b".snd", "{target:?}");
                    assert_eq!(
                        u32::from_be_bytes(bytes[8..12].try_into().unwrap()),
                        48_000 * 6 * 2
                    );
                    assert_eq!(u32::from_be_bytes(bytes[20..24].try_into().unwrap()), 6);
                }
                OutputFormat::Opus => {
                    let wav = convert_bytes("surround.opus", bytes, OutputFormat::Wav).unwrap();
                    let parsed = parse_wav(&wav[0].bytes);
                    assert_eq!(parsed.channels, 6, "{target:?}");
                    assert_eq!(parsed.sample_rate, 48_000, "{target:?}");
                    assert!(
                        parsed.samples.len() / 6 >= 48_000,
                        "{target:?} was truncated"
                    );
                    assert_audible(&parsed.samples);
                }
                OutputFormat::Mp3 => {
                    let wav = convert_bytes("surround.mp3", bytes, OutputFormat::Wav).unwrap();
                    let parsed = parse_wav(&wav[0].bytes);
                    assert_eq!(parsed.channels, 2, "{target:?}");
                    assert_eq!(parsed.sample_rate, 48_000, "{target:?}");
                    assert!(
                        parsed.samples.len() / 2 >= 47_000,
                        "{target:?} was truncated"
                    );
                    assert_audible(&parsed.samples);
                    let left_peak = parsed
                        .samples
                        .iter()
                        .step_by(2)
                        .map(|sample| sample.unsigned_abs())
                        .max()
                        .unwrap();
                    let right_peak = parsed
                        .samples
                        .iter()
                        .skip(1)
                        .step_by(2)
                        .map(|sample| sample.unsigned_abs())
                        .max()
                        .unwrap();
                    assert!(left_peak > 1_000, "the centre tone was lost from the left");
                    assert!(
                        right_peak > 1_000,
                        "the centre tone was lost from the right"
                    );
                }
                _ => unreachable!("unexpected audio target {target:?}"),
            }
        }
    }

    #[test]
    fn downmix_layouts_use_explicit_coefficients() {
        // Each case has one non-zero channel, so the result is exactly the
        // coefficient for that layout rather than an accidental slice.
        // Expected values: stereo left/right and mono.
        let cases: &[(&str, &[f32], f32, f32, f32)] = &[
            ("mono up-mix", &[0.5], 0.5, 0.5, 0.5),
            ("stereo", &[0.5, 0.25], 0.5, 0.25, 0.375),
            ("3.0 centre", &[0.0, 0.0, 0.5], 0.3535, 0.3535, 0.3535),
            ("quad back left", &[0.0, 0.0, 0.5, 0.0], 0.3535, 0.0, 0.25),
            ("quad back right", &[0.0, 0.0, 0.0, 0.5], 0.0, 0.3535, 0.25),
            (
                "5.0 back left",
                &[0.0, 0.0, 0.0, 0.5, 0.0],
                0.3535,
                0.0,
                0.25,
            ),
            ("5.1 LFE", &[0.0, 0.0, 0.0, 0.5, 0.0, 0.0], 0.25, 0.25, 0.25),
            (
                "6.1 back centre",
                &[0.0, 0.0, 0.0, 0.0, 0.5, 0.0, 0.0],
                0.25,
                0.25,
                0.25,
            ),
            (
                "6.1 side left",
                &[0.0, 0.0, 0.0, 0.0, 0.0, 0.5, 0.0],
                0.3535,
                0.0,
                0.25,
            ),
            (
                "7.1 back left",
                &[0.0, 0.0, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0],
                0.3535,
                0.0,
                0.25,
            ),
            (
                "7.1 side left",
                &[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.5, 0.0],
                0.3535,
                0.0,
                0.25,
            ),
            (
                "7.1 side right",
                &[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.5],
                0.0,
                0.3535,
                0.25,
            ),
            (
                "extra channel above 7.1",
                &[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.5],
                0.25,
                0.25,
                0.25,
            ),
        ];

        for (name, frame, expected_left, expected_right, expected_mono) in cases {
            let audio = super::DecodedAudio {
                samples: frame.to_vec(),
                channels: frame.len(),
                sample_rate: 48_000,
                source_bits: None,
            };
            let stereo = super::remix_channels(audio.clone(), AudioChannels::Stereo);
            assert_eq!(stereo.channels, 2, "{name}");
            assert_eq!(stereo.samples.len(), 2, "{name}");
            assert!(
                (stereo.samples[0] - expected_left).abs() < 1e-5,
                "{name}: left was {}, expected {expected_left}",
                stereo.samples[0]
            );
            assert!(
                (stereo.samples[1] - expected_right).abs() < 1e-5,
                "{name}: right was {}, expected {expected_right}",
                stereo.samples[1]
            );

            let mono = super::remix_channels(audio, AudioChannels::Mono);
            assert_eq!(mono.channels, 1, "{name}");
            assert_eq!(mono.samples.len(), 1, "{name}");
            assert!(
                (mono.samples[0] - expected_mono).abs() < 1e-5,
                "{name}: mono was {}, expected {expected_mono}",
                mono.samples[0]
            );
        }
    }

    #[test]
    fn full_scale_multichannel_downmix_does_not_wrap_or_clip() {
        for value in [1.0_f32, -1.0_f32] {
            let input = wav_channels_at(48_000, 256, &[value; 6]);
            let converted = convert_bytes_with_options(
                "loud.wav",
                &input,
                OutputFormat::Wav,
                ConversionOptions {
                    audio_channels: Some(AudioChannels::Stereo),
                    ..Default::default()
                },
            )
            .unwrap();
            let parsed = parse_wav(&converted[0].bytes);
            assert_eq!(parsed.channels, 2);
            assert_eq!(parsed.samples.len(), 256 * 2);
            assert!(
                !parsed.samples.contains(&i16::MIN),
                "a full-scale sample wrapped to i16::MIN"
            );
            let peak = parsed
                .samples
                .iter()
                .map(|sample| sample.unsigned_abs())
                .max()
                .unwrap();
            assert!(peak > 30_000, "the downmix was unexpectedly quiet");

            let direct = super::remix_channels(
                super::DecodedAudio {
                    samples: vec![value; 6 * 256],
                    channels: 6,
                    sample_rate: 48_000,
                    source_bits: None,
                },
                AudioChannels::Stereo,
            );
            assert!(
                direct.samples.iter().all(|sample| sample.is_finite()),
                "the downmix produced NaN or infinity"
            );
            let direct_peak = direct
                .samples
                .iter()
                .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
            assert!(
                direct_peak <= 1.0 + 1e-6,
                "the downmix exceeded full scale at {direct_peak}"
            );
            assert!(direct_peak > 0.99, "the downmix was over-attenuated");
        }
    }

    #[test]
    fn hot_downmix_preserves_relative_levels() {
        // The left side is twice the right before the mix (L + BL vs R + BR).
        // Peak scaling must keep that 2:1 balance.
        let frame = [1.0_f32, 0.5, 0.0, 0.0, 0.5, 0.25];
        let samples = frame.iter().copied().cycle().take(6 * 256).collect();
        let direct = super::remix_channels(
            super::DecodedAudio {
                samples,
                channels: 6,
                sample_rate: 48_000,
                source_bits: None,
            },
            AudioChannels::Stereo,
        );
        assert!(direct.samples.iter().all(|sample| sample.is_finite()));
        let left = direct.samples[0];
        let right = direct.samples[1];
        assert!(left <= 1.0 + 1e-6, "the downmix exceeded full scale");
        assert!(left > 0.99, "the downmix was over-attenuated");
        assert!(
            (right / left - 0.5).abs() < 1e-5,
            "peak scaling changed the channel balance: {left} vs {right}"
        );
    }

    #[test]
    fn multichannel_opus_decodes_and_downmixes_centre_and_surround() {
        let centre = opus_surround_fixture([0.0, 0.5, 0.0, 0.0, 0.0, 0.0]);
        let decoded = convert_bytes("centre.opus", &centre, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&decoded[0].bytes);
        assert_eq!(decoded.channels, 6);
        assert!(decoded.samples.len() > 10_000 * 6);
        assert_audible(&decoded.samples);
        let centre_peak = decoded
            .samples
            .iter()
            .skip(2)
            .step_by(6)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(centre_peak > 1_000, "the centre channel is silent");

        let stereo = convert_bytes_with_options(
            "centre.opus",
            &centre,
            OutputFormat::Wav,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        )
        .unwrap();
        let stereo = parse_wav(&stereo[0].bytes);
        assert_eq!(stereo.channels, 2);
        let left_peak = stereo
            .samples
            .iter()
            .step_by(2)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        let right_peak = stereo
            .samples
            .iter()
            .skip(1)
            .step_by(2)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(left_peak > 1_000, "centre audio was lost from the left");
        assert!(right_peak > 1_000, "centre audio was lost from the right");
        assert!(
            left_peak.abs_diff(right_peak) < left_peak / 10,
            "centre audio was not balanced: {left_peak} vs {right_peak}"
        );

        let directory = stream_temp_dir("multichannel-opus");
        let path = directory.join("centre.opus");
        std::fs::write(&path, &centre).unwrap();
        let streamed = stream_wav(
            &path,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        );
        let streamed = parse_wav(&streamed);
        assert_eq!(streamed.channels, 2);
        assert_audible(&streamed.samples);
        std::fs::remove_dir_all(&directory).unwrap();

        let surround = opus_surround_fixture([0.0, 0.0, 0.0, 0.5, 0.0, 0.0]);
        let stereo = convert_bytes_with_options(
            "surround.opus",
            &surround,
            OutputFormat::Wav,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        )
        .unwrap();
        let stereo = parse_wav(&stereo[0].bytes);
        let left_peak = stereo
            .samples
            .iter()
            .step_by(2)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        let right_peak = stereo
            .samples
            .iter()
            .skip(1)
            .step_by(2)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(left_peak > 1_000, "surround audio was lost from the left");
        assert!(
            left_peak > right_peak * 2,
            "a back-left signal leaked into the right channel: {left_peak} vs {right_peak}"
        );
    }

    #[test]
    fn multichannel_opus_output_preserves_51_channels() {
        let input = wav_channels_at(48_000, 960, &[0.0, 0.0, 0.5, 0.0, 0.0, 0.0]);
        let opus = convert_bytes("centre.wav", &input, OutputFormat::Opus).unwrap();
        assert!(opus[0].bytes.starts_with(b"OggS"));
        let decoded = convert_bytes("centre.opus", &opus[0].bytes, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&decoded[0].bytes);
        assert_eq!(decoded.channels, 6);
        assert_audible(&decoded.samples);
        let centre_peak = decoded
            .samples
            .iter()
            .skip(2)
            .step_by(6)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(centre_peak > 1_000, "the centre channel was not preserved");

        let directory = stream_temp_dir("opus-51-stream");
        let path = directory.join("centre.wav");
        std::fs::write(&path, &input).unwrap();
        let mut output = std::io::Cursor::new(Vec::new());
        super::convert_audio_file_to_writer(
            &path,
            "centre.wav",
            OutputFormat::Opus,
            ConversionOptions::default(),
            &mut output,
        )
        .unwrap();
        let streamed =
            convert_bytes("centre.opus", &output.into_inner(), OutputFormat::Wav).unwrap();
        let streamed = parse_wav(&streamed[0].bytes);
        assert_eq!(streamed.channels, 6);
        let centre_peak = streamed
            .samples
            .iter()
            .skip(2)
            .step_by(6)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(
            centre_peak > 1_000,
            "streaming Opus did not preserve the centre channel"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn multichannel_opus_output_can_be_downmixed_to_stereo() {
        let input = wav_channels_at(48_000, 960, &[0.0, 0.0, 0.5, 0.0, 0.0, 0.0]);
        let opus = convert_bytes_with_options(
            "centre.wav",
            &input,
            OutputFormat::Opus,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(opus[0].bytes.starts_with(b"OggS"));
        let decoded = convert_bytes("centre.opus", &opus[0].bytes, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&decoded[0].bytes);
        assert_eq!(decoded.channels, 2);
        assert_audible(&decoded.samples);
    }

    #[test]
    fn rejects_invalid_opus_layouts_with_a_typed_error() {
        assert!(matches!(
            super::opus_channels(3, 0),
            Err(ConvertError::UnsupportedChannelLayout {
                codec: "Opus",
                channels: 3
            })
        ));
        assert!(matches!(
            super::opus_channels(9, 1),
            Err(ConvertError::UnsupportedChannelLayout {
                codec: "Opus",
                channels: 9
            })
        ));
        assert_eq!(super::opus_channels(6, 1).unwrap(), 6);
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
        for (name, expected, channels, sample_rate) in [
            // 2 s AAC: 1024 samples of encoder priming are removed, then the
            // edit list's 88,200-sample presentation length is applied.
            ("sample-aac.m4a", 88_200, 1, 44_100),
            // No edit list here. The audio track's mdhd duration is 135,168
            // samples; the movie duration is only 3,036 ms and would be wrong.
            ("sample.m4a", 135_168, 1, 44_100),
            // The video track's movie timing must not determine the audio
            // track's length. This track is also 135,168 samples.
            ("sample-video.mp4", 135_168, 1, 44_100),
            // MP4 Opus: edit list media_time 336, duration 2,001 ms. The
            // decoder already removed the 312-sample Opus pre-skip.
            ("sample-vp9-opus.mp4", 96_048, 1, 48_000),
            // MP4 AC-3: edit list skips 256 samples and presents 2 s.
            ("sample-ac3.mp4", 96_000, 6, 48_000),
        ] {
            let path = fixture_path(name);
            let input = std::fs::read(&path).unwrap();

            let wav = convert_bytes(name, &input, OutputFormat::Wav).unwrap();
            let parsed = parse_wav(&wav[0].bytes);
            assert_eq!(parsed.sample_rate, sample_rate, "{name}");
            assert_eq!(parsed.channels, channels, "{name}");
            assert_eq!(
                parsed.samples.len(),
                expected * usize::from(channels),
                "{name} buffered"
            );
            assert_audible(&parsed.samples);

            let streamed = parse_wav(&stream_wav(&path, ConversionOptions::default()));
            assert_eq!(streamed.sample_rate, sample_rate, "{name}");
            assert_eq!(streamed.channels, channels, "{name}");
            assert_eq!(
                streamed.samples.len(),
                expected * usize::from(channels),
                "{name} streaming"
            );
            assert_eq!(streamed.samples, parsed.samples, "{name} differs");

            let float_options = ConversionOptions {
                audio_bit_depth: AudioBitDepth::Float32,
                ..Default::default()
            };
            let buffered_float = wav_float_samples(
                &convert_bytes_with_options(name, &input, OutputFormat::Wav, float_options)
                    .unwrap()[0]
                    .bytes,
            );
            let streamed_float = wav_float_samples(&stream_wav(&path, float_options));
            assert_eq!(
                buffered_float, streamed_float,
                "{name} float samples differ"
            );
            let truncation = undithered_i16(&buffered_float);
            let buffered_dithered = parsed.samples != truncation;
            let streamed_dithered = streamed.samples != truncation;
            assert!(
                buffered_dithered && streamed_dithered,
                "{name} must differ from the undithered truncation (buffered: {buffered_dithered}, streaming: {streamed_dithered})"
            );
        }

        let aac = fixture("sample-aac.m4a");
        let aac_info = super::mp4_gapless_info(super::mp4_find(&aac, b"moov").unwrap(), 0)
            .expect("the AAC edit list is present");
        assert_eq!(aac_info.skip, 1_024, "AAC priming was not read");
        assert_eq!(aac_info.limit, Some(88_200));
        assert!(aac_info.limit_is_presentation);

        let m4a = fixture("sample.m4a");
        let m4a_info = super::mp4_gapless_info(super::mp4_find(&m4a, b"moov").unwrap(), 0)
            .expect("the audio track duration is present");
        assert_eq!(m4a_info.skip, 0);
        assert_eq!(m4a_info.limit, Some(135_168));
        assert!(!m4a_info.limit_is_presentation);

        let opus = fixture("sample-vp9-opus.mp4");
        let opus_info = super::mp4_gapless_info(super::mp4_find(&opus, b"moov").unwrap(), 1)
            .expect("the Opus edit list is present");
        assert_eq!(opus_info.skip, 336);
        assert_eq!(opus_info.limit, Some(96_048));
        assert!(opus_info.limit_is_presentation);

        let ac3 = fixture("sample-ac3.mp4");
        let ac3_info = super::mp4_gapless_info(super::mp4_find(&ac3, b"moov").unwrap(), 0)
            .expect("the AC-3 edit list is present");
        assert_eq!(ac3_info.skip, 256);
        assert_eq!(ac3_info.limit, Some(96_000));
        assert!(ac3_info.limit_is_presentation);

        // The Opus decoder already consumed the 312-sample header delay, so
        // only the remaining 24 samples of the edit list offset are skipped.
        assert_eq!(
            super::mp4_trim_for_kind(opus_info, super::AudioCodecKind::Opus, 312),
            super::AudioTrim {
                skip_frames: 24,
                limit_frames: Some(96_048),
                tail_padding: 0,
            }
        );
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

    fn linear_resample_reference(
        samples: &[f32],
        channels: usize,
        source_rate: u32,
        target_rate: u32,
    ) -> Vec<f32> {
        if channels == 0 {
            return Vec::new();
        }
        let frames = samples.len() / channels;
        if frames == 0 {
            return Vec::new();
        }
        let ratio = f64::from(target_rate) / f64::from(source_rate);
        let target_frames = ((frames as f64 * ratio).round() as usize).max(1);
        let mut output = Vec::with_capacity(target_frames * channels);
        for frame in 0..target_frames {
            let position = frame as f64 / ratio;
            let left = position.floor() as usize;
            let right = (left + 1).min(frames - 1);
            let fraction = (position - left as f64) as f32;
            for channel in 0..channels {
                let a = samples[left * channels + channel];
                let b = samples[right * channels + channel];
                output.push(a + (b - a) * fraction);
            }
        }
        output
    }

    fn mono_tone(rate: u32, frequency: f64, seconds: f64) -> super::DecodedAudio {
        let frames = (rate as f64 * seconds).round() as usize;
        super::DecodedAudio {
            samples: (0..frames)
                .map(|frame| {
                    (std::f64::consts::TAU * frequency * frame as f64 / f64::from(rate)).sin()
                        as f32
                        * 0.5
                })
                .collect(),
            channels: 1,
            sample_rate: rate,
            source_bits: None,
        }
    }

    fn rms(samples: &[f32]) -> f64 {
        (samples
            .iter()
            .map(|sample| f64::from(*sample) * f64::from(*sample))
            .sum::<f64>()
            / samples.len() as f64)
            .sqrt()
    }

    fn zero_crossings(samples: &[f32]) -> usize {
        samples
            .windows(2)
            .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
            .count()
    }

    #[test]
    fn resampler_rejects_high_frequency_images_at_44_1_and_48_khz() {
        // Sweep test tones across the region above the new 22.05 kHz Nyquist.
        // Linear interpolation folds each one back into the output, while the
        // band-limited path rejects it. (Frequencies must stay below the 24 kHz
        // source Nyquist; a 30 kHz test tone would already alias at 48 kHz.)
        for (target_rate, frequency) in [
            (44_100, 23_000.0),
            (44_100, 23_300.0),
            (44_100, 23_600.0),
            (44_100, 23_900.0),
            // A non-integer 48 -> 32 kHz ratio has a 16 kHz output Nyquist.
            (32_000, 23_000.0),
        ] {
            let source = mono_tone(48_000, frequency, 0.5);
            let old = linear_resample_reference(&source.samples, 1, 48_000, target_rate);
            let new = super::resample_audio(source, target_rate).unwrap();
            assert!(
                rms(&old) > 0.2,
                "{frequency} Hz -> {target_rate}: the linear reference did not alias: RMS {}",
                rms(&old)
            );
            assert!(
                rms(&new.samples) < 0.03,
                "{frequency} Hz -> {target_rate} aliased through the band-limited path at RMS {}",
                rms(&new.samples)
            );
            if target_rate == 32_000 {
                // 23 kHz sampled at 48 kHz and resampled to 32 kHz folds to
                // 9 kHz with linear interpolation: a clearly low image.
                assert!(
                    (zero_crossings(&old) as i64 - 9_000).abs() < 100,
                    "the linear reference did not produce the 9 kHz image: {} crossings",
                    zero_crossings(&old)
                );
            }
        }
    }

    #[test]
    fn resampler_preserves_passband_amplitude_and_frequency() {
        for (source_rate, target_rate) in [(44_100, 48_000), (48_000, 44_100), (48_000, 32_000)] {
            let source = mono_tone(source_rate, 10_000.0, 0.5);
            let output = super::resample_audio(source, target_rate).unwrap();
            assert_eq!(output.sample_rate, target_rate);
            assert_eq!(
                output.samples.len(),
                ((source_rate as f64 * 0.5 * target_rate as f64 / source_rate as f64).round()
                    as usize)
            );
            assert!(
                (rms(&output.samples) - 0.353_553).abs() < 0.03,
                "{source_rate} -> {target_rate}: passband RMS {}",
                rms(&output.samples)
            );
            let crossings = zero_crossings(&output.samples);
            let expected = (10_000.0 * 0.5 * 2.0) as i64;
            assert!(
                (crossings as i64 - expected).abs() < 20,
                "{source_rate} -> {target_rate}: expected about {expected} crossings, got {crossings}"
            );
        }
    }

    #[test]
    fn resampler_preserves_exact_frame_counts_and_streaming_continuity() {
        let directory = stream_temp_dir("resampler-counts");
        for (source_rate, frames, target_rate, expected) in [
            (44_100, 44_100, 48_000, 48_000),
            (48_000, 48_000, 44_100, 44_100),
            // 48 kHz -> 32 kHz is a non-integer ratio (2/3).
            (48_000, 48_000, 32_000, 32_000),
            (44_100, 44_100, 32_000, 32_000),
        ] {
            let source = mono_tone(source_rate, 440.0, frames as f64 / source_rate as f64);
            let direct = super::resample_audio(source, target_rate).unwrap();
            assert_eq!(
                direct.samples.len(),
                expected,
                "direct {source_rate}->{target_rate}"
            );

            let input = directory.join(format!("tone-{source_rate}-{frames}.wav"));
            std::fs::write(&input, wav_fixture_at(source_rate, frames)).unwrap();
            let streamed = stream_wav(
                &input,
                ConversionOptions {
                    audio_sample_rate: Some(target_rate),
                    ..Default::default()
                },
            );
            let parsed = parse_wav(&streamed);
            assert_eq!(parsed.sample_rate, target_rate);
            assert_eq!(
                parsed.samples.len(),
                expected * usize::from(parsed.channels),
                "streaming {source_rate}->{target_rate}"
            );

            let buffered = convert_bytes_with_options(
                "tone.wav",
                &wav_fixture_at(source_rate, frames),
                OutputFormat::Wav,
                ConversionOptions {
                    audio_sample_rate: Some(target_rate),
                    ..Default::default()
                },
            )
            .unwrap();
            let buffered = parse_wav(&buffered[0].bytes);
            assert_eq!(buffered.samples.len(), parsed.samples.len());
            let max_difference = buffered
                .samples
                .iter()
                .zip(&parsed.samples)
                .map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs())
                .max()
                .unwrap();
            assert!(
                max_difference <= 2,
                "streaming chunking changed samples by {max_difference} for {source_rate}->{target_rate}"
            );
        }
        std::fs::remove_dir_all(&directory).unwrap();
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
        assert!(matches!(
            error,
            ConvertError::UnsupportedBitDepth {
                format: "FLAC",
                depth: "32-bit float"
            }
        ));
    }

    #[test]
    fn writes_wav_at_each_supported_depth_with_decodable_samples() {
        let sample_rate = 48_000;
        let frames = 512;
        let mut source = Vec::with_capacity(frames);
        for frame in 0..frames {
            let value = ((frame as f64 / f64::from(sample_rate) * 440.0 * std::f64::consts::TAU)
                .sin()
                * 0.5
                * 8_388_607.0)
                .round() as i32;
            source.push(value);
        }
        // A very low-level sample, both signs at full scale, and a tone.
        source[0] = 1;
        source[1] = -1;
        source[2] = 8_388_607;
        source[3] = -8_388_608;
        let wav = wav_24_bit_at(sample_rate, 1, &source);
        let expected: Vec<f32> = source
            .iter()
            .map(|sample| *sample as f32 / 8_388_608.0)
            .collect();

        for (depth, expected_format, expected_bits) in [
            (AudioBitDepth::Bit16, 1, 16),
            (AudioBitDepth::Bit24, 1, 24),
            (AudioBitDepth::Float32, 3, 32),
        ] {
            let converted = convert_bytes_with_options(
                "source.wav",
                &wav,
                OutputFormat::Wav,
                ConversionOptions {
                    audio_bit_depth: depth,
                    ..Default::default()
                },
            )
            .unwrap();
            let (format, channels, rate, bits, _, size) = wav_layout(&converted[0].bytes);
            assert_eq!(
                (format, channels, rate, bits),
                (expected_format, 1, sample_rate, expected_bits),
                "unexpected WAV header for {depth:?}"
            );
            assert_eq!(size, frames * usize::from(depth.bits() / 8));

            match depth {
                AudioBitDepth::Bit16 => {
                    let parsed = parse_wav(&converted[0].bytes);
                    assert_eq!(parsed.samples.len(), frames);
                    for (index, (&actual, &expected)) in
                        parsed.samples.iter().zip(&expected).enumerate()
                    {
                        let actual = f32::from(actual) / f32::from(i16::MAX);
                        assert!(
                            (actual - expected).abs() <= 2.5 / f32::from(i16::MAX),
                            "16-bit sample {index} was {actual} instead of {expected}"
                        );
                    }
                }
                AudioBitDepth::Bit24 => {
                    let decoded = wav_24_bit_samples(&converted[0].bytes);
                    assert_eq!(decoded.len(), frames);
                    for (index, (&actual, &expected)) in decoded.iter().zip(&source).enumerate() {
                        assert!(
                            (actual - expected).abs() <= 2,
                            "24-bit sample {index} was {actual} instead of {expected}"
                        );
                    }
                }
                AudioBitDepth::Float32 => {
                    let decoded = wav_float_samples(&converted[0].bytes);
                    assert_eq!(decoded.len(), frames);
                    for (index, (&actual, &expected)) in decoded.iter().zip(&expected).enumerate() {
                        assert!(
                            (actual - expected).abs() <= 1.0e-6,
                            "float sample {index} was {actual} instead of {expected}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn preserves_detail_below_16_bit_in_24_bit_flac_and_float_wav() {
        let frames = 8_000;
        let mut source = vec![1_i32; frames];
        source[0] = -1;
        source[1] = 2;
        source[2] = -2;
        source[3] = 8_388_607;
        source[4] = -8_388_608;
        let wav = wav_24_bit_at(48_000, 1, &source);

        let flac = convert_bytes_with_options(
            "detail.wav",
            &wav,
            OutputFormat::Flac,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Bit24,
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = flac_24_bit_samples(&flac[0].bytes);
        assert_eq!(decoded.len(), frames);
        for (index, (&actual, &expected)) in decoded.iter().zip(&source).enumerate() {
            assert!(
                (actual - expected).abs() <= 1,
                "24-bit FLAC sample {index} was {actual} instead of {expected}"
            );
        }
        assert_eq!(decoded[100], 1);

        let float = convert_bytes_with_options(
            "detail.wav",
            &wav,
            OutputFormat::Wav,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Float32,
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = wav_float_samples(&float[0].bytes);
        assert_eq!(decoded.len(), frames);
        for (index, (&actual, &expected)) in decoded.iter().zip(&source).enumerate() {
            let expected = expected as f32 / 8_388_608.0;
            assert!(
                (actual - expected).abs() <= 1.0e-7,
                "float sample {index} was {actual} instead of {expected}"
            );
        }
        assert!(decoded[100] > 0.0);
        assert!(
            decoded[100] < 1.0 / 32_768.0,
            "the float output lost detail below the 16-bit LSB"
        );
    }

    #[test]
    fn dithers_16_bit_output_deterministically_without_dithering_24_bit() {
        let source = vec![1_i32; 512];
        let wav = wav_24_bit_at(48_000, 1, &source);
        let convert = || {
            convert_bytes_with_options(
                "quiet.wav",
                &wav,
                OutputFormat::Wav,
                ConversionOptions {
                    audio_bit_depth: AudioBitDepth::Bit16,
                    ..Default::default()
                },
            )
            .unwrap()
            .remove(0)
            .bytes
        };
        let first = convert();
        let second = convert();
        assert_eq!(first, second, "dither must be reproducible");

        let parsed = parse_wav(&first);
        let mut distinct = parsed.samples.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(
            distinct.len() > 1,
            "a near-silent 16-bit signal was crushed to one value instead of dithered"
        );

        let pcm24 = convert_bytes_with_options(
            "quiet.wav",
            &wav,
            OutputFormat::Wav,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Bit24,
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = wav_24_bit_samples(&pcm24[0].bytes);
        assert!(
            decoded.iter().all(|sample| *sample == 1),
            "24-bit output should be quantized without dither"
        );

        // The same rule applies when the source is a 32-bit f32 pipeline.
        let deep = DecodedAudio {
            samples: vec![1.0 / 8_388_608.0; 512],
            channels: 1,
            sample_rate: 48_000,
            source_bits: Some(32),
        };
        let pcm24 = wav_24_bit_samples(&encode_wav(&deep, AudioBitDepth::Bit24).unwrap());
        assert!(
            pcm24.iter().all(|sample| *sample == 1),
            "24-bit output from a 32-bit source should not be dithered"
        );
        let flac = flac_24_bit_samples(&encode_flac(&deep, AudioBitDepth::Bit24).unwrap());
        assert!(
            flac.iter().all(|sample| *sample == 1),
            "24-bit FLAC from a 32-bit source should not be dithered"
        );
        let float = wav_float_samples(&encode_wav(&deep, AudioBitDepth::Float32).unwrap());
        assert!(float
            .iter()
            .all(|sample| (*sample - 1.0 / 8_388_608.0).abs() <= 1.0e-7));
    }

    #[test]
    fn unknown_precision_f32_to_16_bit_is_dithered() {
        let samples = vec![1.0 / 1_048_576.0; 8_000];
        let unknown = DecodedAudio {
            samples: samples.clone(),
            channels: 1,
            sample_rate: 48_000,
            source_bits: None,
        };
        let known_24 = DecodedAudio {
            source_bits: Some(24),
            ..unknown.clone()
        };
        let known_32 = DecodedAudio {
            source_bits: Some(32),
            ..unknown.clone()
        };

        let unknown_wav = encode_wav(&unknown, AudioBitDepth::Bit16).unwrap();
        assert_eq!(
            unknown_wav,
            encode_wav(&known_24, AudioBitDepth::Bit16).unwrap(),
            "unknown and 24-bit sources should dither identically"
        );
        assert_eq!(
            unknown_wav,
            encode_wav(&known_32, AudioBitDepth::Bit16).unwrap(),
            "unknown and 32-bit sources should dither identically"
        );
        let decoded = parse_wav(&unknown_wav);
        let truncation = undithered_i16(&samples);
        assert_ne!(
            decoded.samples, truncation,
            "unknown-precision WAV matches the undithered truncation"
        );
        let mut distinct = decoded.samples.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(
            distinct.len() > 1,
            "unknown-precision 16-bit output was crushed instead of dithered"
        );
        assert_eq!(
            unknown_wav,
            encode_wav(&unknown, AudioBitDepth::Bit16).unwrap(),
            "dither must be reproducible"
        );

        let unknown_flac = encode_flac(&unknown, AudioBitDepth::Bit16).unwrap();
        assert_eq!(
            unknown_flac,
            encode_flac(&known_24, AudioBitDepth::Bit16).unwrap(),
            "unknown and 24-bit FLAC should dither identically"
        );
        assert_eq!(
            unknown_flac,
            encode_flac(&unknown, AudioBitDepth::Bit16).unwrap(),
            "FLAC dither must be reproducible"
        );
        let mut reader = claxon::FlacReader::new(std::io::Cursor::new(unknown_flac)).unwrap();
        assert_eq!(reader.streaminfo().bits_per_sample, 16);
        let decoded: Vec<i32> = reader.samples().map(Result::unwrap).collect();
        let truncation: Vec<i32> = truncation.iter().map(|&sample| i32::from(sample)).collect();
        assert_ne!(
            decoded, truncation,
            "unknown-precision FLAC matches the undithered truncation"
        );
        let mut distinct = decoded;
        distinct.sort_unstable();
        distinct.dedup();
        assert!(
            distinct.len() > 1,
            "unknown-precision 16-bit FLAC was crushed instead of dithered"
        );
    }

    #[test]
    fn aac_16_bit_output_is_dithered_and_streaming_matches() {
        let path = fixture_path("sample.m4a");
        let input = std::fs::read(&path).unwrap();
        let buffered =
            parse_wav(&convert_bytes("sample.m4a", &input, OutputFormat::Wav).unwrap()[0].bytes);
        let streamed = parse_wav(&stream_wav(&path, ConversionOptions::default()));
        assert_eq!(buffered.samples, streamed.samples, "sample.m4a differs");

        let float_options = ConversionOptions {
            audio_bit_depth: AudioBitDepth::Float32,
            ..Default::default()
        };
        let buffered_float = wav_float_samples(
            &convert_bytes_with_options("sample.m4a", &input, OutputFormat::Wav, float_options)
                .unwrap()[0]
                .bytes,
        );
        let streamed_float = wav_float_samples(&stream_wav(&path, float_options));
        assert_eq!(
            buffered_float, streamed_float,
            "sample.m4a float samples differ"
        );
        let truncation = undithered_i16(&buffered_float);
        let buffered_dithered = buffered.samples != truncation;
        let streamed_dithered = streamed.samples != truncation;
        assert!(
            buffered_dithered && streamed_dithered,
            "AAC 16-bit output must differ from the undithered truncation (buffered: {buffered_dithered}, streaming: {streamed_dithered})"
        );

        let again =
            parse_wav(&convert_bytes("sample.m4a", &input, OutputFormat::Wav).unwrap()[0].bytes);
        assert_eq!(
            buffered.samples, again.samples,
            "dither must be reproducible"
        );

        // The AAC fixture starts with the encoder's quiet pre-roll. A 16-bit
        // conversion without dither crushes that region to zero.
        let quiet = &buffered.samples[..512];
        assert!(
            quiet
                .iter()
                .map(|sample| sample.unsigned_abs())
                .max()
                .unwrap()
                <= 4,
            "the AAC fixture's quiet pre-roll is no longer quiet"
        );
        let mut distinct = quiet.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(distinct.len() > 1, "AAC 16-bit output was not dithered");
        assert!(
            quiet.iter().any(|sample| *sample != 0),
            "AAC 16-bit output was a silent crush"
        );

        let buffered_flac = convert_bytes("sample.m4a", &input, OutputFormat::Flac)
            .unwrap()
            .remove(0)
            .bytes;
        let mut streamed_flac = std::io::Cursor::new(Vec::new());
        super::convert_audio_file_to_writer(
            &path,
            "sample.m4a",
            OutputFormat::Flac,
            ConversionOptions::default(),
            &mut streamed_flac,
        )
        .unwrap();
        let buffered_flac = flac_16_bit_samples(&buffered_flac);
        let streamed_flac = flac_16_bit_samples(&streamed_flac.into_inner());
        assert_eq!(buffered_flac, streamed_flac, "sample.m4a FLAC differs");
        let truncation: Vec<i32> = truncation.iter().map(|&sample| i32::from(sample)).collect();
        let buffered_dithered = buffered_flac != truncation;
        let streamed_dithered = streamed_flac != truncation;
        assert!(
            buffered_dithered && streamed_dithered,
            "AAC FLAC output must differ from the undithered truncation (buffered: {buffered_dithered}, streaming: {streamed_dithered})"
        );
        let mut quiet = buffered_flac[..512].to_vec();
        quiet.sort_unstable();
        quiet.dedup();
        assert!(quiet.len() > 1, "AAC 16-bit FLAC was not dithered");
    }

    #[test]
    fn clamps_out_of_range_samples_at_every_wav_depth() {
        let audio = DecodedAudio {
            samples: vec![2.0, -2.0],
            channels: 1,
            sample_rate: 48_000,
            source_bits: Some(32),
        };
        let pcm16 = parse_wav(&encode_wav(&audio, AudioBitDepth::Bit16).unwrap());
        assert_eq!(pcm16.samples, [i16::MAX, -i16::MAX]);
        let pcm24 = wav_24_bit_samples(&encode_wav(&audio, AudioBitDepth::Bit24).unwrap());
        assert_eq!(pcm24, [8_388_607, -8_388_607]);
        let float = wav_float_samples(&encode_wav(&audio, AudioBitDepth::Float32).unwrap());
        assert_eq!(float, [1.0, -1.0]);
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
        // failing the conversion, even when the UI offers a high bitrate.
        for bitrate in Mp3Bitrate::ALL {
            let low_rate = convert_bytes_with_options(
                "tone.wav",
                &wav_fixture(),
                OutputFormat::Mp3,
                ConversionOptions {
                    mp3_bitrate: bitrate,
                    ..Default::default()
                },
            )
            .unwrap_or_else(|error| panic!("8 kHz MP3 at {} failed: {error}", bitrate.label()));
            assert!(low_rate[0].bytes.len() > 100);
        }
    }

    #[test]
    fn preserves_id3v2_tags_when_re_encoding_mp3() {
        let mp3 = convert_bytes(
            "tone.wav",
            &wav_fixture_at(44_100, 8_000),
            OutputFormat::Mp3,
        )
        .unwrap();
        let mut tagged = b"ID3\x04\x00\x00\x00\x00\x00\x0a".to_vec();
        tagged.extend([0_u8; 10]);
        tagged.extend_from_slice(&mp3[0].bytes);

        let reencoded = convert_bytes("tagged.mp3", &tagged, OutputFormat::Mp3).unwrap();
        assert!(reencoded[0].bytes.starts_with(&tagged[..20]));
    }

    #[test]
    fn preserves_flac_vorbis_comments_and_pictures() {
        let flac = convert_bytes(
            "tone.wav",
            &wav_fixture_at(44_100, 8_000),
            OutputFormat::Flac,
        )
        .unwrap();
        let mut blocks = flac_metadata_blocks(&flac[0].bytes).unwrap();
        let mut comment = Vec::new();
        comment.extend_from_slice(&4_u32.to_le_bytes());
        comment.extend_from_slice(b"test");
        comment.extend_from_slice(&0_u32.to_le_bytes());
        blocks.insert(1, (4, comment.clone()));
        let audio_start = flac_metadata_end(&flac[0].bytes).unwrap();
        let tagged = rebuild_flac(&blocks, &flac[0].bytes[audio_start..]);

        let reencoded = convert_bytes("tagged.flac", &tagged, OutputFormat::Flac).unwrap();
        let output_blocks = flac_metadata_blocks(&reencoded[0].bytes).unwrap();
        assert!(output_blocks
            .iter()
            .any(|(kind, data)| *kind == 4 && data == &comment));
    }

    #[test]
    fn mp3_output_preserves_tone_channels_duration_and_bitrate() {
        for (sample_rate, channels) in [(44_100, 1), (44_100, 2), (48_000, 1), (48_000, 2)] {
            let frames = sample_rate;
            let wav = wav_tone_at(sample_rate, frames, channels, 1_000.0, 0.5);
            let mp3 = convert_bytes_with_options(
                "tone.wav",
                &wav,
                OutputFormat::Mp3,
                ConversionOptions {
                    mp3_bitrate: Mp3Bitrate::Kbps192,
                    ..Default::default()
                },
            )
            .unwrap()
            .remove(0)
            .bytes;
            assert_mp3_tone(
                &mp3,
                sample_rate,
                channels,
                frames as usize,
                Mp3Bitrate::Kbps192,
            );
        }

        let wav = wav_tone_at(44_100, 44_100, 2, 1_000.0, 0.5);
        let encode = |bitrate| {
            convert_bytes_with_options(
                "tone.wav",
                &wav,
                OutputFormat::Mp3,
                ConversionOptions {
                    mp3_bitrate: bitrate,
                    ..Default::default()
                },
            )
            .unwrap()
            .remove(0)
            .bytes
        };
        let low = encode(Mp3Bitrate::Kbps128);
        let high = encode(Mp3Bitrate::Kbps320);
        assert!(
            high.len() > low.len() * 2,
            "320 kbps produced {} bytes but 128 kbps produced {}",
            high.len(),
            low.len()
        );
        assert_mp3_tone(&low, 44_100, 2, 44_100, Mp3Bitrate::Kbps128);
        assert_mp3_tone(&high, 44_100, 2, 44_100, Mp3Bitrate::Kbps320);
    }

    #[test]
    fn streams_mp3_at_the_requested_bitrate() {
        let directory = stream_temp_dir("mp3-bitrate");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_tone_at(48_000, 48_000, 1, 1_000.0, 0.5)).unwrap();
        let encode = |bitrate| {
            let mut output = Cursor::new(Vec::new());
            super::convert_audio_file_to_writer(
                &input,
                "tone.wav",
                OutputFormat::Mp3,
                ConversionOptions {
                    mp3_bitrate: bitrate,
                    ..Default::default()
                },
                &mut output,
            )
            .unwrap();
            output.into_inner()
        };
        let low = encode(Mp3Bitrate::Kbps128);
        let high = encode(Mp3Bitrate::Kbps320);
        assert!(
            high.len() > low.len() * 2,
            "streaming 320 kbps produced {} bytes but 128 kbps produced {}",
            high.len(),
            low.len()
        );
        assert_mp3_tone(&low, 48_000, 1, 48_000, Mp3Bitrate::Kbps128);
        assert_mp3_tone(&high, 48_000, 1, 48_000, Mp3Bitrate::Kbps320);

        std::fs::write(&input, wav_tone_at(44_100, 44_100, 2, 1_000.0, 0.5)).unwrap();
        let stereo = encode(Mp3Bitrate::Kbps256);
        assert_mp3_tone(&stereo, 44_100, 2, 44_100, Mp3Bitrate::Kbps256);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn converts_movie_audio_to_mp3() {
        for name in ["sample-ac3.mkv", "sample-eac3.mkv"] {
            let input = fixture(name);
            let mp3 = convert_bytes_with_options(
                name,
                &input,
                OutputFormat::Mp3,
                ConversionOptions {
                    mp3_bitrate: Mp3Bitrate::Kbps256,
                    ..Default::default()
                },
            )
            .unwrap()
            .remove(0)
            .bytes;
            assert_eq!(mp3_first_frame_bitrate(&mp3), Some(256), "{name}");

            let decoded = convert_bytes("movie.mp3", &mp3, OutputFormat::Wav).unwrap();
            let parsed = parse_wav(&decoded[0].bytes);
            assert_eq!(parsed.sample_rate, 48_000, "{name}");
            assert_eq!(parsed.channels, 2, "{name}");
            assert!(
                parsed.samples.len() / 2 >= 90_000,
                "{name} was truncated to {} frames",
                parsed.samples.len() / 2
            );
            assert_audible(&parsed.samples);
        }
    }

    fn tiff_with_orientations(orientations: &[u16]) -> Vec<u8> {
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II*\0");
        tiff.extend_from_slice(&8_u32.to_le_bytes());
        tiff.extend_from_slice(&(orientations.len() as u16).to_le_bytes());
        for orientation in orientations {
            tiff.extend_from_slice(&0x0112_u16.to_le_bytes());
            tiff.extend_from_slice(&3_u16.to_le_bytes());
            tiff.extend_from_slice(&1_u32.to_le_bytes());
            tiff.extend_from_slice(&orientation.to_le_bytes());
            tiff.extend_from_slice(&[0, 0]);
        }
        tiff.extend_from_slice(&0_u32.to_le_bytes());
        tiff
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
