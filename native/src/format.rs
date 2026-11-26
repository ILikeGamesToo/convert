// SPDX-License-Identifier: GPL-2.0-or-later

//! Output formats and the native format registry.

use crate::input_format;
use crate::shared::{
    file_extension, is_archive_name, is_audio_extension, is_data_extension, is_ebook_extension,
    is_epub_extension, is_font_extension, is_html_extension, is_image_input_extension,
    is_markdown_extension, is_office_extension, is_pdf_extension, is_video_input_extension,
};
use crate::video::{native_video_routes, video_container_source, VideoRouteInfo};
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
    /// The same ZIP container as [`Self::Zip`] written with the `.cbz`
    /// extension every comic reader opens, so a CBR or CB7 can be re-packed
    /// into a CBZ.
    Cbz,
    /// Extract an archive into separate files instead of creating one.
    Files,
    Ttf,
    Otf,
    Woff,
    Woff2,
    /// Write the font's glyphs as an SVG font specimen.
    Svg,
    /// Write each PDF page as a standalone SVG vector document.
    SvgPage,
    /// Write the document as an EPUB 3 ebook, one content document per chapter,
    /// with a navigation document and an NCX.
    Epub,
    /// Write the document as one self-contained HTML file.
    Html,
    /// Write the document as CommonMark.
    Markdown,
    /// Trace a raster image into an SVG vector document.
    SvgTrace,
}

impl OutputFormat {
    pub const ALL: [Self; 51] = [
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
        Self::Cbz,
        Self::Files,
        Self::Ttf,
        Self::Otf,
        Self::Woff,
        Self::Woff2,
        Self::Svg,
        Self::SvgPage,
        Self::Epub,
        Self::Html,
        Self::Markdown,
        Self::SvgTrace,
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
            Self::Cbz => "Comic book archive (CBZ)",
            Self::Files => "Extracted files",
            Self::Ttf => "TrueType font",
            Self::Otf => "OpenType font",
            Self::Woff => "WOFF font",
            Self::Woff2 => "WOFF2 font",
            Self::Svg => "SVG font",
            Self::SvgPage => "SVG page",
            Self::Epub => "EPUB ebook",
            Self::Html => "HTML document",
            Self::Markdown => "Markdown document",
            Self::SvgTrace => "SVG trace",
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

    /// Whether this target is a font container or an SVG font specimen.
    pub fn is_font_target(self) -> bool {
        matches!(
            self,
            Self::Ttf | Self::Otf | Self::Woff | Self::Woff2 | Self::Svg
        )
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
            Self::Cbz => "cbz",
            Self::Files => "files",
            Self::Ttf => "ttf",
            Self::Otf => "otf",
            Self::Woff => "woff",
            Self::Woff2 => "woff2",
            Self::Svg | Self::SvgPage | Self::SvgTrace => "svg",
            Self::Epub => "epub",
            Self::Html => "html",
            Self::Markdown => "md",
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
        // `svg` stays the font specimen so an existing font workflow keeps
        // resolving as it did; the PDF page and image trace targets need their
        // own names because all three write the same `.svg` extension.
        if extension == "svg-page" || extension == "svgpage" {
            return Some(Self::SvgPage);
        }
        if extension == "svg-trace" || extension == "svgtrace" {
            return Some(Self::SvgTrace);
        }
        let normalized = match extension.as_str() {
            "jpeg" => "jpg",
            "tif" => "tiff",
            "aif" => "aiff",
            "yml" => "yaml",
            "tgz" => "tar.gz",
            "gzip" => "gz",
            "text" => "txt",
            "htm" | "xhtml" => "html",
            "markdown" => "md",
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
            Self::Png | Self::Jpeg | Self::Webp | Self::Gif => {
                is_image_input_extension(&extension)
                    || is_video_input_extension(&extension)
                    || is_pdf_extension(&extension)
            }
            Self::Bmp
            | Self::Tiff
            | Self::Ico
            | Self::Ppm
            | Self::Pbm
            | Self::Pgm
            | Self::Pam
            | Self::Tga
            | Self::Qoi
            | Self::Hdr
            | Self::Farbfeld => {
                is_image_input_extension(&extension) || is_pdf_extension(&extension)
            }
            Self::Pdf => extension == "txt" || is_image_input_extension(&extension),
            Self::Text => {
                is_data_extension(&extension)
                    || extension == "pdf"
                    || is_office_extension(&extension)
                    || is_ebook_extension(&extension)
            }
            Self::Csv | Self::Json => {
                is_data_extension(&extension) || is_office_extension(&extension)
            }
            Self::Yaml | Self::Toml => is_data_extension(&extension),
            Self::TextUtf8NoBom
            | Self::TextUtf8Bom
            | Self::TextUtf16Le
            | Self::TextUtf16Be
            | Self::TextUtf32Le
            | Self::TextUtf32Be => {
                extension == "txt"
                    || extension == "pdf"
                    || is_office_extension(&extension)
                    || is_ebook_extension(&extension)
            }
            Self::Wav | Self::Flac | Self::Aiff | Self::Au | Self::Opus | Self::Mp3 => {
                is_audio_extension(&extension)
            }
            // A GIF's frames and an audio-only stream fill an MP4 as well, so
            // the container targets accept them; see
            // [`crate::video::video_container_source`].
            Self::Mp4 | Self::Webm => {
                is_video_input_extension(&extension)
                    || video_container_source(self, &extension).is_some()
            }
            Self::Zip | Self::Tar | Self::TarGz | Self::Gzip => true,
            Self::Cbz | Self::Files => is_archive_name(file_name),
            Self::Ttf | Self::Otf | Self::Woff | Self::Woff2 | Self::Svg => {
                is_font_extension(&extension)
            }
            Self::SvgPage => is_pdf_extension(&extension),
            Self::Epub => {
                extension == "txt"
                    || is_markdown_extension(&extension)
                    || is_html_extension(&extension)
            }
            Self::Html => is_epub_extension(&extension) || is_markdown_extension(&extension),
            Self::Markdown => is_epub_extension(&extension) || is_html_extension(&extension),
            Self::SvgTrace => crate::trace::is_traceable_extension(&extension),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_extensions::supported_input_extensions;
    use crate::input_format;

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
        assert!(pdf.outputs.contains(&OutputFormat::Png));
        assert!(pdf.outputs.contains(&OutputFormat::Jpeg));
        assert!(pdf.outputs.contains(&OutputFormat::Webp));
        assert!(!pdf.outputs.contains(&OutputFormat::Svg));
        assert!(pdf.outputs.contains(&OutputFormat::SvgPage));

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
        assert!(video.outputs.contains(&OutputFormat::Gif));

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
    fn epub_html_and_markdown_route_between_the_text_formats() {
        assert_eq!(OutputFormat::Epub.extension(), "epub");
        assert_eq!(OutputFormat::Epub.label(), "EPUB ebook");
        assert_eq!(OutputFormat::Html.extension(), "html");
        assert_eq!(OutputFormat::Html.label(), "HTML document");
        assert_eq!(OutputFormat::Markdown.extension(), "md");
        assert_eq!(OutputFormat::Markdown.label(), "Markdown document");

        assert!(OutputFormat::Epub.supports_input("notes.txt"));
        assert!(OutputFormat::Epub.supports_input("page.html"));
        assert!(OutputFormat::Epub.supports_input("book.md"));
        assert!(!OutputFormat::Epub.supports_input("photo.png"));
        assert!(OutputFormat::Html.supports_input("book.epub"));
        assert!(OutputFormat::Html.supports_input("page.md"));
        assert!(OutputFormat::Markdown.supports_input("book.epub"));
        assert!(OutputFormat::Markdown.supports_input("page.html"));
        assert!(OutputFormat::Text.supports_input("book.epub"));
        assert!(OutputFormat::TextUtf16Le.supports_input("page.md"));
        assert!(!OutputFormat::Csv.supports_input("book.epub"));

        assert_eq!(
            OutputFormat::from_extension("htm"),
            Some(OutputFormat::Html)
        );
        assert_eq!(
            OutputFormat::from_extension("markdown"),
            Some(OutputFormat::Markdown)
        );
        assert_eq!(
            OutputFormat::from_extension(".epub"),
            Some(OutputFormat::Epub)
        );

        let registry = native_format_registry();
        let epub = registry
            .iter()
            .find(|handler| handler.input == input_format::InputFormat::Epub)
            .unwrap();
        assert_eq!(epub.canonical_extension, Some("epub"));
        assert!(epub.outputs.contains(&OutputFormat::Text));
        assert!(epub.outputs.contains(&OutputFormat::Markdown));
        assert!(epub.outputs.contains(&OutputFormat::Html));
        assert!(!epub.outputs.contains(&OutputFormat::Epub));
        assert!(!epub.outputs.contains(&OutputFormat::Csv));

        let html = registry
            .iter()
            .find(|handler| handler.input == input_format::InputFormat::Html)
            .unwrap();
        assert!(html.outputs.contains(&OutputFormat::Epub));
        assert!(html.outputs.contains(&OutputFormat::Markdown));
    }

    #[test]
    fn cbz_is_a_zip_comic_target_for_archive_inputs_only() {
        assert_eq!(OutputFormat::Cbz.extension(), "cbz");
        assert_eq!(OutputFormat::Cbz.label(), "Comic book archive (CBZ)");
        assert!(OutputFormat::Cbz.supports_input("comic.cbr"));
        assert!(OutputFormat::Cbz.supports_input("comic.cb7"));
        assert!(OutputFormat::Cbz.supports_input("comic.cbz"));
        assert!(!OutputFormat::Cbz.supports_input("page.png"));
        assert!(!OutputFormat::Cbz.supports_input("notes.txt"));
        assert!(OutputFormat::ALL.contains(&OutputFormat::Cbz));
    }
    #[test]
    fn registry_lists_avif_and_heic_as_image_inputs() {
        assert!(OutputFormat::Png.supports_input("photo.avif"));
        assert!(OutputFormat::Pdf.supports_input("photo.avif"));
        assert!(!OutputFormat::Wav.supports_input("photo.avif"));
        assert!(OutputFormat::Png.supports_input("photo.heic"));
        assert!(OutputFormat::Jpeg.supports_input("photo.heif"));
        assert!(OutputFormat::Pdf.supports_input("photo.heic"));
        assert!(!OutputFormat::Wav.supports_input("photo.heic"));

        for (input, extension) in [
            (input_format::InputFormat::Avif, "avif"),
            (input_format::InputFormat::Heic, "heic"),
        ] {
            let registry = native_format_registry();
            let handler = registry
                .iter()
                .find(|handler| handler.input == input)
                .unwrap();
            assert_eq!(handler.canonical_extension, Some(extension));
            assert!(handler.outputs.contains(&OutputFormat::Png));
            assert!(handler.outputs.contains(&OutputFormat::Jpeg));
            assert!(handler.outputs.contains(&OutputFormat::Pdf));
            assert!(!handler.outputs.contains(&OutputFormat::Wav));
        }
    }
    #[test]
    fn a_gif_and_the_audio_kinds_reach_the_mp4_container() {
        // The GIF kind keeps every image target and gains MP4, which encodes
        // its frames; WebM would need a VP9 encode and stays out.
        assert!(OutputFormat::Gif.supports_input("animation.gif"));
        assert!(OutputFormat::Mp4.supports_input("animation.gif"));
        assert!(!OutputFormat::Webm.supports_input("animation.gif"));
        // Only the audio kinds MP4 holds without re-encoding.
        assert!(OutputFormat::Mp4.supports_input("song.mp3"));
        assert!(OutputFormat::Mp4.supports_input("song.m4a"));
        assert!(!OutputFormat::Mp4.supports_input("song.flac"));
        assert!(!OutputFormat::Webm.supports_input("song.mp3"));
        // The route must not leak into the image targets.
        assert!(!OutputFormat::Png.supports_input("song.mp3"));
        assert!(!OutputFormat::Gif.supports_input("song.mp3"));

        let registry = native_format_registry();
        let handler = |input| {
            registry
                .iter()
                .find(|handler| handler.input == input)
                .unwrap()
        };
        for input in [
            input_format::InputFormat::Gif,
            input_format::InputFormat::Mp3,
            input_format::InputFormat::M4a,
        ] {
            assert!(
                handler(input).outputs.contains(&OutputFormat::Mp4),
                "{} does not offer MP4",
                input.label()
            );
            assert!(
                !handler(input).outputs.contains(&OutputFormat::Webm),
                "{} must not offer WebM",
                input.label()
            );
        }
    }

    #[test]
    fn the_trace_target_is_offered_for_raster_inputs_but_not_svg() {
        assert_eq!(OutputFormat::SvgTrace.extension(), "svg");
        assert_eq!(OutputFormat::SvgTrace.label(), "SVG trace");
        assert_eq!(
            OutputFormat::from_extension("svg-trace"),
            Some(OutputFormat::SvgTrace)
        );
        assert_eq!(
            OutputFormat::from_extension("svgtrace"),
            Some(OutputFormat::SvgTrace)
        );
        // The name `svg` still resolves to the font specimen, as it always has.
        assert_eq!(OutputFormat::from_extension("svg"), Some(OutputFormat::Svg));

        for extension in [
            "png", "jpg", "webp", "gif", "bmp", "tiff", "ico", "tga", "qoi", "hdr", "ff", "dds",
            "avif", "heic",
        ] {
            assert!(
                OutputFormat::SvgTrace.supports_input(&format!("sample.{extension}")),
                ".{extension} should be traceable"
            );
        }
        // SVG input is already vector, and the font and PDF targets keep their
        // own names rather than being reached by tracing.
        assert!(!OutputFormat::SvgTrace.supports_input("icon.svg"));
        assert!(!OutputFormat::SvgTrace.supports_input("report.pdf"));
        assert!(!OutputFormat::SvgTrace.supports_input("font.ttf"));
        assert!(!OutputFormat::SvgTrace.supports_input("song.mp3"));
        assert!(!OutputFormat::SvgTrace.supports_input("archive.zip"));
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
}
