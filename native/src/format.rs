// SPDX-License-Identifier: GPL-2.0-or-later

//! Output formats and the native format registry.

use crate::input_format;
use crate::shared::{
    file_extension, is_archive_name, is_audio_extension, is_data_extension,
    is_image_input_extension, is_video_input_extension,
};
use crate::video::{native_video_routes, VideoRouteInfo};
use serde::{Deserialize, Serialize};

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

    pub(crate) fn image_format(self) -> Option<image::ImageFormat> {
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
