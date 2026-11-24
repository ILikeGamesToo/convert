// SPDX-License-Identifier: GPL-2.0-or-later

use std::{fs::File, io::Read, path::Path};

const SNIFF_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputFormat {
    Png,
    Jpeg,
    Webp,
    Gif,
    Bmp,
    Tiff,
    Ico,
    Pnm,
    Tga,
    Qoi,
    Hdr,
    Farbfeld,
    Dds,
    Svg,
    Avif,
    Heic,
    Text,
    Csv,
    Json,
    Yaml,
    Toml,
    Pdf,
    Wav,
    Mp3,
    Flac,
    Ogg,
    Opus,
    M4a,
    Aac,
    Aiff,
    Ac3,
    Video,
    Archive,
    /// RAR, including the CBR comic extension. Read-only: the native build
    /// extracts and re-packs it, but never writes one.
    Rar,
    /// 7z, including the CB7 comic extension. Read-only for the same reason.
    SevenZip,
    Ttf,
    Otf,
    Woff,
    Woff2,
    Any,
    Mixed,
}

impl InputFormat {
    pub const SELECTABLE: [Self; 40] = [
        Self::Any,
        Self::Png,
        Self::Jpeg,
        Self::Webp,
        Self::Gif,
        Self::Bmp,
        Self::Tiff,
        Self::Ico,
        Self::Pnm,
        Self::Tga,
        Self::Qoi,
        Self::Hdr,
        Self::Farbfeld,
        Self::Dds,
        Self::Svg,
        Self::Avif,
        Self::Heic,
        Self::Text,
        Self::Csv,
        Self::Json,
        Self::Yaml,
        Self::Toml,
        Self::Pdf,
        Self::Wav,
        Self::Mp3,
        Self::Flac,
        Self::Ogg,
        Self::Opus,
        Self::M4a,
        Self::Aac,
        Self::Aiff,
        Self::Ac3,
        Self::Video,
        Self::Archive,
        Self::Rar,
        Self::SevenZip,
        Self::Ttf,
        Self::Otf,
        Self::Woff,
        Self::Woff2,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG image",
            Self::Jpeg => "JPEG image",
            Self::Webp => "WebP image",
            Self::Gif => "GIF image",
            Self::Bmp => "BMP image",
            Self::Tiff => "TIFF image",
            Self::Ico => "Windows icon",
            Self::Pnm => "PNM image",
            Self::Tga => "TGA image",
            Self::Qoi => "QOI image",
            Self::Hdr => "Radiance HDR image",
            Self::Farbfeld => "Farbfeld image",
            Self::Dds => "DDS texture",
            Self::Svg => "SVG image",
            Self::Avif => "AVIF image",
            Self::Heic => "HEIC image",
            Self::Text => "Plain text",
            Self::Csv => "CSV data",
            Self::Json => "JSON data",
            Self::Yaml => "YAML data",
            Self::Toml => "TOML data",
            Self::Pdf => "PDF document",
            Self::Wav => "WAV audio",
            Self::Mp3 => "MP3 audio",
            Self::Flac => "FLAC audio",
            Self::Ogg => "OGG audio",
            Self::Opus => "Opus audio",
            Self::M4a => "M4A audio",
            Self::Aac => "AAC audio",
            Self::Aiff => "AIFF audio",
            Self::Ac3 => "Dolby audio",
            Self::Video => "Video",
            Self::Archive => "Archive file",
            Self::Rar => "RAR archive",
            Self::SevenZip => "7z archive",
            Self::Ttf => "TrueType font",
            Self::Otf => "OpenType font",
            Self::Woff => "WOFF font",
            Self::Woff2 => "WOFF2 font",
            Self::Any => "Any file",
            Self::Mixed => "Mixed formats",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Png => "Portable Network Graphics",
            Self::Jpeg => "JPEG/JFIF image",
            Self::Webp => "WebP image",
            Self::Gif => "GIF image",
            Self::Bmp => "Windows bitmap image",
            Self::Tiff => "Tagged Image File Format",
            Self::Ico => "Windows icon",
            Self::Pnm => "PNM, PBM, PGM, PPM, or PAM image",
            Self::Tga => "Truevision TGA image",
            Self::Qoi => "Quite OK Image",
            Self::Hdr => "Radiance HDR image",
            Self::Farbfeld => "Farbfeld image",
            Self::Dds => "DirectDraw Surface texture",
            Self::Svg => "Scalable Vector Graphics",
            Self::Avif => "AV1 Image File Format",
            Self::Heic => "High Efficiency Image File Format (HEVC)",
            Self::Text => "Plain text and text encodings",
            Self::Csv => "Comma-separated values",
            Self::Json => "JavaScript Object Notation",
            Self::Yaml => "YAML data",
            Self::Toml => "TOML data",
            Self::Pdf => "PDF text extraction",
            Self::Wav => "Waveform audio",
            Self::Mp3 => "MPEG audio",
            Self::Flac => "Free Lossless Audio Codec",
            Self::Ogg => "Ogg audio",
            Self::Opus => "Opus audio",
            Self::M4a => "M4A or ALAC audio",
            Self::Aac => "Advanced Audio Coding",
            Self::Aiff => "Audio Interchange File Format",
            Self::Ac3 => "AC-3 or E-AC-3 audio",
            Self::Video => "MP4, MOV, MKV, or WebM with an audio track",
            Self::Archive => "ZIP, TAR, TAR.GZ, or GZIP; archive creation only",
            Self::Rar => "RAR version 4 or 5; extraction and re-packing only",
            Self::SevenZip => "7z; extraction and re-packing only",
            Self::Ttf => "TrueType outlines in an sfnt container",
            Self::Otf => "CFF outlines in an OpenType container",
            Self::Woff => "Web Open Font Format 1.0",
            Self::Woff2 => "Web Open Font Format 2.0",
            Self::Any | Self::Mixed => "Archive creation only",
        }
    }

    pub fn extension_label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPG",
            Self::Webp => "WEBP",
            Self::Gif => "GIF",
            Self::Bmp => "BMP",
            Self::Tiff => "TIFF",
            Self::Ico => "ICO",
            Self::Pnm => "PNM",
            Self::Tga => "TGA",
            Self::Qoi => "QOI",
            Self::Hdr => "HDR",
            Self::Farbfeld => "FF",
            Self::Dds => "DDS",
            Self::Svg => "SVG",
            Self::Avif => "AVIF",
            Self::Heic => "HEIC",
            Self::Text => "TXT",
            Self::Csv => "CSV",
            Self::Json => "JSON",
            Self::Yaml => "YAML",
            Self::Toml => "TOML",
            Self::Pdf => "PDF",
            Self::Wav => "WAV",
            Self::Mp3 => "MP3",
            Self::Flac => "FLAC",
            Self::Ogg => "OGG",
            Self::Opus => "OPUS",
            Self::M4a => "M4A",
            Self::Aac => "AAC",
            Self::Aiff => "AIFF",
            Self::Ac3 => "AC3",
            Self::Video => "VIDEO",
            Self::Archive => "ARCHIVE",
            Self::Rar => "RAR",
            Self::SevenZip => "7Z",
            Self::Ttf => "TTF",
            Self::Otf => "OTF",
            Self::Woff => "WOFF",
            Self::Woff2 => "WOFF2",
            Self::Any => "ANY",
            Self::Mixed => "MIXED",
        }
    }

    pub fn canonical_extension(self) -> Option<&'static str> {
        Some(match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Gif => "gif",
            Self::Bmp => "bmp",
            Self::Tiff => "tiff",
            Self::Ico => "ico",
            Self::Pnm => "pnm",
            Self::Tga => "tga",
            Self::Qoi => "qoi",
            Self::Hdr => "hdr",
            Self::Farbfeld => "ff",
            Self::Dds => "dds",
            Self::Svg => "svg",
            Self::Avif => "avif",
            Self::Heic => "heic",
            Self::Text => "txt",
            Self::Csv => "csv",
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Toml => "toml",
            Self::Pdf => "pdf",
            Self::Wav => "wav",
            Self::Mp3 => "mp3",
            Self::Flac => "flac",
            Self::Ogg => "ogg",
            Self::Opus => "opus",
            Self::M4a => "m4a",
            Self::Aac => "aac",
            Self::Aiff => "aiff",
            Self::Ac3 => "ac3",
            Self::Video => "mp4",
            Self::Archive => "zip",
            Self::Rar => "rar",
            Self::SevenZip => "7z",
            Self::Ttf => "ttf",
            Self::Otf => "otf",
            Self::Woff => "woff",
            Self::Woff2 => "woff2",
            Self::Any | Self::Mixed => return None,
        })
    }

    pub fn is_image(self) -> bool {
        matches!(
            self,
            Self::Png
                | Self::Jpeg
                | Self::Webp
                | Self::Gif
                | Self::Bmp
                | Self::Tiff
                | Self::Ico
                | Self::Pnm
                | Self::Tga
                | Self::Qoi
                | Self::Hdr
                | Self::Farbfeld
                | Self::Dds
                | Self::Svg
                | Self::Avif
                | Self::Heic
        )
    }

    pub fn matches_search(self, term: &str) -> bool {
        term.is_empty()
            || self.label().to_ascii_lowercase().contains(term)
            || self.description().to_ascii_lowercase().contains(term)
            || self.extension_label().to_ascii_lowercase().contains(term)
            || self
                .canonical_extension()
                .is_some_and(|extension| extension.contains(term))
    }
}

pub fn common_format(formats: &[InputFormat]) -> InputFormat {
    let Some((first, rest)) = formats.split_first() else {
        return InputFormat::Any;
    };
    if rest.iter().all(|format| format == first) {
        *first
    } else {
        InputFormat::Mixed
    }
}

pub fn detect(path: &Path) -> InputFormat {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let mut bytes = Vec::with_capacity(SNIFF_BYTES);
    if let Ok(file) = File::open(path) {
        let _ = file.take(SNIFF_BYTES as u64).read_to_end(&mut bytes);
    }
    detect_from_bytes(name, &bytes)
}

fn detect_from_bytes(name: &str, bytes: &[u8]) -> InputFormat {
    let lower_name = name.to_ascii_lowercase();
    if let Some(format) = sniff(bytes) {
        return format;
    }
    if lower_name.ends_with(".tar.gz") || lower_name.ends_with(".tgz") {
        return InputFormat::Archive;
    }
    if let Some(format) = from_extension(&lower_name) {
        return format;
    }
    if looks_like_text(bytes) {
        return InputFormat::Text;
    }
    InputFormat::Any
}

fn from_extension(name: &str) -> Option<InputFormat> {
    let extension = Path::new(name).extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => InputFormat::Png,
        "jpg" | "jpeg" => InputFormat::Jpeg,
        "webp" => InputFormat::Webp,
        "gif" => InputFormat::Gif,
        "bmp" => InputFormat::Bmp,
        "tif" | "tiff" => InputFormat::Tiff,
        "ico" => InputFormat::Ico,
        "pnm" | "pbm" | "pgm" | "ppm" | "pam" => InputFormat::Pnm,
        "tga" => InputFormat::Tga,
        "qoi" => InputFormat::Qoi,
        "hdr" => InputFormat::Hdr,
        "ff" => InputFormat::Farbfeld,
        "dds" => InputFormat::Dds,
        "svg" => InputFormat::Svg,
        "avif" => InputFormat::Avif,
        "heic" | "heif" => InputFormat::Heic,
        "txt" => InputFormat::Text,
        "csv" => InputFormat::Csv,
        "json" => InputFormat::Json,
        "yaml" | "yml" => InputFormat::Yaml,
        "toml" => InputFormat::Toml,
        "pdf" => InputFormat::Pdf,
        "wav" => InputFormat::Wav,
        "mp3" => InputFormat::Mp3,
        "flac" => InputFormat::Flac,
        "ogg" | "oga" => InputFormat::Ogg,
        "opus" => InputFormat::Opus,
        "m4a" | "alac" => InputFormat::M4a,
        "aac" => InputFormat::Aac,
        "aiff" | "aif" => InputFormat::Aiff,
        "ac3" | "eac3" | "ec3" => InputFormat::Ac3,
        "mp4" | "m4v" | "mov" | "mkv" | "webm" => InputFormat::Video,
        "zip" | "tar" | "gz" | "tgz" | "cbz" | "cbt" => InputFormat::Archive,
        "rar" | "cbr" => InputFormat::Rar,
        "7z" | "cb7" => InputFormat::SevenZip,
        "ttf" => InputFormat::Ttf,
        "otf" => InputFormat::Otf,
        "woff" => InputFormat::Woff,
        "woff2" => InputFormat::Woff2,
        "ttc" | "otc" => InputFormat::Ttf,
        _ => return None,
    })
}

fn is_ogg_opus(bytes: &[u8]) -> bool {
    bytes.starts_with(b"OggS") && bytes.get(28..36).is_some_and(|magic| magic == b"OpusHead")
}

fn sniff(bytes: &[u8]) -> Option<InputFormat> {
    if crate::archive_readers::is_rar(bytes) {
        return Some(InputFormat::Rar);
    }
    if crate::archive_readers::is_seven_zip(bytes) {
        return Some(InputFormat::SevenZip);
    }
    if let Some(font) = crate::fonts::sniff(bytes) {
        return Some(match font {
            crate::fonts::FontContainer::TrueType | crate::fonts::FontContainer::Collection => {
                InputFormat::Ttf
            }
            crate::fonts::FontContainer::Cff => InputFormat::Otf,
            crate::fonts::FontContainer::Woff => InputFormat::Woff,
            crate::fonts::FontContainer::Woff2 => InputFormat::Woff2,
        });
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some(InputFormat::Png);
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(InputFormat::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(InputFormat::Gif);
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" {
        match &bytes[8..12] {
            b"WEBP" => return Some(InputFormat::Webp),
            b"WAVE" => return Some(InputFormat::Wav),
            _ => {}
        }
    }
    if bytes.len() >= 14 && bytes.starts_with(b"BM") && bytes[6..10] == [0, 0, 0, 0] {
        return Some(InputFormat::Bmp);
    }
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return Some(InputFormat::Tiff);
    }
    if bytes.starts_with(&[0, 0, 1, 0]) {
        return Some(InputFormat::Ico);
    }
    if bytes.len() >= 3
        && bytes[0] == b'P'
        && (b'1'..=b'6').contains(&bytes[1])
        && bytes[2].is_ascii_whitespace()
    {
        return Some(InputFormat::Pnm);
    }
    if bytes.starts_with(b"qoif") {
        return Some(InputFormat::Qoi);
    }
    if bytes.starts_with(b"#?RADIANCE") || bytes.starts_with(b"#?RGBE") {
        return Some(InputFormat::Hdr);
    }
    if bytes.starts_with(b"farbfeld") {
        return Some(InputFormat::Farbfeld);
    }
    if bytes.starts_with(b"DDS ") {
        return Some(InputFormat::Dds);
    }
    if bytes.starts_with(b"%PDF-") {
        return Some(InputFormat::Pdf);
    }
    if bytes.starts_with(b"fLaC") {
        return Some(InputFormat::Flac);
    }
    if is_ogg_opus(bytes) {
        return Some(InputFormat::Opus);
    }
    if bytes.starts_with(b"OggS") {
        return Some(InputFormat::Ogg);
    }
    if bytes.starts_with(b"ID3") {
        return Some(InputFormat::Mp3);
    }
    if bytes.starts_with(&[0x0B, 0x77]) {
        return Some(InputFormat::Ac3);
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        if crate::shared::is_avif_input(bytes) {
            return Some(InputFormat::Avif);
        }
        if crate::shared::is_heic_input(bytes) {
            return Some(InputFormat::Heic);
        }
        return Some(match &bytes[8..12] {
            b"M4A " | b"M4B " | b"M4P " => InputFormat::M4a,
            _ => InputFormat::Video,
        });
    }
    if bytes.len() >= 12 && bytes.starts_with(b"FORM") && &bytes[8..12] == b"AIFF" {
        return Some(InputFormat::Aiff);
    }
    if bytes.starts_with(&[0x1f, 0x8b]) {
        return Some(InputFormat::Archive);
    }
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        return Some(InputFormat::Archive);
    }
    if bytes.len() >= 262 && &bytes[257..262] == b"ustar" {
        return Some(InputFormat::Archive);
    }
    if looks_like_svg(bytes) {
        return Some(InputFormat::Svg);
    }
    None
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let lower = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    let trimmed = lower.trim_start_matches('\u{feff}').trim_start();
    (trimmed.starts_with("<svg") || trimmed.starts_with("<?xml")) && lower.contains("<svg")
}

fn looks_like_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.contains(&0) {
        return false;
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let total = text.chars().count();
    let printable = text
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .count();
    printable * 10 >= total * 9
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_extensions() {
        let cases = [
            ("photo.png", InputFormat::Png),
            ("photo.jpg", InputFormat::Jpeg),
            ("photo.jpeg", InputFormat::Jpeg),
            ("photo.webp", InputFormat::Webp),
            ("photo.gif", InputFormat::Gif),
            ("photo.bmp", InputFormat::Bmp),
            ("photo.tif", InputFormat::Tiff),
            ("photo.tiff", InputFormat::Tiff),
            ("icon.ico", InputFormat::Ico),
            ("image.pnm", InputFormat::Pnm),
            ("image.pbm", InputFormat::Pnm),
            ("image.pgm", InputFormat::Pnm),
            ("image.ppm", InputFormat::Pnm),
            ("image.pam", InputFormat::Pnm),
            ("image.tga", InputFormat::Tga),
            ("image.qoi", InputFormat::Qoi),
            ("image.hdr", InputFormat::Hdr),
            ("image.ff", InputFormat::Farbfeld),
            ("image.dds", InputFormat::Dds),
            ("image.svg", InputFormat::Svg),
            ("image.avif", InputFormat::Avif),
            ("image.heic", InputFormat::Heic),
            ("image.heif", InputFormat::Heic),
            ("notes.txt", InputFormat::Text),
            ("data.csv", InputFormat::Csv),
            ("data.json", InputFormat::Json),
            ("data.yaml", InputFormat::Yaml),
            ("data.yml", InputFormat::Yaml),
            ("data.toml", InputFormat::Toml),
            ("document.pdf", InputFormat::Pdf),
            ("song.wav", InputFormat::Wav),
            ("song.mp3", InputFormat::Mp3),
            ("song.flac", InputFormat::Flac),
            ("song.ogg", InputFormat::Ogg),
            ("song.oga", InputFormat::Ogg),
            ("song.opus", InputFormat::Opus),
            ("song.m4a", InputFormat::M4a),
            ("song.aac", InputFormat::Aac),
            ("song.aiff", InputFormat::Aiff),
            ("song.aif", InputFormat::Aiff),
            ("movie.ac3", InputFormat::Ac3),
            ("movie.eac3", InputFormat::Ac3),
            ("movie.ec3", InputFormat::Ac3),
            ("movie.mp4", InputFormat::Video),
            ("movie.m4v", InputFormat::Video),
            ("movie.mov", InputFormat::Video),
            ("movie.mkv", InputFormat::Video),
            ("movie.webm", InputFormat::Video),
            ("song.alac", InputFormat::M4a),
            ("archive.zip", InputFormat::Archive),
            ("archive.tar", InputFormat::Archive),
            ("archive.tar.gz", InputFormat::Archive),
            ("archive.tgz", InputFormat::Archive),
            ("archive.gz", InputFormat::Archive),
            ("archive.rar", InputFormat::Rar),
            ("comic.cbr", InputFormat::Rar),
            ("archive.7z", InputFormat::SevenZip),
            ("comic.cb7", InputFormat::SevenZip),
            ("comic.cbz", InputFormat::Archive),
            ("comic.cbt", InputFormat::Archive),
        ];
        for (name, expected) in cases {
            assert_eq!(detect_from_bytes(name, b""), expected, "{name}");
        }
    }

    #[test]
    fn covers_every_supported_input_extension() {
        for extension in crate::supported_input_extensions() {
            let format = detect_from_bytes(&format!("sample.{extension}"), b"");
            assert_ne!(format, InputFormat::Any, ".{extension} was not mapped");
            assert_ne!(format, InputFormat::Mixed, ".{extension} mapped to mixed");
        }
    }

    #[test]
    fn content_signatures_win_over_misleading_extensions() {
        let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        assert_eq!(detect_from_bytes("mystery.jpg", &png), InputFormat::Png);

        let jpeg = [0xff, 0xd8, 0xff, 0xe0];
        assert_eq!(detect_from_bytes("mystery.png", &jpeg), InputFormat::Jpeg);

        assert_eq!(
            detect_from_bytes("mystery.zip", b"Rar!\x1a\x07\x00"),
            InputFormat::Rar
        );
        assert_eq!(
            detect_from_bytes("mystery.zip.gz", b"7z\xbc\xaf\x27\x1c\x00\x04"),
            InputFormat::SevenZip
        );

        let wav = b"RIFF\x24\x00\x00\x00WAVEfmt ";
        assert_eq!(detect_from_bytes("mystery.bin", wav), InputFormat::Wav);

        let zip = b"PK\x03\x04";
        assert_eq!(detect_from_bytes("mystery.txt", zip), InputFormat::Archive);

        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"></svg>"#;
        assert_eq!(detect_from_bytes("mystery.bin", svg), InputFormat::Svg);

        let mp4 = b"\0\0\0\x18ftypM4A ";
        assert_eq!(detect_from_bytes("mystery.bin", mp4), InputFormat::M4a);

        let video = b"\0\0\0\x18ftypisom";
        assert_eq!(detect_from_bytes("mystery.bin", video), InputFormat::Video);

        let avif = b"\0\0\0\x20ftypavif\0\0\0\0avifmif1miaf";
        assert_eq!(detect_from_bytes("mystery.bin", avif), InputFormat::Avif);
        assert_eq!(
            detect_from_bytes("mystery.avif", &[0, 1, 2, 3]),
            InputFormat::Avif
        );

        let heic = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic";
        assert_eq!(detect_from_bytes("mystery.bin", heic), InputFormat::Heic);
        assert_eq!(
            detect_from_bytes("mystery.heic", &[0, 1, 2, 3]),
            InputFormat::Heic
        );

        let aiff = b"FORM\0\0\0\0AIFFCOMM";
        assert_eq!(detect_from_bytes("mystery.bin", aiff), InputFormat::Aiff);

        let mut ogg_opus = vec![0_u8; 256];
        ogg_opus[..4].copy_from_slice(b"OggS");
        ogg_opus[28..36].copy_from_slice(b"OpusHead");
        assert_eq!(
            detect_from_bytes("mystery.ogg", &ogg_opus),
            InputFormat::Opus
        );
    }

    #[test]
    fn falls_back_to_text_or_any() {
        assert_eq!(
            detect_from_bytes("mystery.bin", b"hello world"),
            InputFormat::Text
        );
        assert_eq!(
            detect_from_bytes("mystery.bin", &[0, 1, 2, 3]),
            InputFormat::Any
        );
        assert_eq!(detect_from_bytes("mystery.bin", b""), InputFormat::Any);
    }

    #[test]
    fn detects_archive_headers() {
        let mut tar = vec![0_u8; 512];
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(detect_from_bytes("mystery.bin", &tar), InputFormat::Archive);

        let gzip = [0x1f, 0x8b, 0x08];
        assert_eq!(
            detect_from_bytes("mystery.bin", &gzip),
            InputFormat::Archive
        );
    }

    #[test]
    fn common_format_handles_empty_and_mixed_inputs() {
        assert_eq!(common_format(&[]), InputFormat::Any);
        assert_eq!(
            common_format(&[InputFormat::Png, InputFormat::Png]),
            InputFormat::Png
        );
        assert_eq!(
            common_format(&[InputFormat::Png, InputFormat::Jpeg]),
            InputFormat::Mixed
        );
    }
}
