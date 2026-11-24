// SPDX-License-Identifier: GPL-2.0-or-later

/// Image extensions accepted by the conversion engine.
pub const IMAGE_INPUT_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff", "ico", "pnm", "pbm", "pgm", "ppm",
    "pam", "tga", "qoi", "hdr", "ff", "dds", "svg", "avif", "heic", "heif",
];

/// Text and data extensions accepted by the conversion engine.
pub const DATA_INPUT_EXTENSIONS: &[&str] = &["txt", "csv", "json", "yaml", "yml", "toml"];

/// PDF documents. Their text is extracted for the text targets, and their pages
/// are rasterised in-process for the image targets.
pub const PDF_INPUT_EXTENSIONS: &[&str] = &["pdf"];

/// Audio extensions accepted by the conversion engine, including video
/// containers whose audio track can be extracted.
pub const AUDIO_INPUT_EXTENSIONS: &[&str] = &[
    "wav", "mp3", "flac", "ogg", "oga", "opus", "m4a", "aac", "mp4", "m4v", "mov", "mkv", "webm",
    "alac", "aiff", "aif", "ac3", "eac3", "ec3",
];

/// Font extensions accepted by the conversion engine. Collections are listed
/// because their first face is converted.
pub const FONT_INPUT_EXTENSIONS: &[&str] = &["ttf", "otf", "woff", "woff2", "ttc", "otc"];

/// Archive extensions accepted for extraction and re-packing. The comic
/// extensions name the same four containers: CBR is a RAR, CBZ a ZIP, CBT a
/// TAR and CB7 a 7z.
pub const ARCHIVE_INPUT_EXTENSIONS: &[&str] = &[
    "zip", "tar", "gz", "tgz", "rar", "cbr", "7z", "cb7", "cbz", "cbt",
];

/// Office documents. DOCX, XLSX and PPTX are ZIP packages of XML parts, XLS is
/// the legacy OLE2 workbook, and ODT and ODS are OpenDocument packages. Their
/// content is extracted into the text and data targets; nothing is rendered.
pub const OFFICE_INPUT_EXTENSIONS: &[&str] = &["docx", "xlsx", "xls", "pptx", "odt", "ods"];

/// Every extension accepted by the conversion engine, for the default
/// "Supported files" filter. Archive inputs are included so ZIP/TAR/GZIP can be
/// extracted with the `files` target.
pub fn supported_input_extensions() -> Vec<&'static str> {
    IMAGE_INPUT_EXTENSIONS
        .iter()
        .chain(DATA_INPUT_EXTENSIONS)
        .chain(PDF_INPUT_EXTENSIONS)
        .chain(AUDIO_INPUT_EXTENSIONS)
        .chain(ARCHIVE_INPUT_EXTENSIONS)
        .chain(FONT_INPUT_EXTENSIONS)
        .chain(OFFICE_INPUT_EXTENSIONS)
        .copied()
        .collect()
}
