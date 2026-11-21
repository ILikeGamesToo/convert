// SPDX-License-Identifier: GPL-2.0-or-later

/// Image extensions accepted by the conversion engine.
pub const IMAGE_INPUT_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff", "ico", "pnm", "pbm", "pgm", "ppm",
    "pam", "tga", "qoi", "hdr", "ff", "dds", "svg",
];

/// Text and data extensions accepted by the conversion engine.
pub const DATA_INPUT_EXTENSIONS: &[&str] = &["txt", "csv", "json", "yaml", "yml", "toml"];

/// Audio extensions accepted by the conversion engine.
pub const AUDIO_INPUT_EXTENSIONS: &[&str] = &[
    "wav", "mp3", "flac", "ogg", "oga", "m4a", "aac", "mp4", "alac",
];

/// Archive extensions accepted for extraction.
pub const ARCHIVE_INPUT_EXTENSIONS: &[&str] = &["zip", "tar", "tar.gz", "tgz", "gz"];

/// Every file extension the conversion engine can read, for the file picker.
pub fn supported_input_extensions() -> Vec<&'static str> {
    IMAGE_INPUT_EXTENSIONS
        .iter()
        .chain(DATA_INPUT_EXTENSIONS)
        .chain(AUDIO_INPUT_EXTENSIONS)
        .chain(ARCHIVE_INPUT_EXTENSIONS)
        .copied()
        .collect()
}
