// SPDX-License-Identifier: GPL-2.0-or-later

//! Fixtures and helpers shared by more than one test module.

use crate::audio::Mp3Bitrate;
use crate::format::OutputFormat;
use crate::options::ConversionOptions;
use std::{
    io::Cursor,
    path::{Path, PathBuf},
};

/// Abel, an OFL text font with TrueType outlines.
pub(crate) fn ttf_font_fixture() -> Vec<u8> {
    include_bytes!("../testdata/abel-regular.ttf").to_vec()
}

/// Source Code Pro, an OFL text font with CFF outlines.
pub(crate) fn otf_font_fixture() -> Vec<u8> {
    include_bytes!("../testdata/source-code-pro-regular.otf").to_vec()
}

/// Abel's latin subset as Google Fonts serves it: a WOFF2 with the `glyf`
/// transform applied, not just a recompressed TTF.
pub(crate) fn woff2_font_fixture() -> Vec<u8> {
    include_bytes!("../testdata/abel-regular.woff2").to_vec()
}

pub(crate) fn png_fixture() -> Vec<u8> {
    let image =
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([20, 40, 60])));
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    bytes.into_inner()
}

pub(crate) fn wav_fixture() -> Vec<u8> {
    wav_fixture_at(8_000, 8_000)
}

pub(crate) fn wav_fixture_at(sample_rate: u32, frames: u32) -> Vec<u8> {
    wav_tone_at(sample_rate, frames, 2, 440.0, 0.5)
}

pub(crate) fn wav_tone_at(
    sample_rate: u32,
    frames: u32,
    channels: u16,
    frequency: f64,
    amplitude: f64,
) -> Vec<u8> {
    let mut samples = Vec::with_capacity(frames as usize * usize::from(channels));
    for frame in 0..frames {
        let value = ((frame as f64 / f64::from(sample_rate) * frequency * std::f64::consts::TAU)
            .sin()
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
pub(crate) fn wav_channels_at(sample_rate: u32, frames: usize, channels: &[f32]) -> Vec<u8> {
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
pub(crate) fn wav_24_bit_at(sample_rate: u32, channels: u16, samples: &[i32]) -> Vec<u8> {
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

pub(crate) fn wav_24_bit_samples(bytes: &[u8]) -> Vec<i32> {
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

pub(crate) fn wav_float_samples(bytes: &[u8]) -> Vec<f32> {
    let (format, _, _, bits, offset, size) = wav_layout(bytes);
    assert_eq!((format, bits), (3, 32));
    bytes[offset..offset + size]
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect()
}

pub(crate) fn mp3_first_frame_bitrate(bytes: &[u8]) -> Option<u16> {
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

pub(crate) fn assert_mp3_tone(
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
    let decoded = crate::convert::convert_bytes("tone.mp3", mp3, OutputFormat::Wav).unwrap();
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

pub(crate) struct ParsedWav {
    pub(crate) channels: u16,
    pub(crate) sample_rate: u32,
    pub(crate) bits_per_sample: u16,
    pub(crate) samples: Vec<i16>,
}

pub(crate) fn parse_wav(bytes: &[u8]) -> ParsedWav {
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

pub(crate) fn assert_audible(samples: &[i16]) {
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

pub(crate) fn fixture_path(name: &str) -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

/// Returns (format tag, channels, sample rate, bits, data offset, data size).
pub(crate) fn wav_layout(bytes: &[u8]) -> (u16, u16, u32, u16, usize, usize) {
    assert!(bytes.len() >= 12);
    let mut offset = 12;
    let mut format: (u16, u16, u32, u16) = (0, 0, 0, 0);
    let mut data: (usize, usize) = (0, 0);
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
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

pub(crate) fn stream_temp_dir(name: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("convert2it-stream-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

pub(crate) fn stream_wav(input: &Path, options: ConversionOptions) -> Vec<u8> {
    let mut output = std::io::Cursor::new(Vec::new());
    let cancel = std::sync::atomic::AtomicBool::new(false);
    crate::streaming::convert_audio_file_to_writer_with_cancel(
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

pub(crate) fn rms(samples: &[f32]) -> f64 {
    (samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt()
}

pub(crate) fn tiff_with_orientations(orientations: &[u16]) -> Vec<u8> {
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

/// A ZIP container holding `entries` in the given order. `mimetype` is stored
/// uncompressed, which is what an EPUB container requires of its first entry;
/// everything else is deflated.
pub(crate) fn zip_with_entries(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    use std::io::Write;

    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        let method = if name == "mimetype" {
            zip::CompressionMethod::Stored
        } else {
            zip::CompressionMethod::Deflated
        };
        writer
            .start_file(
                name.clone(),
                zip::write::SimpleFileOptions::default().compression_method(method),
            )
            .unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// One chapter of a generated EPUB fixture.
pub(crate) struct EpubFixtureChapter {
    pub(crate) label: String,
    pub(crate) href: String,
    pub(crate) body: String,
}

const EPUB_CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"#;

fn epub_chapters(bodies: &[&str]) -> Vec<EpubFixtureChapter> {
    bodies
        .iter()
        .enumerate()
        .map(|(index, body)| EpubFixtureChapter {
            label: match index {
                0 => "Chapter One".to_owned(),
                1 => "Chapter Two".to_owned(),
                other => format!("Chapter {}", other + 1),
            },
            href: format!("ch{}.xhtml", index + 1),
            body: (*body).to_owned(),
        })
        .collect()
}

fn epub_document(label: &str, body: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <html xmlns=\"http://www.w3.org/1999/xhtml\">\n\
         <head><title>{label}</title></head>\n\
         <body>\n{body}\n</body>\n</html>\n"
    )
}

fn epub_ncx(chapters: &[EpubFixtureChapter]) -> String {
    let mut nav_map = String::new();
    for (index, chapter) in chapters.iter().enumerate() {
        nav_map.push_str(&format!(
            "<navPoint id=\"nav{}\" playOrder=\"{}\"><navLabel><text>{}</text></navLabel><content src=\"{}\"/></navPoint>\n",
            index + 1,
            index + 1,
            chapter.label,
            chapter.href
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <ncx xmlns=\"http://www.daisy.org/z3986/2005/ncx/\" version=\"2005-1\">\n\
         <head><meta name=\"dtb:uid\" content=\"urn:uuid:convert2it-fixture\"/></head>\n\
         <docTitle><text>Fixture Book</text></docTitle>\n\
         <navMap>{nav_map}</navMap>\n</ncx>\n"
    )
}

fn epub_nav(chapters: &[EpubFixtureChapter]) -> String {
    let mut items = String::new();
    for chapter in chapters {
        items.push_str(&format!(
            "<li><a href=\"{}\">{}</a></li>\n",
            chapter.href, chapter.label
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\">\n\
         <head><title>Contents</title></head>\n\
         <body><nav epub:type=\"toc\" id=\"toc\"><h1>Contents</h1><ol>\n{items}</ol></nav></body>\n</html>\n"
    )
}

/// Builds an EPUB 2 or 3 container by hand: a stored `mimetype` first, the
/// package with the given chapters and extra manifest items, a navigation
/// document for EPUB 3 or an NCX for EPUB 2, and whatever extra entries the
/// caller needs. Every byte of it is written here from the specification, so a
/// reader test does not depend on the writer under test.
pub(crate) fn build_epub(
    version: u8,
    chapters: &[EpubFixtureChapter],
    extra_entries: &[(String, Vec<u8>)],
    extra_manifest: &str,
) -> Vec<u8> {
    let mut entries: Vec<(String, Vec<u8>)> = vec![
        ("mimetype".to_owned(), b"application/epub+zip".to_vec()),
        (
            "META-INF/container.xml".to_owned(),
            EPUB_CONTAINER_XML.as_bytes().to_vec(),
        ),
    ];

    let mut manifest = String::new();
    let mut spine = String::new();
    for (index, chapter) in chapters.iter().enumerate() {
        manifest.push_str(&format!(
            "<item id=\"ch{}\" href=\"{}\" media-type=\"application/xhtml+xml\"/>\n",
            index + 1,
            chapter.href
        ));
        spine.push_str(&format!("<itemref idref=\"ch{}\"/>\n", index + 1));
        entries.push((
            format!("OEBPS/{}", chapter.href),
            epub_document(&chapter.label, &chapter.body).into_bytes(),
        ));
    }
    manifest.push_str(extra_manifest);

    let spine_attribute = if version == 2 {
        manifest.push_str(
            "<item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n",
        );
        entries.push(("OEBPS/toc.ncx".to_owned(), epub_ncx(chapters).into_bytes()));
        " toc=\"ncx\""
    } else {
        manifest.push_str(
            "<item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n",
        );
        entries.push((
            "OEBPS/nav.xhtml".to_owned(),
            epub_nav(chapters).into_bytes(),
        ));
        ""
    };

    let modified = if version == 2 {
        String::new()
    } else {
        "<meta property=\"dcterms:modified\">2020-01-01T00:00:00Z</meta>\n".to_owned()
    };
    let package = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <package xmlns=\"http://www.idpf.org/2007/opf\" version=\"{version}.0\" unique-identifier=\"uid\">\n\
         <metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n\
         <dc:title>Fixture Book</dc:title>\n\
         <dc:language>en</dc:language>\n\
         <dc:identifier id=\"uid\">urn:uuid:convert2it-fixture</dc:identifier>\n\
         {modified}</metadata>\n\
         <manifest>\n{manifest}</manifest>\n\
         <spine{spine_attribute}>\n{spine}</spine>\n\
         </package>\n"
    );
    entries.push(("OEBPS/content.opf".to_owned(), package.into_bytes()));
    entries.extend(extra_entries.iter().cloned());
    zip_with_entries(&entries)
}

const EPUB_CHAPTER_ONE: &str = "<p>A quiet beginning.</p>\n\
    <p>We had tea &amp; cake.<br/>first line<br/>second line</p>\n\
    <p>Some <em>quiet</em> emphasis.</p>";

const EPUB_CHAPTER_TWO: &str = "<h2>The second chapter</h2>\n<p>It continues.</p>";

/// A two-chapter EPUB 2 with an NCX and no navigation document.
pub(crate) fn epub_fixture() -> Vec<u8> {
    build_epub(
        2,
        &epub_chapters(&[EPUB_CHAPTER_ONE, EPUB_CHAPTER_TWO]),
        &[],
        "",
    )
}

/// The same book as EPUB 3: a navigation document in the manifest and no NCX.
pub(crate) fn epub3_fixture() -> Vec<u8> {
    build_epub(
        3,
        &epub_chapters(&[EPUB_CHAPTER_ONE, EPUB_CHAPTER_TWO]),
        &[],
        "",
    )
}

/// An EPUB 2 whose chapters are exactly `bodies`, which is how a book with no
/// readable text and a book with one chapter are both generated.
pub(crate) fn epub2_with_bodies(bodies: &[&str]) -> Vec<u8> {
    build_epub(2, &epub_chapters(bodies), &[], "")
}

/// An EPUB 3 carrying one image, referenced from the first chapter.
pub(crate) fn epub_with_image() -> Vec<u8> {
    let mut chapters = epub_chapters(&[
        "<p>A picture follows.</p><p><img src=\"img/pic.png\" alt=\"dot\"/></p>",
        EPUB_CHAPTER_TWO,
    ]);
    chapters[0].href = "text/ch1.xhtml".to_owned();
    build_epub(
        3,
        &chapters,
        &[("OEBPS/text/img/pic.png".to_owned(), png_fixture())],
        "<item id=\"pic\" href=\"text/img/pic.png\" media-type=\"image/png\"/>\n",
    )
}

/// An EPUB 3 with a `META-INF/encryption.xml` holding `method_xml`, and
/// optionally an Adobe `rights.xml` beside it.
pub(crate) fn epub_with_encryption(method_xml: &str, rights: bool) -> Vec<u8> {
    let encryption = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <encryption xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\" \
         xmlns:enc=\"http://www.w3.org/2001/04/xmlenc#\">\n\
         <enc:EncryptedData>\n{method_xml}\n\
         <enc:CipherData><enc:CipherReference URI=\"OEBPS/ch1.xhtml\"/></enc:CipherData>\n\
         </enc:EncryptedData>\n</encryption>\n"
    );
    let mut extra = vec![(
        "META-INF/encryption.xml".to_owned(),
        encryption.into_bytes(),
    )];
    if rights {
        extra.push((
            "META-INF/rights.xml".to_owned(),
            b"<?xml version=\"1.0\"?><rights/>".to_vec(),
        ));
    }
    build_epub(
        3,
        &epub_chapters(&[EPUB_CHAPTER_ONE, EPUB_CHAPTER_TWO]),
        &extra,
        "",
    )
}
