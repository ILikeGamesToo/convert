// SPDX-License-Identifier: GPL-2.0-or-later

//! Tag carry-over for audio conversions.
//!
//! Tags are read from whatever container the source uses (ID3v2 in MP3,
//! Vorbis comments in FLAC and Ogg, the iTunes `ilst` atoms in M4A/AAC) and
//! written into the target's own tagging scheme:
//!
//! - MP3 gets an ID3v2.3 tag with text frames and an `APIC` cover.
//! - FLAC gets a Vorbis comment block and `PICTURE` blocks.
//! - Opus gets an `OpusTags` packet (Vorbis comments) plus a
//!   `METADATA_BLOCK_PICTURE` comment for the cover.
//!
//! WAV, AIFF and AU have no tag container this app writes, so a conversion to
//! them drops tags by design. An MP3 input re-encoded to MP3 keeps its whole
//! original ID3v2 tag verbatim instead of being rebuilt, which is what makes
//! ReplayGain and other frames survive; the same is true of FLAC to FLAC.

use crate::audio::{flac_metadata_blocks, flac_metadata_end, rebuild_flac};
use std::io::Cursor;
use symphonia::core::meta::{StandardTagKey, StandardVisualKey, Value};

/// The descriptive fields this app carries between audio containers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct AudioTags {
    pub(crate) title: Option<String>,
    pub(crate) artist: Option<String>,
    pub(crate) album: Option<String>,
    pub(crate) album_artist: Option<String>,
    pub(crate) track: Option<u32>,
    pub(crate) track_total: Option<u32>,
    pub(crate) genre: Option<String>,
    pub(crate) date: Option<String>,
    pub(crate) comment: Option<String>,
    pub(crate) cover: Option<CoverArt>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CoverArt {
    pub(crate) media_type: String,
    pub(crate) data: Vec<u8>,
}

impl AudioTags {
    pub(crate) fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// Reads the tags from an audio input. A file whose tags cannot be read (an
/// unrecognised container, a codec without a metadata reader) simply yields
/// the empty set rather than failing the conversion.
///
/// ID3v2 is read by the probe rather than by the format reader, because an
/// MP3's tag lives before the container the reader sees; the probe hands those
/// tags back separately from the format's own metadata.
pub(crate) fn read(input: &[u8], extension: &str) -> AudioTags {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let source = MediaSourceStream::new(Box::new(Cursor::new(input.to_vec())), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(extension);
    let Ok(probed) = symphonia::default::get_probe().format(
        &hint,
        source,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    ) else {
        return AudioTags::default();
    };
    let mut probed_metadata = probed.metadata;
    let mut format = probed.format;

    let mut tags = AudioTags::default();
    let mut track_total = None;
    if let Some(metadata) = probed_metadata.get() {
        if let Some(revision) = metadata.current() {
            apply_revision(revision, &mut tags, &mut track_total);
        }
    }
    if let Some(revision) = format.metadata().current() {
        apply_revision(revision, &mut tags, &mut track_total);
    }
    tags.track_total = track_total;
    tags
}

fn apply_revision(
    revision: &symphonia::core::meta::MetadataRevision,
    tags: &mut AudioTags,
    track_total: &mut Option<u32>,
) {
    for tag in revision.tags() {
        let Some(key) = tag.std_key else {
            continue;
        };
        let Some(text) = value_text(&tag.value) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        match key {
            StandardTagKey::TrackTitle => {
                tags.title.get_or_insert(text);
            }
            StandardTagKey::Artist => {
                tags.artist.get_or_insert(text);
            }
            StandardTagKey::Album => {
                tags.album.get_or_insert(text);
            }
            StandardTagKey::AlbumArtist => {
                tags.album_artist.get_or_insert(text);
            }
            StandardTagKey::Genre => {
                tags.genre.get_or_insert(text);
            }
            StandardTagKey::Date | StandardTagKey::OriginalDate => {
                tags.date.get_or_insert(text);
            }
            StandardTagKey::Comment => {
                tags.comment.get_or_insert(text);
            }
            StandardTagKey::TrackNumber => {
                let (number, total) = split_track(&text);
                tags.track = tags.track.or(number);
                *track_total = track_total.or(total);
            }
            StandardTagKey::TrackTotal => {
                *track_total = track_total.or(text.parse().ok());
            }
            _ => {}
        }
    }
    if tags.cover.is_none() {
        tags.cover = revision
            .visuals()
            .iter()
            .find(|visual| visual.usage == Some(StandardVisualKey::FrontCover))
            .or_else(|| revision.visuals().first())
            .filter(|visual| !visual.data.is_empty())
            .map(|visual| CoverArt {
                media_type: if visual.media_type.is_empty() {
                    sniff_image_type(&visual.data).to_owned()
                } else {
                    visual.media_type.clone()
                },
                data: visual.data.to_vec(),
            });
    }
}

fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::UnsignedInt(number) => Some(number.to_string()),
        Value::SignedInt(number) => Some(number.to_string()),
        _ => None,
    }
}

fn split_track(text: &str) -> (Option<u32>, Option<u32>) {
    let mut parts = text.split('/');
    let number = parts.next().and_then(|part| part.trim().parse().ok());
    let total = parts.next().and_then(|part| part.trim().parse().ok());
    (number, total)
}

fn sniff_image_type(data: &[u8]) -> &'static str {
    if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else if data.starts_with(b"GIF8") {
        "image/gif"
    } else {
        "image/jpeg"
    }
}

/// The Vorbis comment body shared by FLAC and Opus.
fn vorbis_comments(tags: &AudioTags) -> Vec<(String, String)> {
    let mut comments = Vec::new();
    let mut push = |name: &str, value: &Option<String>| {
        if let Some(value) = value {
            comments.push((name.to_owned(), value.clone()));
        }
    };
    push("TITLE", &tags.title);
    push("ARTIST", &tags.artist);
    push("ALBUM", &tags.album);
    push("ALBUMARTIST", &tags.album_artist);
    push("GENRE", &tags.genre);
    push("DATE", &tags.date);
    push("COMMENT", &tags.comment);
    if let Some(track) = tags.track {
        comments.push(("TRACKNUMBER".to_owned(), track.to_string()));
    }
    if let Some(total) = tags.track_total {
        comments.push(("TRACKTOTAL".to_owned(), total.to_string()));
    }
    comments
}

/// A FLAC `PICTURE` block, which the Opus comment block also embeds (base64
/// encoded under `METADATA_BLOCK_PICTURE`).
fn picture_block(cover: &CoverArt) -> Vec<u8> {
    let mut block = Vec::new();
    block.extend_from_slice(&3_u32.to_be_bytes()); // front cover
    block.extend_from_slice(&(cover.media_type.len() as u32).to_be_bytes());
    block.extend_from_slice(cover.media_type.as_bytes());
    block.extend_from_slice(&0_u32.to_be_bytes()); // empty description
    for value in [0_u32; 4] {
        // width, height, colour depth, indexed-colour count: unknown.
        block.extend_from_slice(&value.to_be_bytes());
    }
    block.extend_from_slice(&(cover.data.len() as u32).to_be_bytes());
    block.extend_from_slice(&cover.data);
    block
}

/// Prepends an ID3v2.3 tag built from `tags` to an MP3 stream.
pub(crate) fn attach_mp3(output: Vec<u8>, tags: &AudioTags) -> Vec<u8> {
    let mut frames = Vec::new();
    let append_text = |frames: &mut Vec<u8>, id: &[u8; 4], value: &Option<String>| {
        if let Some(value) = value {
            if let Some(frame) = id3_text_frame(id, value) {
                frames.extend_from_slice(&frame);
            }
        }
    };
    append_text(&mut frames, b"TIT2", &tags.title);
    append_text(&mut frames, b"TPE1", &tags.artist);
    append_text(&mut frames, b"TALB", &tags.album);
    append_text(&mut frames, b"TPE2", &tags.album_artist);
    append_text(&mut frames, b"TCON", &tags.genre);
    if let Some(track) = tags.track {
        let value = match tags.track_total {
            Some(total) => format!("{track}/{total}"),
            None => track.to_string(),
        };
        if let Some(frame) = id3_text_frame(b"TRCK", &value) {
            frames.extend_from_slice(&frame);
        }
    }
    append_text(
        &mut frames,
        b"TYER",
        &tags.date.as_deref().and_then(year).map(str::to_owned),
    );
    if let Some(comment) = &tags.comment {
        if let Some(frame) = id3_comment_frame(comment) {
            frames.extend_from_slice(&frame);
        }
    }
    if let Some(cover) = &tags.cover {
        if let Some(frame) = id3_picture_frame(cover) {
            frames.extend_from_slice(&frame);
        }
    }
    if frames.is_empty() {
        return output;
    }

    let mut tag = Vec::with_capacity(10 + frames.len());
    tag.extend_from_slice(b"ID3\x03\x00\x00");
    tag.extend_from_slice(&synchsafe(frames.len()));
    tag.extend_from_slice(&frames);
    tag.extend_from_slice(&output);
    tag
}

fn year(date: &str) -> Option<&str> {
    let year = date.get(..4)?;
    year.chars()
        .all(|character| character.is_ascii_digit())
        .then_some(year)
}

fn id3_text_frame(id: &[u8; 4], text: &str) -> Option<Vec<u8>> {
    let mut payload = vec![1_u8];
    push_utf16(&mut payload, text);
    Some(id3_frame(id, payload))
}

fn id3_comment_frame(text: &str) -> Option<Vec<u8>> {
    let mut payload = vec![1_u8];
    payload.extend_from_slice(b"eng");
    push_utf16(&mut payload, "");
    push_utf16(&mut payload, text);
    Some(id3_frame(b"COMM", payload))
}

fn id3_picture_frame(cover: &CoverArt) -> Option<Vec<u8>> {
    let mut payload = vec![0_u8];
    payload.extend_from_slice(cover.media_type.as_bytes());
    payload.push(0);
    payload.push(3); // front cover
    payload.push(0); // empty description
    payload.extend_from_slice(&cover.data);
    Some(id3_frame(b"APIC", payload))
}

fn id3_frame(id: &[u8; 4], payload: Vec<u8>) -> Vec<u8> {
    let mut frame = Vec::with_capacity(10 + payload.len());
    frame.extend_from_slice(id);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&payload);
    frame
}

/// UTF-16 with a little-endian BOM, which ID3v2.3 accepts for every text
/// frame and which covers non-ASCII values that ISO-8859-1 cannot.
fn push_utf16(output: &mut Vec<u8>, text: &str) {
    output.extend_from_slice(&[0xFF, 0xFE]);
    for unit in text.encode_utf16() {
        output.extend_from_slice(&unit.to_le_bytes());
    }
}

fn synchsafe(length: usize) -> [u8; 4] {
    [
        ((length >> 21) & 0x7F) as u8,
        ((length >> 14) & 0x7F) as u8,
        ((length >> 7) & 0x7F) as u8,
        (length & 0x7F) as u8,
    ]
}

/// Replaces a FLAC output's descriptive blocks with ones built from `tags`.
pub(crate) fn attach_flac(output: Vec<u8>, tags: &AudioTags) -> Vec<u8> {
    if tags.is_empty() {
        return output;
    }
    let Some(mut blocks) = flac_metadata_blocks(&output) else {
        return output;
    };
    let Some(audio_start) = flac_metadata_end(&output) else {
        return output;
    };
    blocks.retain(|(kind, _)| *kind != 4 && *kind != 6);

    let mut comment = Vec::new();
    let vendor = b"convert2it";
    comment.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    comment.extend_from_slice(vendor);
    let comments = vorbis_comments(tags);
    comment.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for (name, value) in &comments {
        let entry = format!("{name}={value}");
        comment.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        comment.extend_from_slice(entry.as_bytes());
    }

    let insert_at = usize::from(blocks.first().is_some_and(|(kind, _)| *kind == 0));
    blocks.insert(insert_at, (4, comment));
    if let Some(cover) = &tags.cover {
        blocks.insert(insert_at + 1, (6, picture_block(cover)));
    }
    rebuild_flac(&blocks, &output[audio_start..])
}

/// The `OpusTags` packet contents for an Ogg Opus output.
pub(crate) fn opus_tags(tags: &AudioTags) -> opus_pure::OpusTags {
    let mut comments: Vec<String> = vorbis_comments(tags)
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    if let Some(cover) = &tags.cover {
        comments.push(format!(
            "METADATA_BLOCK_PICTURE={}",
            base64(&picture_block(cover))
        ));
    }
    if comments.is_empty() {
        return opus_pure::OpusTags::new();
    }
    opus_pure::OpusTags {
        vendor: "convert2it".to_owned(),
        comments,
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(ALPHABET[(chunk[0] >> 2) as usize] as char);
        output.push(ALPHABET[(((chunk[0] & 3) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(ALPHABET[(((second & 15) << 2) | (third >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(ALPHABET[(third & 63) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::convert_bytes;
    use crate::format::OutputFormat;
    use crate::test_support::fixture_path;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(fixture_path(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
    }

    fn convert(name: &str, input: &[u8], target: OutputFormat) -> Vec<u8> {
        convert_bytes(name, input, target)
            .expect("the conversion succeeds")
            .remove(0)
            .bytes
    }

    fn tags() -> AudioTags {
        AudioTags {
            title: Some("Waves".to_owned()),
            artist: Some("Grüße".to_owned()),
            album: Some("Album".to_owned()),
            album_artist: Some("Album Artist".to_owned()),
            track: Some(3),
            track_total: Some(12),
            genre: Some("Ambient".to_owned()),
            date: Some("2026-11-24".to_owned()),
            comment: Some("comment".to_owned()),
            cover: Some(CoverArt {
                media_type: "image/png".to_owned(),
                data: vec![1, 2, 3, 4, 5],
            }),
        }
    }

    #[test]
    fn reads_the_fixture_tags_from_every_container() {
        let mp3 = read(&fixture("metadata-tone.mp3"), "mp3");
        assert_eq!(mp3.title.as_deref(), Some("Waves"));
        assert_eq!(mp3.artist.as_deref(), Some("Grüße"));
        assert_eq!(mp3.album.as_deref(), Some("Metadata Album"));
        assert_eq!(mp3.track, Some(3));
        assert_eq!(mp3.track_total, Some(12));
        assert_eq!(mp3.genre.as_deref(), Some("Ambient"));
        let cover = mp3.cover.as_ref().expect("the MP3 carries an APIC frame");
        assert_eq!(cover.media_type, "image/png");

        for (name, extension) in [
            ("metadata-tone.flac", "flac"),
            ("metadata-tone.m4a", "m4a"),
            ("metadata-tone.opus", "opus"),
        ] {
            let tags = read(&fixture(name), extension);
            assert_eq!(tags.title.as_deref(), Some("Waves"), "{name} title");
            assert_eq!(tags.artist.as_deref(), Some("Grüße"), "{name} artist");
            assert_eq!(
                tags.album.as_deref(),
                Some("Metadata Album"),
                "{name} album"
            );
            assert_eq!(tags.track, Some(3), "{name} track");
            assert_eq!(tags.genre.as_deref(), Some("Ambient"), "{name} genre");
        }
        assert!(
            read(&fixture("metadata-tone.m4a"), "m4a").cover.is_some(),
            "the M4A cover atom must be read"
        );
    }

    #[test]
    fn carries_mp3_tags_into_flac_and_opus() {
        let source = fixture("metadata-tone.mp3");
        let expected = read(&source, "mp3");
        assert!(!expected.is_empty());
        for (target, extension) in [
            (OutputFormat::Flac, "flac"),
            (OutputFormat::Opus, "opus"),
            (OutputFormat::Mp3, "mp3"),
        ] {
            let output = convert("metadata-tone.mp3", &source, target);
            let tags = read(&output, extension);
            assert_eq!(
                tags,
                expected,
                "{} did not carry the MP3 tags",
                target.label()
            );
        }
    }

    #[test]
    fn carries_flac_m4a_and_opus_tags_into_mp3() {
        for (name, extension) in [
            ("metadata-tone.flac", "flac"),
            ("metadata-tone.m4a", "m4a"),
            ("metadata-tone.opus", "opus"),
        ] {
            let source = fixture(name);
            let expected = read(&source, extension);
            assert_eq!(expected.title.as_deref(), Some("Waves"), "{name}");
            let output = convert(name, &source, OutputFormat::Mp3);
            let tags = read(&output, "mp3");
            assert_eq!(
                tags.title.as_deref(),
                Some("Waves"),
                "{name} -> MP3 lost the title"
            );
            assert_eq!(
                tags.artist.as_deref(),
                Some("Grüße"),
                "{name} -> MP3 lost the artist"
            );
            assert_eq!(
                tags.album.as_deref(),
                Some("Metadata Album"),
                "{name} -> MP3 lost the album"
            );
            assert_eq!(tags.track, Some(3), "{name} -> MP3 lost the track");
            assert_eq!(tags.genre.as_deref(), Some("Ambient"), "{name} -> MP3");
            assert_eq!(
                tags.cover, expected.cover,
                "{name} -> MP3 lost or changed the cover art"
            );

            let flac = convert(name, &source, OutputFormat::Flac);
            let flac_tags = read(&flac, "flac");
            assert_eq!(
                flac_tags.cover, expected.cover,
                "{name} -> FLAC lost or changed the cover art"
            );
        }
    }

    #[test]
    fn wav_output_has_no_tag_container() {
        let source = fixture("metadata-tone.mp3");
        let wav = convert("metadata-tone.mp3", &source, OutputFormat::Wav);
        assert!(read(&wav, "wav").is_empty());
    }

    #[test]
    fn builds_a_synchsafe_id3v2_tag() {
        let mp3 = attach_mp3(b"\xFF\xFBaudio".to_vec(), &tags());
        assert!(mp3.starts_with(b"ID3\x03\x00\x00"));
        let size = ((mp3[6] as usize) << 21)
            | ((mp3[7] as usize) << 14)
            | ((mp3[8] as usize) << 7)
            | (mp3[9] as usize);
        assert_eq!(10 + size + 7, mp3.len());
        assert_eq!(crate::audio::id3v2_len(&mp3), Some(6 + 4 + size));
        let tag = String::from_utf8_lossy(&mp3[..6 + 4 + size]);
        assert!(tag.contains("TIT2"));
        assert!(tag.contains("APIC"));
        // The audio is untouched after the tag.
        assert!(mp3.ends_with(b"\xFF\xFBaudio"));
    }

    #[test]
    fn builds_flac_comments_and_a_picture() {
        let flac = crate::test_support::wav_fixture();
        let flac =
            crate::convert::convert_bytes("tone.wav", &flac, crate::format::OutputFormat::Flac)
                .unwrap()
                .remove(0)
                .bytes;
        let tagged = attach_flac(flac.clone(), &tags());
        let blocks = crate::audio::flac_metadata_blocks(&tagged).unwrap();
        let comment = blocks.iter().find(|(kind, _)| *kind == 4).unwrap();
        let comment_text = String::from_utf8_lossy(&comment.1);
        assert!(comment_text.contains("TITLE=Waves"));
        assert!(comment_text.contains("TRACKNUMBER=3"));
        let picture = blocks.iter().find(|(kind, _)| *kind == 6).unwrap();
        assert_eq!(picture.1[..4], 3_u32.to_be_bytes());
        assert_eq!(&picture.1[picture.1.len() - 5..], &[1, 2, 3, 4, 5]);
        // The audio frames stay byte-identical.
        assert_eq!(
            &tagged[crate::audio::flac_metadata_end(&tagged).unwrap()..],
            &flac[crate::audio::flac_metadata_end(&flac).unwrap()..]
        );
    }

    #[test]
    fn encodes_cover_art_for_opus() {
        let tags = tags();
        let opus = opus_tags(&tags);
        assert!(opus.comments.iter().any(|entry| entry == "TITLE=Waves"));
        let picture = opus
            .comments
            .iter()
            .find_map(|entry| entry.strip_prefix("METADATA_BLOCK_PICTURE="))
            .unwrap();
        assert!(picture.starts_with("AAAAAwAA"));
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn splits_track_numbers() {
        assert_eq!(split_track("3/12"), (Some(3), Some(12)));
        assert_eq!(split_track("7"), (Some(7), None));
        assert_eq!(split_track(""), (None, None));
    }
}
