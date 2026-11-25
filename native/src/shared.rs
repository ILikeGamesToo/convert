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
        || name.ends_with(".rar")
        || name.ends_with(".7z")
        || name.ends_with(".cbr")
        || name.ends_with(".cbz")
        || name.ends_with(".cbt")
        || name.ends_with(".cb7")
}

fn output_stem(input_name: &str) -> &std::ffi::OsStr {
    Path::new(input_name)
        .file_stem()
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("converted"))
}

pub(crate) fn output_name(input_name: &str, extension: &str) -> PathBuf {
    PathBuf::from(format!(
        "{}.{}",
        output_stem(input_name).to_string_lossy(),
        extension
    ))
}

/// The name of one page's output file for a multi-page input. The web app names
/// a PDF's pages `<stem>_<page>.<ext>` with a zero-based page index, so this
/// matches it: a two-page `report.pdf` becomes `report_0.png` and
/// `report_1.png`.
pub(crate) fn page_output_name(input_name: &str, extension: &str, page: usize) -> PathBuf {
    PathBuf::from(format!(
        "{}_{page}.{extension}",
        output_stem(input_name).to_string_lossy()
    ))
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

pub(crate) fn is_office_extension(extension: &str) -> bool {
    input_extensions::OFFICE_INPUT_EXTENSIONS.contains(&extension)
}

pub(crate) fn is_epub_extension(extension: &str) -> bool {
    input_extensions::EPUB_INPUT_EXTENSIONS.contains(&extension)
}

pub(crate) fn is_html_extension(extension: &str) -> bool {
    input_extensions::HTML_INPUT_EXTENSIONS.contains(&extension)
}

pub(crate) fn is_markdown_extension(extension: &str) -> bool {
    input_extensions::MARKDOWN_INPUT_EXTENSIONS.contains(&extension)
}

/// HTML, XHTML, Markdown and EPUB: the documents whose text is read directly
/// rather than decoded from a media container.
pub(crate) fn is_ebook_extension(extension: &str) -> bool {
    is_epub_extension(extension) || is_html_extension(extension) || is_markdown_extension(extension)
}

/// Whether `bytes` begin with an ISO BMFF `ftyp` box naming the AVIF still
/// image brand (`avif`) or the AVIF image sequence brand (`avis`), as either
/// the major brand or one of the compatible brands.
pub(crate) fn is_avif_input(bytes: &[u8]) -> bool {
    ftyp_has_brand(bytes, &[b"avif", b"avis"])
}

/// Whether `bytes` begin with an ISO BMFF `ftyp` box naming a HEIC/HEIF brand:
/// HEVC images (`heic`, `heix`), HEVC images with alpha (`hevc`, `hevx`) and
/// their sequence variants (`heis`, `hevm`, `hevs`, `msf1`), plus the generic
/// `mif1`/`mif2` brands that AVIF files also use.
///
/// Because `mif1`/`mif2`/`msf1` say nothing about the codec, callers test
/// [`is_avif_input`] first: an AVIF that declares the `avif` brand among its
/// compatible brands is AVIF, and anything else generic is treated as HEIC.
pub(crate) fn is_heic_input(bytes: &[u8]) -> bool {
    ftyp_has_brand(
        bytes,
        &[
            b"heic", b"heix", b"hevc", b"hevx", b"heis", b"hevm", b"hevs", b"msf1", b"mif1",
            b"mif2",
        ],
    )
}

/// Whether the `ftyp` box at the start of `bytes` names any of `brands`, as the
/// major brand or as one of the compatible brands.
fn ftyp_has_brand(bytes: &[u8], brands: &[&[u8; 4]]) -> bool {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    let size = u32::from_be_bytes(bytes[0..4].try_into().expect("four bytes")) as usize;
    let end = if size >= 16 && size <= bytes.len() {
        size
    } else {
        bytes.len()
    };
    let Some((major_brand, rest)) = bytes.get(8..end).and_then(|list| list.split_at_checked(4))
    else {
        return false;
    };
    if <&[u8; 4]>::try_from(major_brand).is_ok_and(|major| brands.contains(&major)) {
        return true;
    }
    // The major brand and the minor version are followed by the compatible
    // brands, four bytes each.
    rest.get(4..)
        .unwrap_or_default()
        .as_chunks::<4>()
        .0
        .iter()
        .any(|brand| brands.contains(&brand))
}
