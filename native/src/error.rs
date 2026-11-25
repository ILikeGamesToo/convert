// SPDX-License-Identifier: GPL-2.0-or-later

//! The error type shared by every conversion path.

use thiserror::Error;

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
    #[error("the PDF is encrypted and cannot be rendered without its password")]
    PdfEncrypted,
    #[error("could not render PDF page {page}: {width} x {height} pixels is too large")]
    PdfPageTooLarge {
        page: usize,
        width: f32,
        height: f32,
    },
    #[error("the SVG is too large to render")]
    SvgTooLarge,
    #[error(
        "the image is {width} x {height} pixels; tracing is limited to {max_side} pixels per side and {max_pixels} pixels in total"
    )]
    TraceTooLarge {
        width: u32,
        height: u32,
        max_side: u32,
        max_pixels: u64,
    },
    #[error("could not trace the image: {0}")]
    Trace(String),
    #[error("could not read or write the font: {0}")]
    Font(String),
    #[error("the text needs {pages} PDF pages, more than the {max}-page limit")]
    PdfTooManyPages { pages: usize, max: usize },
    #[error("the audio file has no decodable audio track")]
    NoAudioTrack,
    #[error("audio track {requested} does not exist; the file has {available} audio track(s)")]
    AudioTrackNotFound { requested: usize, available: usize },
    #[error("{codec} audio with {channels} channels is not supported")]
    UnsupportedChannelLayout {
        codec: &'static str,
        channels: usize,
    },
    #[error("the audio file is too large to convert in memory")]
    AudioTooLarge,
    #[error("{format} cannot store {depth} samples")]
    UnsupportedBitDepth {
        format: &'static str,
        depth: &'static str,
    },
    #[error("conversion cancelled")]
    Cancelled,
    #[error("the archive would contain {count} entries, more than the {max}-entry limit")]
    ArchiveTooManyEntries { count: usize, max: usize },
    #[error("the archive expands to more than {max} bytes")]
    ArchiveTooLarge { max: u64 },
    #[error("the archive contains no files")]
    NoArchiveEntries,
    #[error("the archive is encrypted or password-protected; this app cannot ask for a password")]
    ArchiveEncrypted,
    #[error("the {format} decoder refused the archive: {message}")]
    ArchiveDecoder {
        format: &'static str,
        message: String,
    },
    #[error("the archive took longer than {seconds} seconds to decode")]
    ArchiveDecodeTimeout { seconds: u64 },
    #[error("archive entry {0:?} would escape the output directory")]
    UnsafeArchivePath(String),
    #[error("could not read the PDF: {0}")]
    Pdf(String),
    #[error(
        "ffmpeg was not found ({path}); install it on PATH or set CONVERT2IT_FFMPEG to its path"
    )]
    FfmpegNotFound { path: String },
    #[error("ffmpeg exited with {status}: {stderr}")]
    FfmpegFailed { status: String, stderr: String },
    #[error("could not start ffmpeg: {0}")]
    FfmpegSpawn(String),
    #[error("ffmpeg produced no output")]
    FfmpegEmptyOutput,
    #[error("ffmpeg returned an invalid {container} container")]
    FfmpegInvalidOutput { container: &'static str },
    #[error(
        "cannot stream-copy {stream} codec {codec} into {container}; use a compatible source file"
    )]
    UnsupportedVideoCodec {
        stream: &'static str,
        codec: String,
        container: &'static str,
    },
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
    #[error("could not decode the AVIF image: {0}")]
    Avif(String),
    #[error("could not decode the HEIC image: {0}")]
    Heic(String),
    #[error("the document is encrypted or password-protected; this app cannot ask for a password")]
    OfficeEncrypted,
    #[error("the {format} document's container is corrupt: {message}")]
    OfficeCorrupt {
        format: &'static str,
        message: String,
    },
    #[error("the {format} document has no extractable body")]
    OfficeEmptyBody { format: &'static str },
    #[error("the {format} document part {part:?} is larger than the {max}-byte limit")]
    OfficePartTooLarge {
        format: &'static str,
        part: String,
        max: u64,
    },
    #[error(transparent)]
    Csv(#[from] csv::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error("the ebook is encrypted with DRM ({scheme}); this app cannot remove it")]
    EbookDrm { scheme: String },
    #[error("could not read the ebook: {message}")]
    EbookCorrupt { message: String },
    #[error("the ebook contains no readable text")]
    EbookEmpty,
    #[error(
        "{format} input is recognised but not converted by this build; it reads and writes EPUB only"
    )]
    EbookFormatDeferred { format: &'static str },
    #[error("could not write the EPUB: {0}")]
    EbookWrite(String),
    #[error("the ebook holds {count} entries, more than the {max}-entry limit")]
    EbookTooManyEntries { count: usize, max: usize },
    #[error("the ebook expands to more than {max} bytes")]
    EbookTooLarge { max: u64 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
