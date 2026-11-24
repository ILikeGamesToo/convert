// SPDX-License-Identifier: GPL-2.0-or-later

//! Paths, extensions and naming helpers shared by every medium.

use crate::error::ConvertError;
use crate::format::OutputFormat;
use crate::input_extensions;
use std::path::{Path, PathBuf};

pub(crate) fn safe_file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty() && name != "." && name != "..")
        .unwrap_or_else(|| "file".to_owned())
}

pub(crate) fn file_extension(name: &str) -> String {
    Path::new(name)
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

pub(crate) fn is_archive_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.ends_with(".zip")
        || name.ends_with(".tar")
        || name.ends_with(".tar.gz")
        || name.ends_with(".tgz")
        || name.ends_with(".gz")
}

pub(crate) fn output_name(input_name: &str, extension: &str) -> PathBuf {
    let stem = Path::new(input_name)
        .file_stem()
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("converted"));
    PathBuf::from(format!("{}.{}", stem.to_string_lossy(), extension))
}

pub(crate) fn unsupported(extension: &str, target: OutputFormat) -> ConvertError {
    ConvertError::Unsupported {
        input_extension: extension.to_owned(),
        target: target.label(),
    }
}

pub(crate) fn is_image_input_extension(extension: &str) -> bool {
    input_extensions::IMAGE_INPUT_EXTENSIONS.contains(&extension)
}

pub(crate) fn is_data_extension(extension: &str) -> bool {
    input_extensions::DATA_INPUT_EXTENSIONS.contains(&extension)
}

pub(crate) fn is_audio_extension(extension: &str) -> bool {
    input_extensions::AUDIO_INPUT_EXTENSIONS.contains(&extension)
}

pub(crate) fn is_pdf_extension(extension: &str) -> bool {
    input_extensions::PDF_INPUT_EXTENSIONS.contains(&extension)
}

pub(crate) fn is_video_input_extension(extension: &str) -> bool {
    matches!(extension, "mp4" | "m4v" | "mov" | "mkv" | "webm")
}

pub(crate) fn is_font_extension(extension: &str) -> bool {
    input_extensions::FONT_INPUT_EXTENSIONS.contains(&extension)
}

/// Whether `bytes` begin with an ISO BMFF `ftyp` box naming the AVIF still
/// image brand (`avif`) or the AVIF image sequence brand (`avis`), as either
/// the major brand or one of the compatible brands.
pub(crate) fn is_avif_input(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    let size = u32::from_be_bytes(bytes[0..4].try_into().expect("four bytes")) as usize;
    let end = if size >= 16 && size <= bytes.len() {
        size
    } else {
        bytes.len()
    };
    let brands = bytes.get(8..end).unwrap_or(&bytes[8..12]);
    if brands.len() >= 4 && (&brands[0..4] == b"avif" || &brands[0..4] == b"avis") {
        return true;
    }
    // The major brand and the minor version are followed by the compatible
    // brands, four bytes each.
    brands
        .get(8..)
        .unwrap_or_default()
        .as_chunks::<4>()
        .0
        .iter()
        .any(|brand| brand == b"avif" || brand == b"avis")
}
