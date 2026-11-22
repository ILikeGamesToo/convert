// SPDX-License-Identifier: GPL-2.0-or-later

/// Image extensions accepted by the conversion engine.
pub const IMAGE_INPUT_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff", "ico", "pnm", "pbm", "pgm", "ppm",
    "pam", "tga", "qoi", "hdr", "ff", "dds", "svg",
];

/// Text and data extensions accepted by the conversion engine.
pub const DATA_INPUT_EXTENSIONS: &[&str] = &["txt", "csv", "json", "yaml", "yml", "toml"];

/// Audio extensions accepted by the conversion engine, including video
/// containers whose audio track can be extracted.
pub const AUDIO_INPUT_EXTENSIONS: &[&str] = &[
    "wav", "mp3", "flac", "ogg", "oga", "opus", "m4a", "aac", "mp4", "m4v", "mov", "mkv", "webm",
    "alac", "aiff", "aif", "ac3", "eac3", "ec3",
];

/// Every non-archive extension accepted by the conversion engine, for the
/// default "Supported files" filter. Archive files can be selected through
/// "All files" when creating a new archive.
pub fn supported_input_extensions() -> Vec<&'static str> {
    IMAGE_INPUT_EXTENSIONS
        .iter()
        .chain(DATA_INPUT_EXTENSIONS)
        .chain(AUDIO_INPUT_EXTENSIONS)
        .copied()
        .collect()
}
