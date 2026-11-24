// SPDX-License-Identifier: GPL-2.0-or-later

//! Audio decoding, channel mixing, resampling and encoding.

use crate::container::{
    ebml_element_end, is_mp4_extension, mp4_child_boxes, mp4_find, read_ebml_element,
    read_ebml_uint, read_ebml_vint, skip_ebml_element,
};
use crate::error::ConvertError;
use crate::format::OutputFormat;
use crate::options::ConversionOptions;
use crate::shared::{file_extension, is_audio_extension, output_name, unsupported};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub(crate) const MAX_AUDIO_SAMPLES: usize = 120_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AudioChannels {
    Mono,
    Stereo,
}

/// Sample format for WAV and FLAC output.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum AudioBitDepth {
    #[default]
    Bit16,
    Bit24,
    Float32,
}

impl AudioBitDepth {
    pub const ALL: [Self; 3] = [Self::Bit16, Self::Bit24, Self::Float32];

    pub fn label(self) -> &'static str {
        match self {
            Self::Bit16 => "16-bit",
            Self::Bit24 => "24-bit",
            Self::Float32 => "32-bit float",
        }
    }

    /// Bits used to store one sample.
    pub fn bits(self) -> u16 {
        match self {
            Self::Bit16 => 16,
            Self::Bit24 => 24,
            Self::Float32 => 32,
        }
    }
}

/// Bitrate for MP3 output. MP3 is lossy, so sample bit depth does not apply.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum Mp3Bitrate {
    Kbps128,
    #[default]
    Kbps192,
    Kbps256,
    Kbps320,
}

impl Mp3Bitrate {
    pub const ALL: [Self; 4] = [Self::Kbps128, Self::Kbps192, Self::Kbps256, Self::Kbps320];

    pub fn kbps(self) -> u16 {
        match self {
            Self::Kbps128 => 128,
            Self::Kbps192 => 192,
            Self::Kbps256 => 256,
            Self::Kbps320 => 320,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Kbps128 => "128 kbps",
            Self::Kbps192 => "192 kbps",
            Self::Kbps256 => "256 kbps",
            Self::Kbps320 => "320 kbps",
        }
    }
}

/// One audio track that can be decoded from an input file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackInfo {
    /// Zero-based position in the selectable audio-track list.
    pub index: usize,
    /// Container track ID.
    pub id: u32,
    /// Human-readable codec name, such as `AAC` or `AC-3`.
    pub codec: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub channels: usize,
    pub sample_rate: u32,
    /// The first audio track, used when no track is selected.
    pub default: bool,
}

impl AudioTrackInfo {
    /// A compact label for pickers: `#2 · eng · AC-3 · 5.1 · 48 kHz`.
    /// The leading number is the selectable index; the container stream ID is
    /// included when it differs, so an unsupported stream cannot silently
    /// shift the displayed numbering.
    pub fn label(&self) -> String {
        let mut parts = vec![format!("#{}", self.index + 1)];
        if self.index + 1 != self.id as usize {
            parts.push(format!("stream {}", self.id));
        }
        if let Some(language) = self.language.as_deref().filter(|value| *value != "und") {
            parts.push(language.to_owned());
        }
        parts.push(self.codec.clone());
        parts.push(channel_label(self.channels).to_owned());
        if self.sample_rate > 0 {
            parts.push(format!("{:.1} kHz", f64::from(self.sample_rate) / 1000.0));
        }
        parts.join(" · ")
    }
}

pub(crate) fn channel_label(channels: usize) -> &'static str {
    match channels {
        0 => "unknown",
        1 => "mono",
        2 => "stereo",
        6 => "5.1",
        8 => "7.1",
        _ => "multichannel",
    }
}

impl AudioChannels {
    pub fn count(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mono => "Mono",
            Self::Stereo => "Stereo",
        }
    }
}

pub(crate) fn is_end_of_stream(error: &symphonia::core::errors::Error) -> bool {
    match error {
        symphonia::core::errors::Error::IoError(error)
            if error.kind() == std::io::ErrorKind::UnexpectedEof =>
        {
            true
        }
        _ => error
            .to_string()
            .to_ascii_lowercase()
            .contains("end of stream"),
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DecodedAudio {
    pub(crate) samples: Vec<f32>,
    pub(crate) channels: usize,
    pub(crate) sample_rate: u32,
    /// Precision of the source samples when known. Used to decide whether
    /// reducing to 16-bit output needs dither.
    pub(crate) source_bits: Option<u16>,
}

impl DecodedAudio {
    pub(crate) fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }
}

/// Frames to drop from the start, an optional exact length, and frames to
/// discard from the end. All values are in source frames after any decoder
/// delay has already been removed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AudioTrim {
    pub(crate) skip_frames: usize,
    pub(crate) limit_frames: Option<usize>,
    pub(crate) tail_padding: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AudioCodecKind {
    Symphonia,
    Opus,
    /// AC-3 and E-AC-3 frames come from filmcraft-ac3 because Symphonia 0.5
    /// has no AC-3 codec feature. They still use the same track selection,
    /// trim and downmix code as every other codec.
    Ac3,
}

pub(crate) struct AudioTrackEntry {
    pub(crate) info: AudioTrackInfo,
    pub(crate) kind: AudioCodecKind,
    /// Position of the track in the container's track list. MP4 metadata
    /// boxes are indexed by this value, not by the audio-only ordinal.
    pub(crate) track_index: usize,
    /// Opus decoder delay declared by the codec header. MP4 edit lists are
    /// expressed on the undecoded timeline, so this must be removed from their
    /// skip value when the decoder already dropped it.
    pub(crate) pre_skip: u32,
}

/// Lists the audio tracks in a file so callers can choose one before
/// converting. The first entry is the default track.
pub fn audio_tracks(input: &[u8], extension: &str) -> Result<Vec<AudioTrackInfo>, ConvertError> {
    Ok(audio_track_entries(input, extension)?
        .into_iter()
        .map(|entry| entry.info)
        .collect())
}

/// Lists tracks from a file without loading the whole file. Only container
/// headers and the first packets needed to identify the codec are read.
pub fn audio_tracks_from_path(path: &Path) -> Result<Vec<AudioTrackInfo>, ConvertError> {
    Ok(audio_track_entries_from_path(path)?
        .into_iter()
        .map(|entry| entry.info)
        .collect())
}

pub(crate) fn audio_track_entries_from_path(
    path: &Path,
) -> Result<Vec<AudioTrackEntry>, ConvertError> {
    let extension = file_extension(&path.to_string_lossy());
    let mut file = File::open(path)?;
    let mut head = [0_u8; 64 * 1024];
    let read = file.read(&mut head)?;
    let head = &head[..read];

    if extension == "flac" || head.starts_with(b"fLaC") {
        let mut file = File::open(path)?;
        skip_id3v2_file(&mut file)?;
        let reader = claxon::FlacReader::new(file).map_err(|error| {
            ConvertError::Audio(format!("could not open the FLAC stream: {error}"))
        })?;
        let info = reader.streaminfo();
        return Ok(vec![AudioTrackEntry {
            info: AudioTrackInfo {
                index: 0,
                id: 1,
                codec: "FLAC".to_owned(),
                language: None,
                title: None,
                channels: info.channels as usize,
                sample_rate: info.sample_rate,
                default: true,
            },
            kind: AudioCodecKind::Symphonia,
            track_index: 0,
            pre_skip: 0,
        }]);
    }
    if is_ogg_opus(head) {
        use opus_pure::OggOpusReader;
        let reader = OggOpusReader::new(File::open(path)?).map_err(|error| {
            ConvertError::Audio(format!("could not open the Opus stream: {error}"))
        })?;
        let head = reader.head();
        return Ok(vec![AudioTrackEntry {
            info: AudioTrackInfo {
                index: 0,
                id: 1,
                codec: "Opus".to_owned(),
                language: None,
                title: None,
                channels: head.channel_count as usize,
                sample_rate: 48_000,
                default: true,
            },
            kind: AudioCodecKind::Opus,
            track_index: 0,
            pre_skip: 0,
        }]);
    }
    if is_raw_ac3(head, &extension) {
        let header = filmcraft_ac3::parse_header(head).map_err(|error| {
            ConvertError::Audio(format!("could not read the AC-3 stream: {error}"))
        })?;
        return Ok(vec![AudioTrackEntry {
            info: AudioTrackInfo {
                index: 0,
                id: 1,
                codec: ac3_codec_label(&header).to_owned(),
                language: None,
                title: None,
                channels: header.channels(),
                sample_rate: header.sample_rate,
                default: true,
            },
            kind: AudioCodecKind::Ac3,
            track_index: 0,
            pre_skip: 0,
        }]);
    }

    let format = probe_symphonia_file(File::open(path)?, &extension)?;
    let moov = if is_mp4_extension(&extension) {
        read_mp4_moov(path)
    } else {
        None
    };
    audio_track_entries_from_format(format, moov.as_deref())
}

/// Skips an ID3v2 tag at the start of a file, leaving the reader positioned at
/// the first audio byte. Non-ID3 files are left at the start.
pub(crate) fn skip_id3v2_file(file: &mut File) -> Result<(), ConvertError> {
    let mut header = [0_u8; 10];
    if file.read_exact(&mut header).is_err() {
        file.seek(SeekFrom::Start(0))?;
        return Ok(());
    }
    if &header[..3] != b"ID3" {
        file.seek(SeekFrom::Start(0))?;
        return Ok(());
    }
    let size = ((header[6] as u64 & 0x7f) << 21)
        | ((header[7] as u64 & 0x7f) << 14)
        | ((header[8] as u64 & 0x7f) << 7)
        | (header[9] as u64 & 0x7f);
    let footer = if header[5] & 0x10 != 0 { 10 } else { 0 };
    file.seek(SeekFrom::Start(10 + size + footer))?;
    Ok(())
}

pub(crate) fn audio_track_entries(
    input: &[u8],
    extension: &str,
) -> Result<Vec<AudioTrackEntry>, ConvertError> {
    if extension == "flac" || input.starts_with(b"fLaC") {
        let input = strip_id3v2(input);
        let reader = claxon::FlacReader::new(Cursor::new(input)).map_err(|error| {
            ConvertError::Audio(format!("could not open the FLAC stream: {error}"))
        })?;
        let info = reader.streaminfo();
        return Ok(vec![AudioTrackEntry {
            info: AudioTrackInfo {
                index: 0,
                id: 1,
                codec: "FLAC".to_owned(),
                language: None,
                title: None,
                channels: info.channels as usize,
                sample_rate: info.sample_rate,
                default: true,
            },
            kind: AudioCodecKind::Symphonia,
            track_index: 0,
            pre_skip: 0,
        }]);
    }
    if is_ogg_opus(input) {
        use opus_pure::OggOpusReader;
        let reader = OggOpusReader::new(Cursor::new(input)).map_err(|error| {
            ConvertError::Audio(format!("could not open the Opus stream: {error}"))
        })?;
        let head = reader.head();
        return Ok(vec![AudioTrackEntry {
            info: AudioTrackInfo {
                index: 0,
                id: 1,
                codec: "Opus".to_owned(),
                language: None,
                title: None,
                channels: head.channel_count as usize,
                sample_rate: 48_000,
                default: true,
            },
            kind: AudioCodecKind::Opus,
            track_index: 0,
            pre_skip: 0,
        }]);
    }
    if is_raw_ac3(input, extension) {
        let header = filmcraft_ac3::parse_header(input).map_err(|error| {
            ConvertError::Audio(format!("could not read the AC-3 stream: {error}"))
        })?;
        return Ok(vec![AudioTrackEntry {
            info: AudioTrackInfo {
                index: 0,
                id: 1,
                codec: ac3_codec_label(&header).to_owned(),
                language: None,
                title: None,
                channels: header.channels(),
                sample_rate: header.sample_rate,
                default: true,
            },
            kind: AudioCodecKind::Ac3,
            track_index: 0,
            pre_skip: 0,
        }]);
    }
    symphonia_audio_track_entries(input, extension)
}

pub(crate) fn symphonia_audio_track_entries(
    input: &[u8],
    extension: &str,
) -> Result<Vec<AudioTrackEntry>, ConvertError> {
    let format = probe_symphonia(input, extension)?;
    let moov = if is_mp4_container(input) {
        mp4_find(input, b"moov")
    } else {
        None
    };
    audio_track_entries_from_format(format, moov)
}

pub(crate) fn audio_track_entries_from_format(
    mut format: Box<dyn symphonia::core::formats::FormatReader>,
    moov: Option<&[u8]>,
) -> Result<Vec<AudioTrackEntry>, ConvertError> {
    use symphonia::core::codecs::CODEC_TYPE_NULL;
    let tracks: Vec<(
        u32,
        symphonia::core::codecs::CodecParameters,
        Option<String>,
    )> = format
        .tracks()
        .iter()
        .map(|track| (track.id, track.codec_params.clone(), track.language.clone()))
        .collect();

    // Unknown codecs (AC-3/E-AC-3 in MKV and MP4 are reported as NULL) are
    // identified from their first packet. Scanning stops once every track has
    // been seen, so a long file is not read just to list its tracks.
    let mut first_packets: Vec<(u32, Vec<u8>)> = Vec::new();
    for _ in 0..1_024 {
        if first_packets.len() == tracks.len() {
            break;
        }
        match format.next_packet() {
            Ok(packet) => {
                if !first_packets.iter().any(|(id, _)| *id == packet.track_id()) {
                    first_packets.push((
                        packet.track_id(),
                        packet.data[..packet.data.len().min(16)].to_vec(),
                    ));
                }
            }
            Err(_) => break,
        }
    }

    let mut entries = Vec::new();
    for (track_index, (id, codec_params, mut language)) in tracks.into_iter().enumerate() {
        if language.is_none() {
            if let Some(moov) = moov {
                language = mp4_track_language(moov, track_index);
            }
        }
        let first_packet = first_packets
            .iter()
            .find(|(track, _)| *track == id)
            .map(|(_, data)| data.as_slice());
        let known_audio = is_known_audio_codec(codec_params.codec);
        let ac3_header = first_packet
            .filter(|data| data.starts_with(&[0x0B, 0x77]))
            .and_then(|data| filmcraft_ac3::parse_header(data).ok());

        let (kind, codec, channels, sample_rate, pre_skip) =
            if codec_params.codec == symphonia::core::codecs::CODEC_TYPE_OPUS {
                let (channels, pre_skip) = opus_stream_info(&codec_params)
                    .map(|info| (info.channels, info.pre_skip))
                    .unwrap_or((0, 0));
                (
                    AudioCodecKind::Opus,
                    "Opus".to_owned(),
                    channels,
                    codec_params.sample_rate.unwrap_or(48_000),
                    pre_skip,
                )
            } else if let Some(header) = ac3_header {
                (
                    AudioCodecKind::Ac3,
                    ac3_codec_label(&header).to_owned(),
                    header.channels(),
                    header.sample_rate,
                    0,
                )
            } else if known_audio {
                let mut channels = codec_params
                    .channels
                    .map(|channels| channels.count())
                    .unwrap_or(0);
                let mut sample_rate = codec_params.sample_rate.unwrap_or(0);
                if channels == 0 || sample_rate == 0 {
                    if let Some((mp4_channels, mp4_rate)) =
                        moov.and_then(|moov| mp4_track_audio_layout(moov, track_index))
                    {
                        if channels == 0 {
                            channels = mp4_channels;
                        }
                        if sample_rate == 0 {
                            sample_rate = mp4_rate;
                        }
                    }
                }
                (
                    AudioCodecKind::Symphonia,
                    symphonia_codec_label(codec_params.codec).to_owned(),
                    channels,
                    sample_rate,
                    0,
                )
            } else if codec_params.codec == CODEC_TYPE_NULL
                && (codec_params.sample_rate.is_some() || codec_params.channels.is_some())
            {
                // A container that declares audio metadata but uses a codec
                // Symphonia does not know still gets listed; decoding reports a
                // clear error if it cannot handle the packets.
                (
                    AudioCodecKind::Symphonia,
                    "Unknown".to_owned(),
                    codec_params
                        .channels
                        .map(|channels| channels.count())
                        .unwrap_or(0),
                    codec_params.sample_rate.unwrap_or(0),
                    0,
                )
            } else {
                continue;
            };

        entries.push(AudioTrackEntry {
            info: AudioTrackInfo {
                index: entries.len(),
                id,
                codec,
                language,
                title: None,
                channels,
                sample_rate,
                default: entries.is_empty(),
            },
            kind,
            track_index,
            pre_skip,
        });
    }

    if entries.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    Ok(entries)
}

/// Precision of the decoded integer source when it is meaningful. Lossy
/// codecs decode through an f32 pipeline and do not have a source integer
/// precision, so they are reported as unknown and get 16-bit dither.
pub(crate) fn reported_source_bits(
    codec: symphonia::core::codecs::CodecType,
    bits_per_sample: Option<u32>,
) -> Option<u16> {
    use symphonia::core::codecs::*;
    if matches!(
        codec,
        CODEC_TYPE_AAC
            | CODEC_TYPE_MP1
            | CODEC_TYPE_MP2
            | CODEC_TYPE_MP3
            | CODEC_TYPE_VORBIS
            | CODEC_TYPE_OPUS
    ) {
        return None;
    }
    bits_per_sample.and_then(|bits| u16::try_from(bits).ok())
}

pub(crate) fn is_known_audio_codec(codec: symphonia::core::codecs::CodecType) -> bool {
    use symphonia::core::codecs::*;
    matches!(
        codec,
        CODEC_TYPE_AAC
            | CODEC_TYPE_ALAC
            | CODEC_TYPE_FLAC
            | CODEC_TYPE_MP1
            | CODEC_TYPE_MP2
            | CODEC_TYPE_MP3
            | CODEC_TYPE_OPUS
            | CODEC_TYPE_VORBIS
            | CODEC_TYPE_PCM_S32LE
            | CODEC_TYPE_PCM_S32LE_PLANAR
            | CODEC_TYPE_PCM_S32BE
            | CODEC_TYPE_PCM_S32BE_PLANAR
            | CODEC_TYPE_PCM_S24LE
            | CODEC_TYPE_PCM_S24LE_PLANAR
            | CODEC_TYPE_PCM_S24BE
            | CODEC_TYPE_PCM_S24BE_PLANAR
            | CODEC_TYPE_PCM_S16LE
            | CODEC_TYPE_PCM_S16LE_PLANAR
            | CODEC_TYPE_PCM_S16BE
            | CODEC_TYPE_PCM_S16BE_PLANAR
            | CODEC_TYPE_PCM_S8
            | CODEC_TYPE_PCM_S8_PLANAR
            | CODEC_TYPE_PCM_U32LE
            | CODEC_TYPE_PCM_U32LE_PLANAR
            | CODEC_TYPE_PCM_U32BE
            | CODEC_TYPE_PCM_U32BE_PLANAR
            | CODEC_TYPE_PCM_U24LE
            | CODEC_TYPE_PCM_U24LE_PLANAR
            | CODEC_TYPE_PCM_U24BE
            | CODEC_TYPE_PCM_U24BE_PLANAR
            | CODEC_TYPE_PCM_U16LE
            | CODEC_TYPE_PCM_U16LE_PLANAR
            | CODEC_TYPE_PCM_U16BE
            | CODEC_TYPE_PCM_U16BE_PLANAR
            | CODEC_TYPE_PCM_U8
            | CODEC_TYPE_PCM_U8_PLANAR
            | CODEC_TYPE_PCM_F32LE
            | CODEC_TYPE_PCM_F32LE_PLANAR
            | CODEC_TYPE_PCM_F32BE
            | CODEC_TYPE_PCM_F32BE_PLANAR
            | CODEC_TYPE_PCM_F64LE
            | CODEC_TYPE_PCM_F64LE_PLANAR
            | CODEC_TYPE_PCM_F64BE
            | CODEC_TYPE_PCM_F64BE_PLANAR
            | CODEC_TYPE_PCM_ALAW
            | CODEC_TYPE_PCM_MULAW
            | CODEC_TYPE_ADPCM_G722
            | CODEC_TYPE_ADPCM_G726LE
            | CODEC_TYPE_ADPCM_MS
            | CODEC_TYPE_ADPCM_IMA_WAV
    )
}

pub(crate) fn symphonia_codec_label(codec: symphonia::core::codecs::CodecType) -> &'static str {
    use symphonia::core::codecs::*;
    match codec {
        CODEC_TYPE_AAC => "AAC",
        CODEC_TYPE_ALAC => "ALAC",
        CODEC_TYPE_FLAC => "FLAC",
        CODEC_TYPE_MP1 => "MP1",
        CODEC_TYPE_MP2 => "MP2",
        CODEC_TYPE_MP3 => "MP3",
        CODEC_TYPE_OPUS => "Opus",
        CODEC_TYPE_VORBIS => "Vorbis",
        CODEC_TYPE_PCM_ALAW => "A-law",
        CODEC_TYPE_PCM_MULAW => "µ-law",
        _ => "Audio",
    }
}

pub(crate) fn ac3_codec_label(header: &filmcraft_ac3::Header) -> &'static str {
    if header.is_eac3() {
        "E-AC-3"
    } else {
        "AC-3"
    }
}

pub(crate) fn is_raw_ac3(input: &[u8], extension: &str) -> bool {
    matches!(extension, "ac3" | "eac3" | "ec3") && input.starts_with(&[0x0B, 0x77])
}

pub(crate) fn probe_symphonia(
    input: &[u8],
    extension: &str,
) -> Result<Box<dyn symphonia::core::formats::FormatReader>, ConvertError> {
    probe_symphonia_source(Box::new(Cursor::new(input.to_vec())), extension)
}

pub(crate) fn probe_symphonia_file(
    mut file: File,
    extension: &str,
) -> Result<Box<dyn symphonia::core::formats::FormatReader>, ConvertError> {
    skip_id3v2_file(&mut file)?;
    probe_symphonia_source(Box::new(file), extension)
}

pub(crate) fn probe_symphonia_source(
    source: Box<dyn symphonia::core::io::MediaSource>,
    extension: &str,
) -> Result<Box<dyn symphonia::core::formats::FormatReader>, ConvertError> {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let source = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    hint.with_extension(extension);
    symphonia::default::get_probe()
        .format(
            &hint,
            source,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map(|probed| probed.format)
        .map_err(|error| ConvertError::Audio(format!("could not open the audio stream: {error}")))
}

pub(crate) fn decode_audio(
    input: &[u8],
    extension: &str,
    track: Option<usize>,
) -> Result<DecodedAudio, ConvertError> {
    let entries = audio_track_entries(input, extension)?;
    let index = track.unwrap_or(0);
    let entry = entries.get(index).ok_or(ConvertError::AudioTrackNotFound {
        requested: index + 1,
        available: entries.len(),
    })?;
    let track_id = entry.info.id;
    let track_index = entry.track_index;
    let track_channels = entry.info.channels;
    let kind = entry.kind;

    if extension == "flac" || input.starts_with(b"fLaC") {
        return decode_flac(input);
    }
    if is_ogg_opus(input) {
        return decode_ogg_opus(input);
    }
    if is_raw_ac3(input, extension) {
        return decode_ac3_elementary(input);
    }

    let mut format = probe_symphonia(input, extension)?;
    let codec_params = format
        .tracks()
        .iter()
        .find(|candidate| candidate.id == track_id)
        .map(|candidate| candidate.codec_params.clone())
        .ok_or(ConvertError::NoAudioTrack)?;
    let pre_skip = if kind == AudioCodecKind::Opus {
        opus_stream_info(&codec_params)
            .map(|info| info.pre_skip)
            .unwrap_or(0)
    } else {
        0
    };
    let mp4_gapless = if is_mp4_extension(extension) {
        mp4_find(input, b"moov").and_then(|moov| mp4_gapless_info(moov, track_index))
    } else {
        None
    };

    let mut audio = if kind == AudioCodecKind::Opus {
        decode_opus_track(format.as_mut(), track_id, &codec_params)?
    } else if kind == AudioCodecKind::Ac3 {
        decode_ac3_track(format.as_mut(), track_id)?
    } else {
        decode_symphonia_track(format.as_mut(), track_id, &codec_params, track_channels)?
    };

    if let Some(info) = mp4_gapless {
        apply_audio_trim(&mut audio, mp4_trim_for_kind(info, kind, pre_skip));
    }
    if matches!(extension, "mkv" | "webm") && kind == AudioCodecKind::Opus {
        let timing = matroska_audio_timing(input, track_id);
        apply_audio_trim(&mut audio, timing.to_trim());
    }
    if audio.samples.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    Ok(audio)
}

pub(crate) fn decode_symphonia_track(
    format: &mut dyn symphonia::core::formats::FormatReader,
    track_id: u32,
    codec_params: &symphonia::core::codecs::CodecParameters,
    track_channels: usize,
) -> Result<DecodedAudio, ConvertError> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error as SymphoniaError;

    let mut decoder = symphonia::default::get_codecs()
        .make(codec_params, &DecoderOptions::default())
        .map_err(|error| {
            if codec_params.codec == symphonia::core::codecs::CODEC_TYPE_AAC
                && track_channels > 2
            {
                ConvertError::Audio(
                    "multichannel AAC cannot be decoded; choose a stereo AAC track or an AC-3/E-AC-3 track"
                        .to_owned(),
                )
            } else {
                ConvertError::Audio(format!("could not start the audio decoder: {error}"))
            }
        })?;

    let mut sample_buffer: Option<SampleBuffer<f32>> = None;
    let mut samples = Vec::new();
    let mut sample_rate = codec_params.sample_rate.unwrap_or(0);
    let mut channels = codec_params
        .channels
        .map(|channels| channels.count())
        .unwrap_or(0);

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(error) if is_end_of_stream(&error) => break,
            Err(SymphoniaError::ResetRequired) => break,
            Err(error) => {
                return Err(ConvertError::Audio(format!(
                    "could not read audio packets: {error}"
                )))
            }
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(decoded) => decoded,
            Err(error) if is_end_of_stream(&error) => break,
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(error) => {
                return Err(ConvertError::Audio(format!(
                    "could not decode audio samples: {error}"
                )))
            }
        };

        let spec = *decoded.spec();
        sample_rate = spec.rate;
        channels = spec.channels.count();
        let capacity = decoded.capacity();
        if sample_buffer
            .as_ref()
            .is_none_or(|buffer| buffer.capacity() < capacity)
        {
            sample_buffer = Some(SampleBuffer::<f32>::new(capacity as u64, spec));
        }
        let buffer = sample_buffer
            .as_mut()
            .expect("the sample buffer was just initialized");
        buffer.copy_interleaved_ref(decoded);
        samples.extend_from_slice(buffer.samples());
        if samples.len() > MAX_AUDIO_SAMPLES {
            return Err(ConvertError::AudioTooLarge);
        }
    }

    if samples.is_empty() || channels == 0 || sample_rate == 0 {
        return Err(ConvertError::NoAudioTrack);
    }

    let source_bits = reported_source_bits(codec_params.codec, codec_params.bits_per_sample);
    Ok(DecodedAudio {
        samples,
        channels,
        sample_rate,
        source_bits,
    })
}

pub(crate) fn mp4_trim_for_kind(
    info: Mp4Gapless,
    kind: AudioCodecKind,
    pre_skip: u32,
) -> AudioTrim {
    let pre_skip = pre_skip as usize;
    let skip_frames = if kind == AudioCodecKind::Opus {
        info.skip.saturating_sub(pre_skip)
    } else {
        info.skip
    };
    let limit_frames = if kind == AudioCodecKind::Opus && !info.limit_is_presentation {
        info.limit.map(|limit| limit.saturating_sub(pre_skip))
    } else {
        info.limit
    };
    AudioTrim {
        skip_frames,
        limit_frames,
        tail_padding: 0,
    }
}

pub(crate) fn apply_audio_trim(audio: &mut DecodedAudio, trim: AudioTrim) {
    let channels = audio.channels.max(1);
    let frames = audio.samples.len() / channels;
    let skip = trim.skip_frames.min(frames);
    if skip > 0 {
        audio.samples.drain(..skip * channels);
    }
    if let Some(limit) = trim.limit_frames {
        let available = audio.samples.len() / channels;
        if limit < available {
            audio.samples.truncate(limit * channels);
        }
    }
    let padding = trim.tail_padding.min(audio.samples.len() / channels);
    if padding > 0 {
        audio
            .samples
            .truncate(audio.samples.len() - padding * channels);
    }
}

pub(crate) fn is_mp4_container(input: &[u8]) -> bool {
    input.get(4..8).is_some_and(|kind| kind == b"ftyp")
}

/// Reads just the `moov` box from an MP4 file, seeking over the media data.
/// Metadata such as edit lists and track languages lives in this box.
pub(crate) fn read_mp4_moov(path: &Path) -> Option<Vec<u8>> {
    const MAX_MOOV_BYTES: u64 = 64 * 1024 * 1024;
    let mut file = File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let mut position = 0_u64;
    while position.checked_add(8)? <= length {
        file.seek(SeekFrom::Start(position)).ok()?;
        let mut header = [0_u8; 8];
        file.read_exact(&mut header).ok()?;
        let size32 = u32::from_be_bytes(header[..4].try_into().ok()?) as u64;
        let (header_size, size) = if size32 == 1 {
            let mut large = [0_u8; 8];
            file.read_exact(&mut large).ok()?;
            (16, u64::from_be_bytes(large))
        } else if size32 == 0 {
            (8, length - position)
        } else {
            (8, size32)
        };
        if size < header_size || position.checked_add(size)? > length {
            return None;
        }
        if &header[4..8] == b"moov" {
            if size > MAX_MOOV_BYTES {
                return None;
            }
            let mut moov = vec![0_u8; size as usize];
            file.seek(SeekFrom::Start(position)).ok()?;
            file.read_exact(&mut moov).ok()?;
            return Some(moov[header_size as usize..].to_vec());
        }
        position += size;
    }
    None
}

pub(crate) fn is_ogg_opus(input: &[u8]) -> bool {
    input.starts_with(b"OggS") && input.get(28..36).is_some_and(|magic| magic == b"OpusHead")
}

pub(crate) struct OpusStreamInfo {
    pub(crate) channels: usize,
    pub(crate) pre_skip: u32,
    pub(crate) mapping_family: u8,
}

/// Opus mapping family 0 only defines mono and stereo. Family 1 defines the
/// Vorbis surround layouts up to 7.1, which the multistream decoder supports.
pub(crate) fn opus_channels(channels: usize, mapping_family: u8) -> Result<usize, ConvertError> {
    let supported = match mapping_family {
        0 => (1..=2).contains(&channels),
        1 => (1..=8).contains(&channels),
        _ => false,
    };
    if !supported {
        return Err(ConvertError::UnsupportedChannelLayout {
            codec: "Opus",
            channels,
        });
    }
    Ok(channels)
}

/// Decodes either an ordinary mono/stereo Opus stream or a multistream
/// surround packet. Keeping both variants here means the file and streaming
/// paths share the same channel handling.
pub(crate) enum OpusPcmDecoder {
    Single(Box<opus_pure::OpusDecoder>),
    Multi(Box<opus_pure::OpusMSDecoder>),
}

impl OpusPcmDecoder {
    pub(crate) fn new(channels: usize, mapping_family: u8) -> Result<Self, ConvertError> {
        if mapping_family == 0 {
            opus_pure::OpusDecoder::new(48_000, channels)
                .map(|decoder| Self::Single(Box::new(decoder)))
                .map_err(|error| {
                    ConvertError::Audio(format!("could not start the Opus decoder: {error}"))
                })
        } else {
            opus_pure::OpusMSDecoder::new(48_000, channels, mapping_family)
                .map(|decoder| Self::Multi(Box::new(decoder)))
                .map_err(|error| {
                    ConvertError::Audio(format!("could not start the Opus decoder: {error}"))
                })
        }
    }

    pub(crate) fn decode(
        &mut self,
        packet: &[u8],
        frame_size: usize,
        block: &mut [f32],
    ) -> Result<usize, opus_pure::Error> {
        match self {
            Self::Single(decoder) => decoder.decode(packet, frame_size, block),
            Self::Multi(decoder) => decoder.decode(packet, frame_size, block),
        }
    }
}

/// Opus mapping family 1 uses the Vorbis channel order, which differs from the
/// WAV/film order the downmixer expects. Reorder to L, R, C, LFE, surrounds.
pub(crate) fn reorder_opus_channels(samples: &mut [f32], channels: usize) {
    let order: &[usize] = match channels {
        3 => &[0, 2, 1],
        5 => &[0, 2, 1, 3, 4],
        6 => &[0, 2, 1, 5, 3, 4],
        7 => &[0, 2, 1, 6, 5, 3, 4],
        8 => &[0, 2, 1, 7, 5, 6, 3, 4],
        _ => return,
    };
    let mut frame = [0.0_f32; 8];
    for chunk in samples.chunks_exact_mut(channels) {
        for (output, &source) in order.iter().enumerate() {
            frame[output] = chunk[source];
        }
        chunk.copy_from_slice(&frame[..channels]);
    }
}

/// The inverse of [`reorder_opus_channels`], used before multistream encoding.
pub(crate) fn reorder_film_to_opus_channels(samples: &mut [f32], channels: usize) {
    let order: &[usize] = match channels {
        3 => &[0, 2, 1],
        5 => &[0, 2, 1, 3, 4],
        6 => &[0, 2, 1, 4, 5, 3],
        7 => &[0, 2, 1, 5, 6, 4, 3],
        8 => &[0, 2, 1, 6, 7, 4, 5, 3],
        _ => return,
    };
    let mut frame = [0.0_f32; 8];
    for chunk in samples.chunks_exact_mut(channels) {
        for (output, &source) in order.iter().enumerate() {
            frame[output] = chunk[source];
        }
        chunk.copy_from_slice(&frame[..channels]);
    }
}

pub(crate) fn append_audio_samples(
    samples: &mut Vec<f32>,
    decoded: &[f32],
) -> Result<(), ConvertError> {
    samples.extend_from_slice(decoded);
    if samples.len() > MAX_AUDIO_SAMPLES {
        return Err(ConvertError::AudioTooLarge);
    }
    Ok(())
}

pub(crate) fn decode_ogg_opus(input: &[u8]) -> Result<DecodedAudio, ConvertError> {
    use opus_pure::{OggOpusReader, Trim, MAX_PACKET_SAMPLES};

    let mut reader = OggOpusReader::new(Cursor::new(input))
        .map_err(|error| ConvertError::Audio(format!("could not open the Opus stream: {error}")))?;
    let head = reader.head().clone();
    let channels = opus_channels(head.channel_count as usize, head.mapping_family)?;
    let mut decoder = OpusPcmDecoder::new(channels, head.mapping_family)?;
    let mut trim = Trim::new(&head, 48_000, channels)
        .map_err(|error| ConvertError::Audio(format!("could not read the Opus stream: {error}")))?;
    let mut block = vec![0.0f32; MAX_PACKET_SAMPLES * channels];
    let mut samples = Vec::new();

    for packet in reader.packets() {
        let packet = packet.map_err(|error| {
            ConvertError::Audio(format!("could not read Opus packets: {error}"))
        })?;
        let decoded = decoder
            .decode(&packet.data, MAX_PACKET_SAMPLES, &mut block)
            .map_err(|error| {
                ConvertError::Audio(format!("could not decode Opus audio: {error}"))
            })?;
        reorder_opus_channels(&mut block[..decoded * channels], channels);
        append_audio_samples(
            &mut samples,
            trim.keep(&packet, &block[..decoded * channels]),
        )?;
    }

    if samples.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    Ok(DecodedAudio {
        samples,
        channels,
        sample_rate: 48_000,
        source_bits: None,
    })
}

pub(crate) fn decode_opus_track(
    format: &mut dyn symphonia::core::formats::FormatReader,
    track_id: u32,
    codec_params: &symphonia::core::codecs::CodecParameters,
) -> Result<DecodedAudio, ConvertError> {
    use opus_pure::MAX_PACKET_SAMPLES;

    let info = opus_stream_info(codec_params)?;
    let channels = info.channels;
    let mut decoder = OpusPcmDecoder::new(channels, info.mapping_family)?;
    let mut block = vec![0.0f32; MAX_PACKET_SAMPLES * channels];
    let mut samples = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(error) if is_end_of_stream(&error) => break,
            Err(symphonia::core::errors::Error::ResetRequired) => break,
            Err(error) => {
                return Err(ConvertError::Audio(format!(
                    "could not read audio packets: {error}"
                )))
            }
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = decoder
            .decode(&packet.data, MAX_PACKET_SAMPLES, &mut block)
            .map_err(|error| {
                ConvertError::Audio(format!("could not decode Opus audio: {error}"))
            })?;
        reorder_opus_channels(&mut block[..decoded * channels], channels);
        append_audio_samples(&mut samples, &block[..decoded * channels])?;
    }

    let skip = (info.pre_skip as usize * channels).min(samples.len());
    samples.drain(..skip);

    if samples.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    Ok(DecodedAudio {
        samples,
        channels,
        sample_rate: 48_000,
        source_bits: None,
    })
}

/// Decodes an AC-3 or E-AC-3 elementary stream by walking its syncframes.
pub(crate) fn decode_ac3_elementary(input: &[u8]) -> Result<DecodedAudio, ConvertError> {
    let mut decoder = filmcraft_ac3::Decoder::new();
    let mut audio = Ac3Accumulator::default();
    let mut offset = 0;
    while offset + 7 <= input.len() {
        if input[offset..].starts_with(&[0x0B, 0x77]) {
            let header = filmcraft_ac3::parse_header(&input[offset..]).map_err(|error| {
                ConvertError::Audio(format!("could not read the AC-3 stream: {error}"))
            })?;
            if offset + header.frame_bytes > input.len() {
                break;
            }
            audio.push_frame(&mut decoder, &input[offset..offset + header.frame_bytes])?;
            offset += header.frame_bytes;
        } else {
            offset += 1;
        }
    }
    audio.finish()
}

pub(crate) fn decode_ac3_track(
    format: &mut dyn symphonia::core::formats::FormatReader,
    track_id: u32,
) -> Result<DecodedAudio, ConvertError> {
    let mut decoder = filmcraft_ac3::Decoder::new();
    let mut audio = Ac3Accumulator::default();

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(error) if is_end_of_stream(&error) => break,
            Err(symphonia::core::errors::Error::ResetRequired) => break,
            Err(error) => {
                return Err(ConvertError::Audio(format!(
                    "could not read audio packets: {error}"
                )))
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        let mut data: &[u8] = &packet.data;
        while data.len() >= 7 {
            if !data.starts_with(&[0x0B, 0x77]) {
                data = &data[1..];
                continue;
            }
            let header = filmcraft_ac3::parse_header(data).map_err(|error| {
                ConvertError::Audio(format!("could not read the AC-3 stream: {error}"))
            })?;
            if data.len() < header.frame_bytes {
                break;
            }
            audio.push_frame(&mut decoder, &data[..header.frame_bytes])?;
            data = &data[header.frame_bytes..];
        }
    }
    audio.finish()
}

#[derive(Default)]
pub(crate) struct Ac3Accumulator {
    pub(crate) samples: Vec<f32>,
    pub(crate) channels: usize,
    pub(crate) sample_rate: u32,
}

impl Ac3Accumulator {
    fn push_frame(
        &mut self,
        decoder: &mut filmcraft_ac3::Decoder,
        frame: &[u8],
    ) -> Result<(), ConvertError> {
        let decoded = decoder.decode(frame).map_err(|error| {
            ConvertError::Audio(format!("could not decode AC-3 audio: {error}"))
        })?;
        if decoded.channels.is_empty() {
            return Ok(());
        }
        let channels = decoded.channels.len();
        if self.channels != 0 && self.channels != channels {
            return Err(ConvertError::Audio(
                "the AC-3 track changes its channel layout".to_owned(),
            ));
        }
        self.channels = channels;
        self.sample_rate = decoded.header.sample_rate;
        let frames = decoded.channels[0].len();
        self.samples.reserve(frames * channels);
        for index in 0..frames {
            for channel in &decoded.channels {
                self.samples.push(channel[index]);
            }
        }
        if self.samples.len() > MAX_AUDIO_SAMPLES {
            return Err(ConvertError::AudioTooLarge);
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<DecodedAudio, ConvertError> {
        if self.samples.is_empty() || self.channels == 0 || self.sample_rate == 0 {
            return Err(ConvertError::NoAudioTrack);
        }
        Ok(DecodedAudio {
            samples: self.samples,
            channels: self.channels,
            sample_rate: self.sample_rate,
            source_bits: None,
        })
    }
}

pub(crate) const EBML_SEGMENT_ID: u32 = 0x1853_8067;

pub(crate) const EBML_CLUSTER_ID: u32 = 0x1F43_B675;

pub(crate) const EBML_BLOCK_GROUP_ID: u32 = 0xA0;

pub(crate) const EBML_BLOCK_ID: u32 = 0xA1;

pub(crate) const EBML_DISCARD_PADDING_ID: u32 = 0x75A2;

pub(crate) fn read_ebml_signed(input: &[u8], pos: usize, size: u64) -> Option<i64> {
    if size > 8 {
        return None;
    }
    let bytes = input.get(pos..pos + size as usize)?;
    let mut value = 0_i64;
    for byte in bytes {
        value = value.wrapping_shl(8) | i64::from(*byte);
    }
    if size < 8 && bytes.first().is_some_and(|byte| *byte & 0x80 != 0) {
        value -= 1_i64 << (size * 8);
    }
    Some(value)
}

pub(crate) const EBML_INFO_ID: u32 = 0x1549_A966;

pub(crate) const EBML_TIMECODE_SCALE_ID: u32 = 0x002A_D7B1;

pub(crate) const EBML_TIMECODE_ID: u32 = 0xE7;

pub(crate) const EBML_SIMPLE_BLOCK_ID: u32 = 0xA3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MatroskaAudioTiming {
    pub(crate) discard_padding_ns: Option<u64>,
    pub(crate) last_block_timestamp_ns: Option<u64>,
}

impl MatroskaAudioTiming {
    pub(crate) fn to_trim(self) -> AudioTrim {
        let mut trim = AudioTrim::default();
        if let Some(padding_ns) = self.discard_padding_ns {
            trim.tail_padding = ns_to_frames(padding_ns, 48_000);
        } else if let Some(timestamp_ns) = self.last_block_timestamp_ns.filter(|value| *value > 0) {
            // Without DiscardPadding, the last block's timestamp is the only
            // per-track presentation bound left in a remux. Do not include the
            // final packet's decoder padding in the output.
            trim.limit_frames = Some(ns_to_frames(timestamp_ns, 48_000));
        }
        trim
    }
}

pub(crate) fn ns_to_frames(ns: u64, sample_rate: u32) -> usize {
    ((u128::from(ns) * u128::from(sample_rate) + 500_000_000) / 1_000_000_000) as usize
}

pub(crate) fn matroska_block_track_and_time(
    input: &[u8],
    start: usize,
    size: u64,
) -> Option<(u64, i64)> {
    let end = start.checked_add(size as usize)?;
    if end > input.len() {
        return None;
    }
    let mut cursor = start;
    let (track, _) = read_ebml_vint(input, &mut cursor, 8, false)?;
    let relative = i16::from_be_bytes(input.get(cursor..cursor + 2)?.try_into().ok()?);
    Some((track, i64::from(relative)))
}

pub(crate) fn matroska_timecode_scale(input: &[u8]) -> Option<u64> {
    if !input.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return None;
    }
    let mut pos = 0;
    let (segment_size, segment_start) = loop {
        let (id, size) = read_ebml_element(input, &mut pos)?;
        if id == EBML_SEGMENT_ID {
            break (size, pos);
        }
        skip_ebml_element(input, &mut pos, size)?;
    };
    let segment_end = ebml_element_end(segment_start, segment_size, input.len());
    let mut scan = segment_start;
    while scan < segment_end {
        let (id, size) = read_ebml_element(input, &mut scan)?;
        let end = ebml_element_end(scan, size, segment_end);
        if id == EBML_INFO_ID {
            let mut info_pos = scan;
            while info_pos < end {
                let (child_id, child_size) = read_ebml_element(input, &mut info_pos)?;
                let child_end = ebml_element_end(info_pos, child_size, end);
                if child_id == EBML_TIMECODE_SCALE_ID {
                    if let Some(value) = child_size
                        .and_then(|size| read_ebml_uint(input, info_pos, size))
                        .filter(|value| *value > 0)
                    {
                        return Some(value);
                    }
                }
                info_pos = child_end;
            }
        }
        scan = end;
    }
    None
}

pub(crate) fn matroska_audio_timing_from_path(path: &Path, track_id: u32) -> MatroskaAudioTiming {
    const HEAD_BYTES: u64 = 4 * 1024 * 1024;
    const TAIL_BYTES: u64 = 8 * 1024 * 1024;
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return MatroskaAudioTiming::default(),
    };
    let length = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(_) => return MatroskaAudioTiming::default(),
    };
    let head_length = length.min(HEAD_BYTES);
    let mut head = vec![0_u8; head_length as usize];
    if file.read_exact(&mut head).is_err() {
        return MatroskaAudioTiming::default();
    }
    let timecode_scale_ns = matroska_timecode_scale(&head).unwrap_or(1_000_000);

    let tail_length = length.min(TAIL_BYTES);
    if file.seek(SeekFrom::Start(length - tail_length)).is_err() {
        return MatroskaAudioTiming::default();
    }
    let mut tail = vec![0_u8; tail_length as usize];
    if file.read_exact(&mut tail).is_err() {
        return MatroskaAudioTiming::default();
    }

    // The final cluster starts within the bounded tail for normal Matroska
    // muxes. Search backwards so the first valid candidate is the latest one;
    // the ID can also occur in compressed block data, so candidates are only
    // accepted when parsing yields timing for the selected track.
    let cluster_id = EBML_CLUSTER_ID.to_be_bytes();
    if tail.len() >= cluster_id.len() {
        for position in (0..=tail.len() - cluster_id.len()).rev() {
            if tail[position..position + cluster_id.len()] != cluster_id {
                continue;
            }
            let mut cursor = position;
            let Some((id, size)) = read_ebml_element(&tail, &mut cursor) else {
                continue;
            };
            if id != EBML_CLUSTER_ID {
                continue;
            }
            let cluster_end = ebml_element_end(cursor, size, tail.len());
            let mut timing = MatroskaAudioTiming::default();
            if parse_matroska_cluster(
                &tail,
                cursor,
                cluster_end,
                timecode_scale_ns,
                track_id,
                &mut timing,
            ) && (timing.discard_padding_ns.is_some()
                || timing.last_block_timestamp_ns.is_some())
            {
                return timing;
            }
        }
    }
    MatroskaAudioTiming::default()
}

/// Reads per-track Opus timing from Matroska. `DiscardPadding` is the precise
/// end trim when a muxer kept it. A remux often drops it, leaving only the last
/// block timestamp; that timestamp still excludes the final packet's padding.
pub(crate) fn matroska_audio_timing(input: &[u8], track_id: u32) -> MatroskaAudioTiming {
    let mut timing = MatroskaAudioTiming::default();
    if !input.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return timing;
    }

    let mut pos = 0;
    let mut visited = 0usize;
    let (segment_size, segment_start) = loop {
        visited += 1;
        if visited > 1_000_000 {
            return timing;
        }
        let Some((id, size)) = read_ebml_element(input, &mut pos) else {
            return timing;
        };
        if id == EBML_SEGMENT_ID {
            break (size, pos);
        }
        if skip_ebml_element(input, &mut pos, size).is_none() {
            return timing;
        }
    };
    let segment_end = ebml_element_end(segment_start, segment_size, input.len());

    let mut timecode_scale_ns = 1_000_000_u64;
    let mut scan = segment_start;
    while scan < segment_end {
        let Some((id, size)) = read_ebml_element(input, &mut scan) else {
            break;
        };
        let end = ebml_element_end(scan, size, segment_end);
        if id == EBML_INFO_ID {
            let mut info_pos = scan;
            while info_pos < end {
                let Some((child_id, child_size)) = read_ebml_element(input, &mut info_pos) else {
                    break;
                };
                let child_end = child_size
                    .and_then(|size| (info_pos as u64).checked_add(size))
                    .map(|value| value.min(end as u64) as usize)
                    .unwrap_or(end);
                if child_id == EBML_TIMECODE_SCALE_ID {
                    if let Some(value) =
                        child_size.and_then(|size| read_ebml_uint(input, info_pos, size))
                    {
                        if value > 0 {
                            timecode_scale_ns = value;
                        }
                    }
                }
                info_pos = child_end;
            }
        }
        scan = end;
    }

    scan = segment_start;
    while scan < segment_end {
        visited += 1;
        if visited > 1_000_000 {
            return timing;
        }
        let Some((id, size)) = read_ebml_element(input, &mut scan) else {
            break;
        };
        let cluster_end = ebml_element_end(scan, size, segment_end);
        if id != EBML_CLUSTER_ID {
            scan = cluster_end;
            continue;
        }

        if !parse_matroska_cluster(
            input,
            scan,
            cluster_end,
            timecode_scale_ns,
            track_id,
            &mut timing,
        ) {
            return timing;
        }
        scan = cluster_end;
    }
    timing
}

pub(crate) fn parse_matroska_cluster(
    input: &[u8],
    cluster_start: usize,
    cluster_end: usize,
    timecode_scale_ns: u64,
    track_id: u32,
    timing: &mut MatroskaAudioTiming,
) -> bool {
    let mut cluster_pos = cluster_start;
    let mut cluster_time_ns = 0_u64;
    let mut visited = 0usize;
    while cluster_pos < cluster_end {
        visited += 1;
        if visited > 1_000_000 {
            return false;
        }
        let Some((child_id, child_size)) = read_ebml_element(input, &mut cluster_pos) else {
            break;
        };
        let child_end = ebml_element_end(cluster_pos, child_size, cluster_end);
        match child_id {
            EBML_TIMECODE_ID => {
                if let Some(value) =
                    child_size.and_then(|size| read_ebml_uint(input, cluster_pos, size))
                {
                    cluster_time_ns = value.saturating_mul(timecode_scale_ns);
                }
            }
            EBML_SIMPLE_BLOCK_ID => {
                if let Some((track, relative)) = child_size
                    .and_then(|size| matroska_block_track_and_time(input, cluster_pos, size))
                {
                    if track == u64::from(track_id) {
                        timing.last_block_timestamp_ns = Some(block_timestamp_ns(
                            cluster_time_ns,
                            relative,
                            timecode_scale_ns,
                        ));
                        timing.discard_padding_ns = None;
                    }
                }
            }
            EBML_BLOCK_GROUP_ID => {
                let mut group_pos = cluster_pos;
                let mut group_track = None;
                let mut group_relative = None;
                let mut group_padding = None;
                while group_pos < child_end {
                    let Some((group_child_id, group_child_size)) =
                        read_ebml_element(input, &mut group_pos)
                    else {
                        break;
                    };
                    let group_child_end = ebml_element_end(group_pos, group_child_size, child_end);
                    match group_child_id {
                        EBML_BLOCK_ID => {
                            if let Some((track, relative)) = group_child_size.and_then(|size| {
                                matroska_block_track_and_time(input, group_pos, size)
                            }) {
                                group_track = Some(track);
                                group_relative = Some(relative);
                            }
                        }
                        EBML_DISCARD_PADDING_ID => {
                            if let Some(value) = group_child_size
                                .and_then(|size| read_ebml_signed(input, group_pos, size))
                                .filter(|value| *value > 0)
                            {
                                group_padding = Some(value as u64);
                            }
                        }
                        _ => {}
                    }
                    group_pos = group_child_end;
                }
                if group_track == Some(u64::from(track_id)) {
                    if let Some(relative) = group_relative {
                        timing.last_block_timestamp_ns = Some(block_timestamp_ns(
                            cluster_time_ns,
                            relative,
                            timecode_scale_ns,
                        ));
                    }
                    timing.discard_padding_ns = group_padding;
                }
            }
            _ => {}
        }
        cluster_pos = child_end;
    }
    true
}

pub(crate) fn block_timestamp_ns(
    cluster_time_ns: u64,
    relative: i64,
    timecode_scale_ns: u64,
) -> u64 {
    let timestamp =
        i128::from(cluster_time_ns) + i128::from(relative) * i128::from(timecode_scale_ns);
    timestamp.max(0) as u64
}

#[cfg(test)]
pub(crate) fn matroska_discard_padding(input: &[u8], track_id: u32) -> Option<u64> {
    matroska_audio_timing(input, track_id).discard_padding_ns
}

/// Reads the ISO 639-2 language code from an MP4 track's media header.
pub(crate) fn mp4_track_language(moov: &[u8], track_index: usize) -> Option<String> {
    let traks: Vec<&[u8]> = mp4_child_boxes(moov)
        .into_iter()
        .filter(|(kind, _)| kind == b"trak")
        .map(|(_, body)| body)
        .collect();
    let mdhd = mp4_find(mp4_find(traks.get(track_index)?, b"mdia")?, b"mdhd")?;
    let offset = match mdhd.first()? {
        0 => 20,
        1 => 32,
        _ => return None,
    };
    let packed = u16::from_be_bytes(mdhd.get(offset..offset + 2)?.try_into().ok()?);
    if packed == 0 {
        return None;
    }
    let mut language = String::new();
    for shift in [10, 5, 0] {
        let value = ((packed >> shift) & 0x1F) as u8;
        language.push(char::from(value + 0x60));
    }
    Some(language)
}

/// Reads the channel count and sample rate from an MP4 audio sample entry.
/// Symphonia leaves these unset for some AAC tracks until the first frame.
pub(crate) fn mp4_track_audio_layout(moov: &[u8], track_index: usize) -> Option<(usize, u32)> {
    let traks: Vec<&[u8]> = mp4_child_boxes(moov)
        .into_iter()
        .filter(|(kind, _)| kind == b"trak")
        .map(|(_, body)| body)
        .collect();
    let trak = traks.get(track_index)?;
    let stsd = mp4_find(mp4_find(mp4_find(trak, b"mdia")?, b"minf")?, b"stbl")?;
    let stsd = mp4_find(stsd, b"stsd")?;
    let entries = mp4_child_boxes(stsd.get(8..)?);
    let (_, sample_entry) = entries.first()?;
    let channels = u16::from_be_bytes(sample_entry.get(16..18)?.try_into().ok()?) as usize;
    let rate = u32::from_be_bytes(sample_entry.get(24..28)?.try_into().ok()?) >> 16;
    Some((channels, rate))
}

pub(crate) fn mp4_timescale(mvhd_or_mdhd: &[u8]) -> Option<u32> {
    let offset = match mvhd_or_mdhd.first()? {
        0 => 12,
        1 => 20,
        _ => return None,
    };
    Some(u32::from_be_bytes(
        mvhd_or_mdhd.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mp4Gapless {
    /// Samples to skip on the media timeline. This includes any codec delay
    /// that a container-level edit list covers.
    pub(crate) skip: usize,
    /// Presentation samples after the skip, when the container declares an
    /// exact length.
    pub(crate) limit: Option<usize>,
    /// Whether `limit` is a presentation length (edit list / `iTunSMPB`) rather
    /// than the raw media duration from `mdhd`.
    pub(crate) limit_is_presentation: bool,
}

/// Returns the MP4 priming skip and intended duration from an edit list (or
/// iTunes' `iTunSMPB` tag), both in media samples. If neither is present, the
/// track's own `mdhd` duration is used rather than the movie duration.
pub(crate) fn mp4_gapless_info(moov: &[u8], track_index: usize) -> Option<Mp4Gapless> {
    let movie_timescale = mp4_timescale(mp4_find(moov, b"mvhd")?)?;
    let traks: Vec<&[u8]> = mp4_child_boxes(moov)
        .into_iter()
        .filter(|(kind, _)| kind == b"trak")
        .map(|(_, body)| body)
        .collect();
    let trak = traks.get(track_index)?;
    let mdia = mp4_find(trak, b"mdia")?;
    let hdlr = mp4_find(mdia, b"hdlr")?;
    if hdlr.get(8..12) != Some(b"soun") {
        return None;
    }
    let mdhd = mp4_find(mdia, b"mdhd")?;
    let media_timescale = mp4_timescale(mdhd)?;
    let media_duration = mp4_media_duration(mdhd);

    if let Some(edts) = mp4_find(trak, b"edts") {
        if let Some(elst) = mp4_find(edts, b"elst") {
            if let Some((duration, media_time)) = mp4_media_edit(elst) {
                if media_time >= 0 && media_timescale > 0 && movie_timescale > 0 {
                    let total = (duration > 0).then(|| {
                        let scaled = u128::from(duration) * u128::from(media_timescale);
                        ((scaled + u128::from(movie_timescale) / 2) / u128::from(movie_timescale))
                            as usize
                    });
                    return Some(Mp4Gapless {
                        skip: media_time as usize,
                        limit: total,
                        limit_is_presentation: true,
                    });
                }
            }
        }
    }

    // iTunes stores the same numbers as hex text in a freeform tag. It is the
    // fallback for files whose muxer omitted the edit list.
    if let Some((skip, original)) = mp4_itunes_gapless_info(moov) {
        return Some(Mp4Gapless {
            skip,
            limit: (original > 0).then_some(original),
            limit_is_presentation: true,
        });
    }

    // No edit list means the track has no priming trim, but its media header
    // still gives the correct per-track duration. A movie-level duration is not
    // usable here: a short video track or a different movie timescale would
    // change the count.
    media_duration
        .filter(|duration| *duration > 0)
        .map(|duration| Mp4Gapless {
            skip: 0,
            limit: Some(duration),
            limit_is_presentation: false,
        })
}

pub(crate) fn mp4_media_edit(elst: &[u8]) -> Option<(u64, i64)> {
    let version = *elst.first()?;
    let count = u32::from_be_bytes(elst.get(4..8)?.try_into().ok()?) as usize;
    let entry_size = if version == 1 { 20 } else { 12 };
    for index in 0..count {
        let start = 8usize.checked_add(index.checked_mul(entry_size)?)?;
        let (duration, media_time) = if version == 1 {
            (
                u64::from_be_bytes(elst.get(start..start + 8)?.try_into().ok()?),
                i64::from_be_bytes(elst.get(start + 8..start + 16)?.try_into().ok()?),
            )
        } else {
            (
                u64::from(u32::from_be_bytes(
                    elst.get(start..start + 4)?.try_into().ok()?,
                )),
                i64::from(i32::from_be_bytes(
                    elst.get(start + 4..start + 8)?.try_into().ok()?,
                )),
            )
        };
        // Negative media times are empty edits (initial delay), not samples.
        if media_time >= 0 {
            return Some((duration, media_time));
        }
    }
    None
}

pub(crate) fn mp4_itunes_gapless_info(moov: &[u8]) -> Option<(usize, usize)> {
    let tag = b"iTunSMPB";
    let tag_position = moov.windows(tag.len()).position(|window| window == tag)?;
    let data_position = moov[tag_position..]
        .windows(4)
        .take(256)
        .position(|window| window == b"data")?;
    let payload_start = tag_position.checked_add(data_position)?.checked_add(12)?;
    let payload_end = moov
        .get(payload_start..)?
        .iter()
        .position(|byte| *byte == 0)
        .map(|length| payload_start + length)?;
    let text = std::str::from_utf8(moov.get(payload_start..payload_end)?).ok()?;
    let fields: Vec<&str> = text.split_whitespace().collect();
    if fields.len() < 3 {
        return None;
    }
    let skip = usize::from_str_radix(fields[0], 16).ok()?;
    let original = usize::from_str_radix(fields[2], 16).ok()?;
    Some((skip, original))
}

pub(crate) fn mp4_media_duration(mdhd: &[u8]) -> Option<usize> {
    let offset = match mdhd.first()? {
        0 => 16,
        1 => 24,
        _ => return None,
    };
    let duration = match mdhd.first()? {
        0 => u64::from(u32::from_be_bytes(
            mdhd.get(offset..offset + 4)?.try_into().ok()?,
        )),
        1 => u64::from_be_bytes(mdhd.get(offset..offset + 8)?.try_into().ok()?),
        _ => return None,
    };
    usize::try_from(duration).ok()
}

pub(crate) fn opus_stream_info(
    codec_params: &symphonia::core::codecs::CodecParameters,
) -> Result<OpusStreamInfo, ConvertError> {
    let head = codec_params
        .extra_data
        .as_deref()
        .filter(|data| data.len() >= 19 && data.starts_with(b"OpusHead"));
    let channels = codec_params
        .channels
        .map(|channels| channels.count())
        .or_else(|| head.map(|data| usize::from(data[9])))
        .filter(|channels| *channels > 0)
        .ok_or_else(|| {
            ConvertError::Audio("the Opus track does not declare a channel count".to_owned())
        })?;
    let mapping_family = head.map(|data| data[18]).unwrap_or(0);
    let channels = opus_channels(channels, mapping_family)?;
    // The OpusHead pre-skip is the decoder delay. Container delay metadata is
    // not the same number: in MP4 it is expressed in the track timescale and
    // can be much larger, so only fall back to it when there is no header.
    // Symphonia builds MP4 extra data by prefixing "OpusHead" to the big-endian
    // dOps payload, so version 0 fields are big-endian while an Ogg OpusHead
    // (version 1) stores them little-endian.
    let pre_skip = head
        .map(|data| {
            let bytes = [data[10], data[11]];
            if data[8] == 1 {
                u32::from(u16::from_le_bytes(bytes))
            } else {
                u32::from(u16::from_be_bytes(bytes))
            }
        })
        .or(codec_params.delay)
        .unwrap_or(0);
    Ok(OpusStreamInfo {
        channels,
        pre_skip,
        mapping_family,
    })
}

pub(crate) fn decode_flac(input: &[u8]) -> Result<DecodedAudio, ConvertError> {
    let input = strip_id3v2(input);
    let mut reader = claxon::FlacReader::new(Cursor::new(input))
        .map_err(|error| ConvertError::Audio(format!("could not open the FLAC stream: {error}")))?;
    let info = reader.streaminfo();
    let channels = info.channels as usize;
    let sample_rate = info.sample_rate;
    if channels == 0 || sample_rate == 0 || info.bits_per_sample == 0 {
        return Err(ConvertError::NoAudioTrack);
    }

    let scale = 1.0 / (1_u32 << (info.bits_per_sample - 1)) as f32;
    let mut samples = Vec::new();
    for sample in reader.samples() {
        let sample = sample.map_err(|error| {
            ConvertError::Audio(format!("could not decode FLAC samples: {error}"))
        })?;
        samples.push(sample as f32 * scale);
        if samples.len() > MAX_AUDIO_SAMPLES {
            return Err(ConvertError::AudioTooLarge);
        }
    }

    if samples.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    Ok(DecodedAudio {
        samples,
        channels,
        sample_rate,
        source_bits: u16::try_from(info.bits_per_sample).ok(),
    })
}

pub(crate) fn apply_audio_options(
    mut audio: DecodedAudio,
    options: ConversionOptions,
) -> Result<DecodedAudio, ConvertError> {
    if let Some(channels) = options.audio_channels {
        audio = remix_channels(audio, channels);
    }
    if let Some(sample_rate) = options.audio_sample_rate {
        audio = resample_audio(audio, sample_rate)?;
    }
    if audio.samples.len() > MAX_AUDIO_SAMPLES {
        return Err(ConvertError::AudioTooLarge);
    }
    Ok(audio)
}

/// Remixes any common channel layout to mono or stereo using the standard
/// ITU-R BS.775 coefficients, so the centre and surround channels are not
/// discarded. The result is scaled down only when the mix would clip.
///
/// Layouts follow the WAV/film channel order: 1 = mono, 2 = L/R, 3 = L/R/C,
/// 4 = quad L/R/BL/BR, 5 = L/R/C/BL/BR, 6 = L/R/C/LFE/BL/BR,
/// 7 = 6.1 L/R/C/LFE/BC/SL/SR, and 8 = 7.1 L/R/C/LFE/BL/BR/SL/SR. Centre and
/// surround channels use 0.707; quad surrounds use 0.707 on their own side.
/// LFE is included at 0.5 rather than omitted: that keeps low-frequency energy
/// in the fold-down while limiting the headroom it consumes. Layouts above
/// eight channels are treated as 7.1 plus equally weighted extra channels.
pub(crate) fn remix_channels(mut audio: DecodedAudio, target: AudioChannels) -> DecodedAudio {
    let target_channels = target.count();
    if audio.channels == target_channels {
        return audio;
    }

    let frames = audio.samples.len() / audio.channels;
    let mut samples = Vec::with_capacity(frames * target_channels);
    for frame in audio.samples.chunks_exact(audio.channels) {
        match target {
            AudioChannels::Mono => samples.push(downmix_mono(frame)),
            AudioChannels::Stereo => {
                if audio.channels == 1 {
                    samples.extend_from_slice(&[frame[0], frame[0]]);
                } else {
                    let (left, right) = downmix_stereo(frame);
                    samples.extend_from_slice(&[left, right]);
                }
            }
        }
    }

    // A 5.1 downmix can exceed full scale even when every source channel is
    // within it. Attenuate the whole mix rather than clipping individual peaks.
    let peak = samples
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    if peak > 1.0 {
        let scale = 1.0 / peak;
        for sample in &mut samples {
            *sample *= scale;
        }
    }

    audio.samples = samples;
    audio.channels = target_channels;
    audio
}

/// Mono fold-down for the layouts documented on [`remix_channels`]. Front
/// left/right are averaged, centre uses 0.707, and LFE and surrounds use 0.5.
pub(crate) fn downmix_mono(frame: &[f32]) -> f32 {
    let channel = |index: usize| frame.get(index).copied().unwrap_or(0.0);
    match frame.len() {
        0 => 0.0,
        1 => frame[0],
        2 => 0.5 * (channel(0) + channel(1)),
        3 => 0.5 * (channel(0) + channel(1)) + 0.707 * channel(2),
        4 => 0.5 * (channel(0) + channel(1)) + 0.5 * (channel(2) + channel(3)),
        5 => 0.5 * (channel(0) + channel(1)) + 0.707 * channel(2) + 0.5 * (channel(3) + channel(4)),
        6 => {
            0.5 * (channel(0) + channel(1))
                + 0.707 * channel(2)
                + 0.5 * channel(3)
                + 0.5 * (channel(4) + channel(5))
        }
        7 => {
            0.5 * (channel(0) + channel(1))
                + 0.707 * channel(2)
                + 0.5 * channel(3)
                + 0.5 * (channel(4) + channel(5) + channel(6))
        }
        8 => {
            0.5 * (channel(0) + channel(1))
                + 0.707 * channel(2)
                + 0.5 * channel(3)
                + 0.5 * (channel(4) + channel(5) + channel(6) + channel(7))
        }
        _ => {
            // 7.1 plus any extra channels, which are split equally so they are
            // not silently dropped.
            0.5 * (channel(0) + channel(1))
                + 0.707 * channel(2)
                + 0.5 * channel(3)
                + 0.5 * frame.iter().skip(4).sum::<f32>()
        }
    }
}

/// Stereo fold-down for the layouts documented on [`remix_channels`].
pub(crate) fn downmix_stereo(frame: &[f32]) -> (f32, f32) {
    let channel = |index: usize| frame.get(index).copied().unwrap_or(0.0);
    let mut left = channel(0);
    let mut right = channel(1);
    match frame.len() {
        0 => return (0.0, 0.0),
        1 => return (frame[0], frame[0]),
        2 => return (left, right),
        3 => {
            left += 0.707 * channel(2);
            right += 0.707 * channel(2);
        }
        4 => {
            left += 0.707 * channel(2);
            right += 0.707 * channel(3);
        }
        5 => {
            left += 0.707 * channel(2) + 0.707 * channel(3);
            right += 0.707 * channel(2) + 0.707 * channel(4);
        }
        6 => {
            left += 0.707 * channel(2) + 0.5 * channel(3) + 0.707 * channel(4);
            right += 0.707 * channel(2) + 0.5 * channel(3) + 0.707 * channel(5);
        }
        7 => {
            left += 0.707 * channel(2) + 0.5 * channel(3) + 0.5 * channel(4) + 0.707 * channel(5);
            right += 0.707 * channel(2) + 0.5 * channel(3) + 0.5 * channel(4) + 0.707 * channel(6);
        }
        8 => {
            left += 0.707 * channel(2) + 0.5 * channel(3) + 0.707 * channel(4) + 0.707 * channel(6);
            right +=
                0.707 * channel(2) + 0.5 * channel(3) + 0.707 * channel(5) + 0.707 * channel(7);
        }
        _ => {
            // 7.1 plus any extra channels, which are split equally so they are
            // not silently dropped.
            left += 0.707 * channel(2) + 0.5 * channel(3) + 0.707 * channel(4) + 0.707 * channel(6);
            right +=
                0.707 * channel(2) + 0.5 * channel(3) + 0.707 * channel(5) + 0.707 * channel(7);
            for sample in frame.iter().skip(8) {
                left += 0.5 * sample;
                right += 0.5 * sample;
            }
        }
    }
    (left, right)
}

pub(crate) const RESAMPLE_ZERO_CROSSINGS: usize = 16;

pub(crate) const RESAMPLE_PHASES: usize = 256;

/// Band-limited resampling with a windowed-sinc low-pass filter. The filter
/// cutoff follows the lower of the two rates, so downsampling does not alias
/// high frequencies back into the audible band.
pub(crate) fn resample_audio(
    audio: DecodedAudio,
    target_rate: u32,
) -> Result<DecodedAudio, ConvertError> {
    let target_rate = target_rate.clamp(
        ConversionOptions::MIN_AUDIO_SAMPLE_RATE,
        ConversionOptions::MAX_AUDIO_SAMPLE_RATE,
    );
    if target_rate == audio.sample_rate {
        return Ok(audio);
    }

    let channels = audio.channels;
    let source_frames = audio.samples.len() / channels;
    if source_frames == 0 {
        return Ok(audio);
    }

    let target_frames = ((source_frames as f64 * f64::from(target_rate))
        / f64::from(audio.sample_rate))
    .round()
    .max(1.0) as usize;
    let sample_count = target_frames
        .checked_mul(channels)
        .ok_or(ConvertError::AudioTooLarge)?;
    if sample_count > MAX_AUDIO_SAMPLES {
        return Err(ConvertError::AudioTooLarge);
    }

    let ratio = f64::from(target_rate) / f64::from(audio.sample_rate);
    let cutoff = ratio.min(1.0) * 0.95;
    let table = resample_kernel_table(cutoff);
    let taps = RESAMPLE_ZERO_CROSSINGS * 2;
    let mut samples = Vec::with_capacity(sample_count);
    let mut mixed = vec![0.0_f64; channels];

    for frame in 0..target_frames {
        let position = frame as f64 / ratio;
        let base = position.floor();
        let phase =
            (((position - base) * RESAMPLE_PHASES as f64) as usize).min(RESAMPLE_PHASES - 1);
        let weights = &table[phase * taps..phase * taps + taps];
        mixed.fill(0.0);
        for (tap, weight) in weights.iter().enumerate() {
            let index = base as isize + tap as isize - (RESAMPLE_ZERO_CROSSINGS as isize - 1);
            let index = index.clamp(0, source_frames as isize - 1) as usize;
            let source = &audio.samples[index * channels..(index + 1) * channels];
            for channel in 0..channels {
                mixed[channel] += f64::from(source[channel]) * weight;
            }
        }
        for value in &mixed {
            samples.push(*value as f32);
        }
    }

    Ok(DecodedAudio {
        samples,
        channels,
        sample_rate: target_rate,
        source_bits: audio.source_bits,
    })
}

pub(crate) fn resample_kernel_table(cutoff: f64) -> Vec<f64> {
    let taps = RESAMPLE_ZERO_CROSSINGS * 2;
    let mut table = vec![0.0; RESAMPLE_PHASES * taps];
    for phase in 0..RESAMPLE_PHASES {
        let fraction = phase as f64 / RESAMPLE_PHASES as f64;
        let mut sum = 0.0;
        for tap in 0..taps {
            let distance = tap as f64 - (RESAMPLE_ZERO_CROSSINGS as f64 - 1.0) - fraction;
            let normalized = distance / RESAMPLE_ZERO_CROSSINGS as f64;
            let window = if normalized.abs() <= 1.0 {
                0.42 + 0.5 * (std::f64::consts::PI * normalized).cos()
                    + 0.08 * (2.0 * std::f64::consts::PI * normalized).cos()
            } else {
                0.0
            };
            let argument = std::f64::consts::PI * cutoff * distance;
            let sinc = if argument.abs() < 1e-9 {
                1.0
            } else {
                argument.sin() / argument
            };
            let weight = cutoff * sinc * window;
            table[phase * taps + tap] = weight;
            sum += weight;
        }
        if sum.abs() > 1e-12 {
            for tap in 0..taps {
                table[phase * taps + tap] /= sum;
            }
        }
    }
    table
}

pub(crate) fn id3v2_len(input: &[u8]) -> Option<usize> {
    if input.len() < 10 || &input[..3] != b"ID3" {
        return None;
    }
    let size = ((input[6] as usize & 0x7f) << 21)
        | ((input[7] as usize & 0x7f) << 14)
        | ((input[8] as usize & 0x7f) << 7)
        | (input[9] as usize & 0x7f);
    let footer = if input[5] & 0x10 != 0 { 10 } else { 0 };
    let end = 10 + size + footer;
    (end <= input.len()).then_some(end)
}

pub(crate) fn preserve_mp3_id3v2(input: &[u8], output: Vec<u8>) -> Vec<u8> {
    let Some(tag_len) = id3v2_len(input) else {
        return output;
    };
    let audio = strip_id3v2(&output);
    let mut result = Vec::with_capacity(tag_len + audio.len());
    result.extend_from_slice(&input[..tag_len]);
    result.extend_from_slice(audio);
    result
}

pub(crate) fn flac_metadata_end(input: &[u8]) -> Option<usize> {
    if !input.starts_with(b"fLaC") {
        return None;
    }
    let mut position = 4;
    loop {
        let header = input.get(position..position + 4)?;
        let last = header[0] & 0x80 != 0;
        let length =
            ((header[1] as usize) << 16) | ((header[2] as usize) << 8) | header[3] as usize;
        position = position.checked_add(4 + length)?;
        if last {
            return (position <= input.len()).then_some(position);
        }
    }
}

pub(crate) fn flac_metadata_blocks(input: &[u8]) -> Option<Vec<(u8, Vec<u8>)>> {
    let end = flac_metadata_end(input)?;
    let mut blocks = Vec::new();
    let mut position = 4;
    while position < end {
        let header = input.get(position..position + 4)?;
        let kind = header[0] & 0x7f;
        let length =
            ((header[1] as usize) << 16) | ((header[2] as usize) << 8) | header[3] as usize;
        let data = input.get(position + 4..position + 4 + length)?.to_vec();
        blocks.push((kind, data));
        position += 4 + length;
    }
    Some(blocks)
}

pub(crate) fn rebuild_flac(blocks: &[(u8, Vec<u8>)], audio: &[u8]) -> Vec<u8> {
    let mut output = b"fLaC".to_vec();
    for (index, (kind, data)) in blocks.iter().enumerate() {
        let last = index + 1 == blocks.len();
        output.push(if last { 0x80 } else { 0 } | kind);
        output.extend_from_slice(&(data.len() as u32).to_be_bytes()[1..]);
        output.extend_from_slice(data);
    }
    output.extend_from_slice(audio);
    output
}

/// Copies Vorbis comments and pictures from a FLAC input to FLAC output. The
/// encoder writes fresh audio metadata, so the descriptive blocks are replaced
/// rather than appended a second time.
pub(crate) fn preserve_flac_tags(input: &[u8], output: Vec<u8>) -> Vec<u8> {
    let input = strip_id3v2(input);
    let Some(input_blocks) = flac_metadata_blocks(input) else {
        return output;
    };
    let Some(mut output_blocks) = flac_metadata_blocks(&output) else {
        return output;
    };
    let tags: Vec<_> = input_blocks
        .into_iter()
        .filter(|(kind, data)| (*kind == 4 || *kind == 6) && data.len() <= 0xFF_FFFF)
        .collect();
    if tags.is_empty() {
        return output;
    }
    output_blocks.retain(|(kind, _)| *kind != 4 && *kind != 6);
    let insert_at = usize::from(output_blocks.first().is_some_and(|(kind, _)| *kind == 0));
    for (offset, tag) in tags.into_iter().enumerate() {
        output_blocks.insert(insert_at + offset, tag);
    }
    let Some(audio_start) = flac_metadata_end(&output) else {
        return output;
    };
    rebuild_flac(&output_blocks, &output[audio_start..])
}

pub(crate) fn strip_id3v2(input: &[u8]) -> &[u8] {
    if input.len() < 10 || &input[..3] != b"ID3" {
        return input;
    }

    let size = ((input[6] as usize & 0x7f) << 21)
        | ((input[7] as usize & 0x7f) << 14)
        | ((input[8] as usize & 0x7f) << 7)
        | (input[9] as usize & 0x7f);
    let footer = if input[5] & 0x10 != 0 { 10 } else { 0 };
    let end = 10 + size + footer;
    if end <= input.len() {
        &input[end..]
    } else {
        input
    }
}

/// Converts an audio input and writes WAV output directly to `writer`,
/// avoiding a second full-size copy of the encoded file in memory.
pub fn convert_audio_to_wav_writer<W: Write>(
    input_name: &str,
    input: &[u8],
    options: ConversionOptions,
    writer: &mut W,
) -> Result<(), ConvertError> {
    let extension = file_extension(input_name);
    if !is_audio_extension(&extension) {
        return Err(unsupported(&extension, OutputFormat::Wav));
    }
    let audio = apply_audio_options(
        decode_audio(input, &extension, options.audio_track)?,
        options,
    )?;
    write_wav(writer, &audio, options.audio_bit_depth)
}

/// The output name used by [`convert_audio_to_wav_writer`].
pub fn wav_output_name(input_name: &str) -> PathBuf {
    output_name(input_name, "wav")
}

/// The output name for a streaming audio conversion.
pub fn audio_output_name(input_name: &str, target: OutputFormat) -> PathBuf {
    output_name(input_name, target.extension())
}

pub(crate) fn encode_wav(
    audio: &DecodedAudio,
    bit_depth: AudioBitDepth,
) -> Result<Vec<u8>, ConvertError> {
    let mut output = Vec::new();
    write_wav(&mut output, audio, bit_depth)?;
    Ok(output)
}

pub(crate) fn write_wav<W: Write>(
    writer: &mut W,
    audio: &DecodedAudio,
    bit_depth: AudioBitDepth,
) -> Result<(), ConvertError> {
    if audio.channels == 0 {
        return Err(ConvertError::NoAudioTrack);
    }
    let usable_samples = audio.samples.len() - audio.samples.len() % audio.channels;
    let samples = &audio.samples[..usable_samples];
    let frames = samples.len() / audio.channels;
    let channels = u16::try_from(audio.channels).map_err(|_| ConvertError::AudioTooLarge)?;
    let bytes_per_sample = match bit_depth {
        AudioBitDepth::Bit16 => 2_u16,
        AudioBitDepth::Bit24 => 3,
        AudioBitDepth::Float32 => 4,
    };
    let block_align = channels
        .checked_mul(bytes_per_sample)
        .ok_or(ConvertError::AudioTooLarge)?;
    let byte_rate = audio
        .sample_rate
        .checked_mul(u32::from(block_align))
        .ok_or(ConvertError::AudioTooLarge)?;
    let data_size = u32::try_from(
        samples
            .len()
            .checked_mul(bytes_per_sample as usize)
            .ok_or(ConvertError::AudioTooLarge)?,
    )
    .map_err(|_| ConvertError::AudioTooLarge)?;
    let is_float = bit_depth == AudioBitDepth::Float32;
    let fact_size = if is_float { 12 } else { 0 };
    let riff_size = 36_u32
        .checked_add(fact_size)
        .and_then(|size| size.checked_add(data_size))
        .ok_or(ConvertError::AudioTooLarge)?;

    let mut output = Vec::with_capacity(44 + fact_size as usize);
    output.extend_from_slice(b"RIFF");
    output.extend_from_slice(&riff_size.to_le_bytes());
    output.extend_from_slice(b"WAVEfmt ");
    output.extend_from_slice(&16_u32.to_le_bytes());
    output.extend_from_slice(&if is_float { 3_u16 } else { 1 }.to_le_bytes());
    output.extend_from_slice(&channels.to_le_bytes());
    output.extend_from_slice(&audio.sample_rate.to_le_bytes());
    output.extend_from_slice(&byte_rate.to_le_bytes());
    output.extend_from_slice(&block_align.to_le_bytes());
    output.extend_from_slice(&bit_depth.bits().to_le_bytes());
    if is_float {
        output.extend_from_slice(b"fact");
        output.extend_from_slice(&4_u32.to_le_bytes());
        output.extend_from_slice(&(frames as u32).to_le_bytes());
    }
    output.extend_from_slice(b"data");
    output.extend_from_slice(&data_size.to_le_bytes());
    writer.write_all(&output)?;

    let mut dither = Dither::new();
    let mut samples_buffer = Vec::with_capacity(64 * 1024);
    for sample in samples {
        match bit_depth {
            AudioBitDepth::Bit16 => samples_buffer.extend_from_slice(
                &dither
                    .quantize_i16(*sample, audio.source_bits)
                    .to_le_bytes(),
            ),
            AudioBitDepth::Bit24 => {
                let value = dither.quantize_i32(*sample, 24, audio.source_bits);
                samples_buffer.extend_from_slice(&value.to_le_bytes()[..3]);
            }
            AudioBitDepth::Float32 => {
                samples_buffer.extend_from_slice(&(*sample).clamp(-1.0, 1.0).to_le_bytes())
            }
        }
        if samples_buffer.len() >= 32 * 1024 {
            writer.write_all(&samples_buffer)?;
            samples_buffer.clear();
        }
    }
    writer.write_all(&samples_buffer)?;
    Ok(())
}

/// Triangular-PDF dither for the reduction to 16-bit output. The generator is
/// deterministic so conversions are reproducible. Higher depths are quantized
/// without dither.
pub(crate) struct Dither {
    pub(crate) state: u32,
}

impl Dither {
    pub(crate) fn new() -> Self {
        Self { state: 0x1234_5678 }
    }

    pub(crate) fn next(&mut self) -> f32 {
        self.state = self
            .state
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        let first = (self.state >> 8) as f32 / 16_777_216.0;
        self.state = self
            .state
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        let second = (self.state >> 8) as f32 / 16_777_216.0;
        first - second
    }

    pub(crate) fn quantize_i16(&mut self, sample: f32, source_bits: Option<u16>) -> i16 {
        // A source known to be 16-bit needs no dither when it is written back
        // at 16 bits. Deeper sources and f32-decoded sources with no reported
        // integer precision are reductions and are always dithered.
        if source_bits != Some(16) {
            let dither = self.next() / f32::from(i16::MAX);
            // Clamp before adding dither so samples at or beyond full scale
            // saturate identically in every decode path.
            sample_to_i16((sample.clamp(-1.0, 1.0) + dither).clamp(-1.0, 1.0))
        } else {
            sample_to_i16(sample)
        }
    }

    pub(crate) fn quantize_i32(
        &mut self,
        sample: f32,
        bits: u32,
        _source_bits: Option<u16>,
    ) -> i32 {
        let scale = ((1_i64 << (bits - 1)) - 1) as f32;
        (sample.clamp(-1.0, 1.0) * scale).round() as i32
    }
}

pub(crate) fn encode_aiff(audio: &DecodedAudio) -> Result<Vec<u8>, ConvertError> {
    let channels = u16::try_from(audio.channels).map_err(|_| ConvertError::AudioTooLarge)?;
    let usable_samples = audio.samples.len() - audio.samples.len() % audio.channels;
    let frames =
        u32::try_from(usable_samples / audio.channels).map_err(|_| ConvertError::AudioTooLarge)?;
    let data_size = u32::try_from(usable_samples * 2).map_err(|_| ConvertError::AudioTooLarge)?;
    let form_size = 46_u32
        .checked_add(data_size)
        .ok_or(ConvertError::AudioTooLarge)?;

    let mut output = Vec::with_capacity(54 + data_size as usize);
    output.extend_from_slice(b"FORM");
    output.extend_from_slice(&form_size.to_be_bytes());
    output.extend_from_slice(b"AIFF");
    output.extend_from_slice(b"COMM");
    output.extend_from_slice(&18_u32.to_be_bytes());
    output.extend_from_slice(&channels.to_be_bytes());
    output.extend_from_slice(&frames.to_be_bytes());
    output.extend_from_slice(&16_u16.to_be_bytes());
    output.extend_from_slice(&extended_sample_rate(audio.sample_rate));
    output.extend_from_slice(b"SSND");
    output.extend_from_slice(&(8 + data_size).to_be_bytes());
    output.extend_from_slice(&0_u32.to_be_bytes());
    output.extend_from_slice(&0_u32.to_be_bytes());
    for sample in &audio.samples[..usable_samples] {
        output.extend_from_slice(&sample_to_i16(*sample).to_be_bytes());
    }
    Ok(output)
}

pub(crate) fn extended_sample_rate(sample_rate: u32) -> [u8; 10] {
    if sample_rate == 0 {
        return [0; 10];
    }
    let exponent = 31 - sample_rate.leading_zeros();
    let mantissa = u64::from(sample_rate) << (63 - exponent);
    let biased_exponent = (exponent + 16_383) as u16;
    let mut bytes = [0; 10];
    bytes[..2].copy_from_slice(&biased_exponent.to_be_bytes());
    bytes[2..].copy_from_slice(&mantissa.to_be_bytes());
    bytes
}

pub(crate) fn encode_au(audio: &DecodedAudio) -> Result<Vec<u8>, ConvertError> {
    let channels = u32::try_from(audio.channels).map_err(|_| ConvertError::AudioTooLarge)?;
    let usable_samples = audio.samples.len() - audio.samples.len() % audio.channels;
    let data_size = u32::try_from(usable_samples * 2).map_err(|_| ConvertError::AudioTooLarge)?;

    let mut output = Vec::with_capacity(24 + data_size as usize);
    output.extend_from_slice(b".snd");
    output.extend_from_slice(&24_u32.to_be_bytes());
    output.extend_from_slice(&data_size.to_be_bytes());
    output.extend_from_slice(&3_u32.to_be_bytes());
    output.extend_from_slice(&audio.sample_rate.to_be_bytes());
    output.extend_from_slice(&channels.to_be_bytes());
    for sample in &audio.samples[..usable_samples] {
        output.extend_from_slice(&sample_to_i16(*sample).to_be_bytes());
    }
    Ok(output)
}

pub(crate) fn opus_sample_rate(sample_rate: u32) -> u32 {
    match sample_rate {
        0..=8_000 => 8_000,
        8_001..=12_000 => 12_000,
        12_001..=16_000 => 16_000,
        16_001..=24_000 => 24_000,
        _ => 48_000,
    }
}

pub(crate) enum OpusPcmEncoder {
    Single(Box<opus_pure::OpusEncoder>),
    Multi(Box<opus_pure::OpusMSEncoder>),
}

impl OpusPcmEncoder {
    pub(crate) fn new(sample_rate: u32, channels: usize) -> Result<Self, ConvertError> {
        use opus_pure::{Application, OpusEncoder, OpusMSEncoder};

        if channels <= 2 {
            let mut encoder = OpusEncoder::new(sample_rate as i32, channels, Application::Audio)
                .map_err(|error| {
                    ConvertError::Audio(format!("could not start the Opus encoder: {error}"))
                })?;
            encoder.bitrate_bps = if channels == 1 { 64_000 } else { 96_000 };
            Ok(Self::Single(Box::new(encoder)))
        } else if channels <= 8 {
            let mut encoder =
                OpusMSEncoder::new(sample_rate as i32, channels, 1, Application::Audio).map_err(
                    |error| {
                        ConvertError::Audio(format!("could not start the Opus encoder: {error}"))
                    },
                )?;
            encoder.set_bitrate(64_000 * channels as i32);
            Ok(Self::Multi(Box::new(encoder)))
        } else {
            Err(ConvertError::UnsupportedChannelLayout {
                codec: "Opus",
                channels,
            })
        }
    }

    pub(crate) fn head(&self, sample_rate: u32) -> opus_pure::OpusHead {
        use opus_pure::OpusHead;
        match self {
            Self::Single(encoder) => OpusHead::for_encoder(encoder, sample_rate),
            Self::Multi(encoder) => OpusHead::for_ms_encoder(encoder, sample_rate),
        }
    }

    pub(crate) fn encode(
        &mut self,
        block: &[f32],
        frame_size: usize,
        packet: &mut [u8],
    ) -> Result<usize, opus_pure::Error> {
        match self {
            Self::Single(encoder) => encoder.encode(block, frame_size, packet),
            Self::Multi(encoder) => encoder.encode(block, frame_size, packet),
        }
    }
}

pub(crate) fn encode_opus(mut audio: DecodedAudio) -> Result<Vec<u8>, ConvertError> {
    use opus_pure::{OggOpusWriter, MAX_PACKET_BYTES};

    if audio.channels == 0 || audio.sample_rate == 0 || audio.samples.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    let sample_rate = opus_sample_rate(audio.sample_rate);
    if audio.sample_rate != sample_rate {
        audio = resample_audio(audio, sample_rate)?;
    }

    let channels = audio.channels;
    let mut encoder = OpusPcmEncoder::new(sample_rate, channels)?;
    if channels > 2 {
        reorder_film_to_opus_channels(&mut audio.samples, channels);
    }
    let head = encoder.head(sample_rate);
    let pre_skip = u64::from(head.pre_skip);
    let mut writer = OggOpusWriter::new(Vec::new(), head).map_err(|error| {
        ConvertError::Audio(format!("could not start the Opus stream: {error}"))
    })?;

    let total = audio.samples.len() / channels;
    let frame_size = (sample_rate / 50) as usize;
    let ticks = 48_000 / sample_rate;
    // The encoder runs `pre_skip` samples behind its input, so flush that much
    // silence and round up to whole frames.
    let flush = (pre_skip + u64::from(ticks) - 1) / u64::from(ticks);
    let frames = (total + flush as usize).div_ceil(frame_size);
    let final_granule = pre_skip + total as u64 * 48_000 / u64::from(sample_rate);
    let mut packet = vec![0_u8; MAX_PACKET_BYTES];
    let mut block = vec![0.0f32; frame_size * channels];

    for i in 0..frames {
        let offset = i * frame_size;
        let end = (offset + frame_size).min(total);
        block.fill(0.0);
        if offset < total {
            block[..(end - offset) * channels]
                .copy_from_slice(&audio.samples[offset * channels..end * channels]);
        }
        let len = encoder
            .encode(&block, frame_size, &mut packet)
            .map_err(|error| {
                ConvertError::Audio(format!("could not encode Opus audio: {error}"))
            })?;
        if i + 1 == frames {
            let duration = final_granule.saturating_sub(writer.granule() as u64) as u32;
            writer
                .write_packet_with_duration(&packet[..len], duration)
                .map_err(|error| {
                    ConvertError::Audio(format!("could not write the Opus stream: {error}"))
                })?;
        } else {
            writer.write_packet(&packet[..len]).map_err(|error| {
                ConvertError::Audio(format!("could not write the Opus stream: {error}"))
            })?;
        }
    }

    writer
        .finish()
        .map_err(|error| ConvertError::Audio(format!("could not finish the Opus stream: {error}")))
}

pub(crate) fn sample_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}

pub(crate) fn mp3_sample_rate(sample_rate: u32) -> u32 {
    match sample_rate {
        0..=8_000 => 8_000,
        8_001..=11_025 => 11_025,
        11_026..=12_000 => 12_000,
        12_001..=16_000 => 16_000,
        16_001..=22_050 => 22_050,
        22_051..=24_000 => 24_000,
        24_001..=32_000 => 32_000,
        32_001..=44_100 => 44_100,
        _ => 48_000,
    }
}

pub(crate) fn mp3_lame_bitrate(bitrate: Mp3Bitrate) -> mp3lame_encoder::Bitrate {
    use mp3lame_encoder::Bitrate;
    match bitrate {
        Mp3Bitrate::Kbps128 => Bitrate::Kbps128,
        Mp3Bitrate::Kbps192 => Bitrate::Kbps192,
        Mp3Bitrate::Kbps256 => Bitrate::Kbps256,
        Mp3Bitrate::Kbps320 => Bitrate::Kbps320,
    }
}

pub(crate) fn encode_mp3(
    audio: DecodedAudio,
    bitrate: Mp3Bitrate,
) -> Result<Vec<u8>, ConvertError> {
    use mp3lame_encoder::{Builder, FlushNoGap, InterleavedPcm, MonoPcm, Quality};

    if audio.channels == 0 || audio.sample_rate == 0 || audio.samples.is_empty() {
        return Err(ConvertError::NoAudioTrack);
    }
    let mut audio = if audio.channels > 2 {
        remix_channels(audio, AudioChannels::Stereo)
    } else {
        audio
    };
    let sample_rate = mp3_sample_rate(audio.sample_rate);
    if audio.sample_rate != sample_rate {
        audio = resample_audio(audio, sample_rate)?;
    }
    let channels = audio.channels;

    let mut builder = Builder::new()
        .ok_or_else(|| ConvertError::Audio("could not start the MP3 encoder".to_owned()))?;
    builder
        .set_num_channels(channels as u8)
        .and_then(|_| builder.set_sample_rate(sample_rate))
        .and_then(|_| builder.set_brate(mp3_lame_bitrate(bitrate)))
        .and_then(|_| builder.set_quality(Quality::NearBest))
        .map_err(|error| {
            ConvertError::Audio(format!("could not configure the MP3 encoder: {error}"))
        })?;
    let mut encoder = builder.build().map_err(|error| {
        ConvertError::Audio(format!("could not start the MP3 encoder: {error}"))
    })?;

    let mut output = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(audio.frames()));
    if channels == 1 {
        encoder
            .encode_to_vec(MonoPcm(&audio.samples), &mut output)
            .map_err(|error| ConvertError::Audio(format!("could not encode MP3 audio: {error}")))?;
    } else {
        encoder
            .encode_to_vec(InterleavedPcm(&audio.samples), &mut output)
            .map_err(|error| ConvertError::Audio(format!("could not encode MP3 audio: {error}")))?;
    }
    encoder
        .flush_to_vec::<FlushNoGap>(&mut output)
        .map_err(|error| {
            ConvertError::Audio(format!("could not finish the MP3 stream: {error}"))
        })?;
    Ok(output)
}

pub(crate) fn encode_flac(
    audio: &DecodedAudio,
    bit_depth: AudioBitDepth,
) -> Result<Vec<u8>, ConvertError> {
    use flacenc::component::BitRepr;
    use flacenc::error::Verify;

    let bits = match bit_depth {
        AudioBitDepth::Bit16 => 16,
        AudioBitDepth::Bit24 => 24,
        AudioBitDepth::Float32 => {
            return Err(ConvertError::UnsupportedBitDepth {
                format: "FLAC",
                depth: bit_depth.label(),
            })
        }
    };
    let usable_samples = audio.samples.len() - audio.samples.len() % audio.channels;
    let mut dither = Dither::new();
    let integer_samples: Vec<i32> = audio.samples[..usable_samples]
        .iter()
        .map(|sample| {
            if bits == 16 {
                i32::from(dither.quantize_i16(*sample, audio.source_bits))
            } else {
                dither.quantize_i32(*sample, 24, audio.source_bits)
            }
        })
        .collect();

    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, error)| ConvertError::Audio(error.to_string()))?;
    let source = flacenc::source::MemSource::from_samples(
        &integer_samples,
        audio.channels,
        bits as usize,
        audio.sample_rate as usize,
    );
    let mut stream = flacenc::encode_with_fixed_block_size(&config, source, config.block_size)
        .map_err(|error| ConvertError::Audio(error.to_string()))?;
    // The encoder reports the shortest block (the final partial block) as the
    // minimum, but writes fixed-block frame numbers. Readers such as Symphonia
    // treat that as a variable-block stream and reject the frames, so describe
    // the stream as fixed-size. A shorter final block is still valid FLAC.
    stream
        .stream_info_mut()
        .set_block_sizes(config.block_size, config.block_size)
        .map_err(|error| ConvertError::Audio(error.to_string()))?;
    let mut sink = flacenc::bitsink::ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|error| ConvertError::Audio(error.to_string()))?;
    Ok(sink.as_slice().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::convert::{convert_bytes, convert_bytes_with_options};
    use crate::error::ConvertError;
    use crate::format::OutputFormat;
    use crate::options::ConversionOptions;

    use crate::test_support::{
        assert_audible, assert_mp3_tone, fixture_path, mp3_first_frame_bitrate, parse_wav, rms,
        stream_temp_dir, stream_wav, wav_24_bit_at, wav_24_bit_samples, wav_channels_at,
        wav_fixture, wav_fixture_at, wav_float_samples, wav_layout, wav_tone_at, ParsedWav,
    };
    use std::{io::Cursor, path::PathBuf};
    fn flac_24_bit_samples(bytes: &[u8]) -> Vec<i32> {
        let mut reader = claxon::FlacReader::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(reader.streaminfo().bits_per_sample, 24);
        reader.samples().map(Result::unwrap).collect()
    }
    fn flac_16_bit_samples(bytes: &[u8]) -> Vec<i32> {
        let mut reader = claxon::FlacReader::new(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(reader.streaminfo().bits_per_sample, 16);
        reader.samples().map(Result::unwrap).collect()
    }
    /// The deterministic 16-bit truncation the old bypass produced: no dither,
    /// just the same f32 samples rounded and clamped.
    fn undithered_i16(samples: &[f32]) -> Vec<i16> {
        samples
            .iter()
            .map(|&sample| sample_to_i16(sample))
            .collect()
    }
    /// Encodes a 5.1 Ogg Opus stream with the Vorbis channel order the Opus
    /// mapping family expects: FL, FC, FR, RL, RR, LFE.
    fn opus_surround_fixture(channels: [f32; 6]) -> Vec<u8> {
        use opus_pure::{Application, OggOpusWriter, OpusHead, OpusMSEncoder, MAX_PACKET_BYTES};

        let frame_size = 960;
        let mut encoder = OpusMSEncoder::new(48_000, 6, 1, Application::Audio).unwrap();
        let head = OpusHead::for_ms_encoder(&encoder, 48_000);
        let mut writer = OggOpusWriter::new(Vec::new(), head).unwrap();
        let mut block = vec![0.0_f32; frame_size * 6];
        let mut packet = vec![0_u8; MAX_PACKET_BYTES];
        for _ in 0..12 {
            for frame in 0..frame_size {
                block[frame * 6..(frame + 1) * 6].copy_from_slice(&channels);
            }
            let len = encoder.encode(&block, frame_size, &mut packet).unwrap();
            writer.write_packet(&packet[..len]).unwrap();
        }
        writer.finish().unwrap()
    }
    fn decoded_tone_frequency(parsed: &ParsedWav) -> f64 {
        let channels = usize::from(parsed.channels);
        let frames = parsed.samples.len() / channels;
        assert!(frames > 4, "not enough samples to measure a tone");
        let start = frames / 4 * channels;
        let end = frames * 3 / 4 * channels;
        let first_channel: Vec<i16> = parsed.samples[start..end]
            .iter()
            .step_by(channels)
            .copied()
            .collect();
        let crossings = first_channel
            .windows(2)
            .filter(|pair| (pair[0] < 0) != (pair[1] < 0))
            .count();
        crossings as f64 * f64::from(parsed.sample_rate) / (2.0 * first_channel.len() as f64)
    }
    fn decoded_rms(parsed: &ParsedWav) -> f64 {
        (parsed
            .samples
            .iter()
            .map(|sample| f64::from(*sample) * f64::from(*sample))
            .sum::<f64>()
            / parsed.samples.len() as f64)
            .sqrt()
    }
    #[test]
    fn converts_wav_audio_to_flac_and_back() {
        let wav = wav_fixture();
        let original = parse_wav(&wav);
        assert_eq!(original.sample_rate, 8_000);
        assert_eq!(original.channels, 2);
        assert_eq!(original.bits_per_sample, 16);
        assert_eq!(original.samples.len(), 16_000);

        let same_format = convert_bytes("tone.wav", &wav, OutputFormat::Wav).unwrap();
        let reencoded = parse_wav(&same_format[0].bytes);
        assert_eq!(reencoded.sample_rate, original.sample_rate);
        assert_eq!(reencoded.channels, original.channels);
        assert_eq!(reencoded.samples, original.samples);
        assert_audible(&reencoded.samples);

        let flac = convert_bytes("tone.wav", &wav, OutputFormat::Flac).unwrap();
        assert_eq!(flac[0].name, PathBuf::from("tone.flac"));
        assert_eq!(&flac[0].bytes[0..4], b"fLaC");

        let back = convert_bytes("tone.flac", &flac[0].bytes, OutputFormat::Wav).unwrap();
        assert_eq!(back[0].name, PathBuf::from("tone.wav"));
        let decoded = parse_wav(&back[0].bytes);
        assert_eq!(decoded.sample_rate, original.sample_rate);
        assert_eq!(decoded.channels, original.channels);
        assert_eq!(decoded.samples.len(), original.samples.len());
        assert_eq!(decoded.samples, original.samples);
        assert_audible(&decoded.samples);
    }
    #[test]
    fn converts_wav_audio_to_aiff_and_au() {
        let wav = wav_fixture();
        let original = parse_wav(&wav);

        let aiff = convert_bytes("tone.wav", &wav, OutputFormat::Aiff).unwrap();
        assert_eq!(aiff[0].name, PathBuf::from("tone.aiff"));
        assert_eq!(&aiff[0].bytes[0..4], b"FORM");
        assert_eq!(&aiff[0].bytes[8..12], b"AIFF");
        let back = convert_bytes("tone.aiff", &aiff[0].bytes, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&back[0].bytes);
        assert_eq!(decoded.sample_rate, original.sample_rate);
        assert_eq!(decoded.channels, original.channels);
        assert_eq!(decoded.samples, original.samples);
        assert_audible(&decoded.samples);

        let au = convert_bytes("tone.wav", &wav, OutputFormat::Au).unwrap();
        assert_eq!(au[0].name, PathBuf::from("tone.au"));
        assert_eq!(&au[0].bytes[0..4], b".snd");
        assert_eq!(
            u32::from_be_bytes(au[0].bytes[4..8].try_into().unwrap()),
            24
        );
        assert_eq!(
            u32::from_be_bytes(au[0].bytes[8..12].try_into().unwrap()) as usize,
            au[0].bytes.len() - 24
        );
        assert_eq!(
            u32::from_be_bytes(au[0].bytes[12..16].try_into().unwrap()),
            3
        );
        assert_eq!(
            u32::from_be_bytes(au[0].bytes[16..20].try_into().unwrap()),
            original.sample_rate
        );
        assert_eq!(
            u32::from_be_bytes(au[0].bytes[20..24].try_into().unwrap()),
            u32::from(original.channels)
        );
        let au_samples: Vec<i16> = au[0].bytes[24..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| i16::from_be_bytes(*bytes))
            .collect();
        assert_eq!(au_samples, original.samples);
    }
    #[test]
    fn converts_wav_audio_to_opus_and_back() {
        let wav = wav_fixture();

        let opus = convert_bytes("tone.wav", &wav, OutputFormat::Opus).unwrap();
        assert_eq!(opus[0].name, PathBuf::from("tone.opus"));
        assert!(opus[0].bytes.starts_with(b"OggS"));
        let back = convert_bytes("tone.opus", &opus[0].bytes, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&back[0].bytes);
        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(decoded.channels, 2);
        assert_eq!(decoded.samples.len(), 96_000);
        assert_audible(&decoded.samples);

        let mono = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Opus,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Mono),
                ..Default::default()
            },
        )
        .unwrap();
        let mono_back = convert_bytes("tone.opus", &mono[0].bytes, OutputFormat::Wav).unwrap();
        let mono_decoded = parse_wav(&mono_back[0].bytes);
        assert_eq!(mono_decoded.sample_rate, 48_000);
        assert_eq!(mono_decoded.channels, 1);
        assert_eq!(mono_decoded.samples.len(), 48_000);
        assert_audible(&mono_decoded.samples);

        let sixteen_k = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Opus,
            ConversionOptions {
                audio_sample_rate: Some(16_000),
                ..Default::default()
            },
        )
        .unwrap();
        let sixteen_k_back =
            convert_bytes("tone.opus", &sixteen_k[0].bytes, OutputFormat::Wav).unwrap();
        let sixteen_k_decoded = parse_wav(&sixteen_k_back[0].bytes);
        assert_eq!(sixteen_k_decoded.sample_rate, 48_000);
        assert_eq!(sixteen_k_decoded.samples.len(), 96_000);
        assert_audible(&sixteen_k_decoded.samples);

        // 44.1 kHz is the app's default sample-rate setting, but Opus encodes
        // at 48 kHz, so this covers the default-rate resampling path.
        let default_rate = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Opus,
            ConversionOptions {
                audio_sample_rate: Some(44_100),
                ..Default::default()
            },
        )
        .unwrap();
        let default_rate_back =
            convert_bytes("tone.opus", &default_rate[0].bytes, OutputFormat::Wav).unwrap();
        let default_rate_decoded = parse_wav(&default_rate_back[0].bytes);
        assert_eq!(default_rate_decoded.sample_rate, 48_000);
        assert_eq!(default_rate_decoded.channels, 2);
        assert_eq!(default_rate_decoded.samples.len(), 96_000);
        assert_audible(&default_rate_decoded.samples);

        // Default options keep the source rate, so this exercises the Opus
        // encoder's own 44.1 kHz -> 48 kHz fallback instead of the shared
        // resampler.
        let wav_44_1 = wav_fixture_at(44_100, 44_100);
        let source_44_1 = parse_wav(&wav_44_1);
        assert_eq!(source_44_1.sample_rate, 44_100);
        assert_eq!(source_44_1.samples.len(), 88_200);
        let fallback = convert_bytes("tone-44.1k.wav", &wav_44_1, OutputFormat::Opus).unwrap();
        let fallback_back =
            convert_bytes("tone.opus", &fallback[0].bytes, OutputFormat::Wav).unwrap();
        let fallback_decoded = parse_wav(&fallback_back[0].bytes);
        assert_eq!(fallback_decoded.sample_rate, 48_000);
        assert_eq!(fallback_decoded.channels, 2);
        assert_eq!(fallback_decoded.samples.len(), 96_000);
        assert_audible(&fallback_decoded.samples);
    }
    #[test]
    fn writes_the_aiff_extended_sample_rate() {
        assert_eq!(
            super::extended_sample_rate(44_100),
            [0x40, 0x0e, 0xac, 0x44, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(super::extended_sample_rate(0), [0; 10]);
    }
    #[test]
    fn applies_audio_sample_rate_and_channel_options() {
        let wav = wav_fixture();

        let upsampled = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Wav,
            ConversionOptions {
                audio_sample_rate: Some(16_000),
                ..Default::default()
            },
        )
        .unwrap();
        let parsed = parse_wav(&upsampled[0].bytes);
        assert_eq!(parsed.sample_rate, 16_000);
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.samples.len(), 32_000);
        assert_audible(&parsed.samples);

        let mono = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Wav,
            ConversionOptions {
                audio_sample_rate: Some(16_000),
                audio_channels: Some(AudioChannels::Mono),
                ..Default::default()
            },
        )
        .unwrap();
        let parsed = parse_wav(&mono[0].bytes);
        assert_eq!(parsed.sample_rate, 16_000);
        assert_eq!(parsed.channels, 1);
        assert_eq!(parsed.samples.len(), 16_000);
        assert_audible(&parsed.samples);

        let stereo = convert_bytes_with_options(
            "tone.wav",
            &mono[0].bytes,
            OutputFormat::Wav,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        )
        .unwrap();
        let parsed = parse_wav(&stereo[0].bytes);
        assert_eq!(parsed.sample_rate, 16_000);
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.samples.len(), 32_000);
        assert_audible(&parsed.samples);

        let flac = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Flac,
            ConversionOptions {
                audio_sample_rate: Some(16_000),
                audio_channels: Some(AudioChannels::Mono),
                ..Default::default()
            },
        )
        .unwrap();
        let flac_wav = convert_bytes("tone.flac", &flac[0].bytes, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&flac_wav[0].bytes);
        assert_eq!(parsed.sample_rate, 16_000);
        assert_eq!(parsed.channels, 1);
        assert_eq!(parsed.samples.len(), 16_000);
        assert_audible(&parsed.samples);

        let clamped = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Flac,
            ConversionOptions {
                audio_sample_rate: Some(384_000),
                ..Default::default()
            },
        )
        .unwrap();
        let clamped_wav = convert_bytes("tone.flac", &clamped[0].bytes, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&clamped_wav[0].bytes);
        assert_eq!(parsed.sample_rate, 96_000);
        assert_audible(&parsed.samples);
    }
    #[test]
    fn converts_real_mp3_audio_to_wav_and_flac() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../test/resources/gaster.mp3");
        let input = std::fs::read(path).unwrap();

        let wav = convert_bytes("gaster.mp3", &input, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&wav[0].bytes);
        assert!(parsed.sample_rate >= 8_000);
        assert!(parsed.channels >= 1);
        assert!(parsed.samples.len() > 1_000);
        assert_audible(&parsed.samples);

        let flac = convert_bytes("gaster.mp3", &input, OutputFormat::Flac).unwrap();
        assert_eq!(&flac[0].bytes[0..4], b"fLaC");

        let flac_wav = convert_bytes("gaster.flac", &flac[0].bytes, OutputFormat::Wav).unwrap();
        let flac_parsed = parse_wav(&flac_wav[0].bytes);
        assert_eq!(flac_parsed.sample_rate, parsed.sample_rate);
        assert_eq!(flac_parsed.channels, parsed.channels);
        assert_eq!(flac_parsed.samples.len(), parsed.samples.len());
        let max_difference = flac_parsed
            .samples
            .iter()
            .zip(&parsed.samples)
            .map(|(flac_sample, wav_sample)| {
                (i32::from(*flac_sample) - i32::from(*wav_sample)).unsigned_abs()
            })
            .max()
            .unwrap();
        assert!(
            max_difference <= 1,
            "the FLAC round-trip changed a sample by {max_difference}"
        );
        assert_audible(&flac_parsed.samples);
    }
    #[test]
    fn converts_real_ogg_audio_to_wav() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/piano.ogg");
        let input = std::fs::read(path).unwrap();
        assert_eq!(&input[0..4], b"OggS");

        let wav = convert_bytes("piano.ogg", &input, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&wav[0].bytes);
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.channels, 1);
        assert!(
            parsed.samples.len() >= 384_854,
            "decoded only {} samples",
            parsed.samples.len()
        );
        assert_audible(&parsed.samples);
    }
    #[test]
    fn converts_real_m4a_audio_to_wav() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sample.m4a");
        let input = std::fs::read(path).unwrap();

        let wav = convert_bytes("sample.m4a", &input, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&wav[0].bytes);
        assert_eq!(parsed.sample_rate, 44_100);
        assert_eq!(parsed.channels, 1);
        assert!(
            parsed.samples.len() >= 100_000,
            "decoded only {} samples",
            parsed.samples.len()
        );
        assert_audible(&parsed.samples);
    }
    #[test]
    fn extracts_audio_from_a_video_container() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sample-video.mp4");
        let input = std::fs::read(path).unwrap();

        let tracks = super::audio_tracks(&input, "mp4").unwrap();
        assert_eq!(tracks.len(), 1, "the video track must not be listed");
        assert_eq!(tracks[0].codec, "AAC");
        assert_eq!(tracks[0].channels, 1);

        let wav = convert_bytes("sample-video.mp4", &input, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&wav[0].bytes);
        assert_eq!(parsed.sample_rate, 44_100);
        assert_eq!(parsed.channels, 1);
        assert!(
            parsed.samples.len() >= 100_000,
            "decoded only {} samples",
            parsed.samples.len()
        );
        assert_audible(&parsed.samples);
    }
    #[test]
    fn extracts_audio_from_a_video_with_opus() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sample-opus.webm");
        let input = std::fs::read(path).unwrap();

        let wav = convert_bytes("sample-opus.webm", &input, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&wav[0].bytes);
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.channels, 2);
        assert!(
            parsed.samples.len() >= 200_000,
            "decoded only {} samples",
            parsed.samples.len()
        );
        assert_audible(&parsed.samples);
    }
    #[test]
    fn extracts_audio_from_an_mp4_with_opus() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sample-opus.mp4");
        let input = std::fs::read(path).unwrap();

        let wav = convert_bytes("sample-opus.mp4", &input, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&wav[0].bytes);
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.channels, 2);
        assert!(
            parsed.samples.len() >= 200_000,
            "decoded only {} samples",
            parsed.samples.len()
        );
        assert_audible(&parsed.samples);
    }
    #[test]
    fn extracts_audio_from_vp9_opus_webm_mp4_webm_round_trip() {
        for (name, expected) in [
            // The source WebM carries 13.5 ms of DiscardPadding after the
            // 2 s tone, so the precise trim is 96,000 samples.
            ("sample-vp9-opus.webm", 96_000),
            // The MP4 edit list starts 336 samples into the media and presents
            // 2,001 ms. The Opus header's 312-sample pre-skip is already
            // removed by the decoder, leaving a 24-sample additional skip.
            ("sample-vp9-opus.mp4", 96_048),
            // The remux dropped DiscardPadding. The final Opus block starts at
            // 2,001 ms, which is the per-track end bound, so the final packet's
            // padding must not be emitted.
            ("sample-vp9-opus-roundtrip.webm", 96_048),
        ] {
            let path = fixture_path(name);
            let input = std::fs::read(&path).unwrap();

            let wav = convert_bytes(name, &input, OutputFormat::Wav).unwrap();
            let parsed = parse_wav(&wav[0].bytes);
            assert_eq!(parsed.sample_rate, 48_000, "{name}");
            assert_eq!(parsed.channels, 1, "{name}");
            assert_eq!(parsed.samples.len(), expected, "{name} buffered");
            assert_audible(&parsed.samples);

            let streamed = parse_wav(&stream_wav(&path, ConversionOptions::default()));
            assert_eq!(streamed.sample_rate, 48_000, "{name}");
            assert_eq!(streamed.channels, 1, "{name}");
            assert_eq!(streamed.samples.len(), expected, "{name} streaming");
            assert_eq!(streamed.samples, parsed.samples, "{name} differs");
        }

        let source = fixture("sample-vp9-opus.webm");
        let source_timing = super::matroska_audio_timing(&source, 2);
        assert_eq!(source_timing.discard_padding_ns, Some(13_500_000));
        assert_eq!(source_timing.last_block_timestamp_ns, Some(2_001_000_000));

        let remux = fixture("sample-vp9-opus-roundtrip.webm");
        let remux_timing = super::matroska_audio_timing(&remux, 2);
        assert_eq!(remux_timing.discard_padding_ns, None);
        assert_eq!(remux_timing.last_block_timestamp_ns, Some(2_001_000_000));
    }
    #[test]
    fn converts_vp9_opus_webm_directly_to_opus_and_back() {
        for (name, expected) in [
            // The source WebM carries the exact DiscardPadding trim.
            ("sample-vp9-opus.webm", 96_000),
            // The remuxed WebM has no DiscardPadding, so its final block
            // timestamp supplies the 2,001 ms presentation length.
            ("sample-vp9-opus-roundtrip.webm", 96_048),
        ] {
            let input = fixture(name);
            let opus = convert_bytes(name, &input, OutputFormat::Opus).unwrap();
            assert_eq!(
                opus[0].name,
                PathBuf::from(name).with_extension("opus"),
                "{name}"
            );
            assert!(opus[0].bytes.starts_with(b"OggS"), "{name}");

            let back = convert_bytes("tone.opus", &opus[0].bytes, OutputFormat::Wav).unwrap();
            let decoded = parse_wav(&back[0].bytes);
            assert_eq!(decoded.sample_rate, 48_000, "{name}");
            assert_eq!(decoded.channels, 1, "{name}");
            // The end trim survives the decode and re-encode; no final-packet
            // padding may reappear on the round trip.
            assert_eq!(decoded.samples.len(), expected, "{name}");
            assert_audible(&decoded.samples);
        }
    }
    #[test]
    fn trims_discard_padding_for_the_selected_audio_track() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/sample-two-opus-tracks.webm");
        let input = std::fs::read(path).unwrap();

        // The tracks hold 1 s and 1.01 s of audio and carry different end
        // padding, so using the wrong track's value changes the decoded length.
        assert_eq!(super::matroska_discard_padding(&input, 1), Some(13_500_000));
        assert_eq!(super::matroska_discard_padding(&input, 2), Some(3_500_000));

        let wav = convert_bytes("sample-two-opus-tracks.webm", &input, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&wav[0].bytes);
        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(decoded.channels, 1);
        assert_eq!(decoded.samples.len(), 48_000);
        assert_audible(&decoded.samples);
    }
    #[test]
    fn decodes_ogg_opus_audio() {
        use opus_pure::{Application, OggOpusWriter, OpusEncoder, OpusHead, MAX_PACKET_BYTES};

        let sample_rate = 48_000;
        let frame_size = 960;
        let mut encoder = OpusEncoder::new(sample_rate, 1, Application::Audio).unwrap();
        let head = OpusHead::for_encoder(&encoder, sample_rate as u32);
        let mut writer = OggOpusWriter::new(Vec::new(), head).unwrap();
        let mut packet = vec![0_u8; MAX_PACKET_BYTES];
        for frame in 0..50 {
            let pcm: Vec<f32> = (0..frame_size)
                .map(|index| {
                    let time = (frame * frame_size + index) as f32 / sample_rate as f32;
                    (time * 440.0 * std::f32::consts::TAU).sin() * 0.5
                })
                .collect();
            let len = encoder.encode(&pcm, frame_size, &mut packet).unwrap();
            writer.write_packet(&packet[..len]).unwrap();
        }
        let bytes = writer.finish().unwrap();

        let wav = convert_bytes("tone.opus", &bytes, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&wav[0].bytes);
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.channels, 1);
        assert!(
            parsed.samples.len() >= 40_000,
            "decoded only {} samples",
            parsed.samples.len()
        );
        assert_audible(&parsed.samples);
    }
    #[test]
    fn decodes_flac_with_an_id3_tag() {
        let wav = wav_fixture();
        let flac = convert_bytes("tone.wav", &wav, OutputFormat::Flac).unwrap();

        let mut tagged = b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec();
        tagged.extend_from_slice(&flac[0].bytes);
        let back = convert_bytes("tone.flac", &tagged, OutputFormat::Wav).unwrap();
        assert_eq!(&back[0].bytes[0..4], b"RIFF");
    }
    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(fixture_path(name)).unwrap()
    }
    #[test]
    fn writes_wav_directly_to_a_writer() {
        let input = wav_fixture();
        let mut output = Vec::new();
        convert_audio_to_wav_writer(
            "tone.wav",
            &input,
            ConversionOptions::default(),
            &mut output,
        )
        .unwrap();
        let parsed = parse_wav(&output);
        assert_eq!(parsed.bits_per_sample, 16);
        assert_eq!(parsed.samples.len(), 16_000);
        assert_audible(&parsed.samples);
        assert_eq!(
            wav_output_name("tone.wav"),
            std::path::PathBuf::from("tone.wav")
        );
    }
    #[test]
    fn lists_tracks_from_a_file_without_loading_it() {
        let tracks =
            super::audio_tracks_from_path(&fixture_path("sample-two-opus-tracks.webm")).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].codec, "Opus");
        assert_eq!(tracks[1].id, 2);

        let tracks = super::audio_tracks_from_path(&fixture_path("sample-video.mp4")).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].codec, "AAC");
        assert_eq!(tracks[0].channels, 1);

        let tracks = super::audio_tracks_from_path(&fixture_path("sample-ac3.mkv")).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].codec, "AC-3");
        assert_eq!(tracks[0].channels, 6);
    }
    #[test]
    fn lists_tracks_from_a_padded_mp4() {
        let directory = stream_temp_dir("padded-probe");
        let input = directory.join("padded.mp4");
        let mut padded = std::fs::read(fixture_path("sample-ac3.mp4")).unwrap();
        padded.resize(padded.len() + 16 * 1024 * 1024, 0);
        std::fs::write(&input, padded).unwrap();

        let tracks = super::audio_tracks_from_path(&input).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].codec, "AC-3");
        assert_eq!(tracks[0].channels, 6);
        assert_eq!(tracks[0].sample_rate, 48_000);

        std::fs::remove_dir_all(&directory).unwrap();
    }
    #[test]
    fn lists_and_selects_audio_tracks() {
        let input = fixture("sample-two-opus-tracks.webm");
        let tracks = super::audio_tracks(&input, "webm").unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].id, 1);
        assert_eq!(tracks[1].id, 2);
        assert_eq!(tracks[0].codec, "Opus");
        assert_eq!(tracks[0].channels, 1);
        assert_eq!(tracks[0].sample_rate, 48_000);
        assert!(tracks[0].default);
        assert!(!tracks[1].default);
        assert!(tracks[1].label().starts_with("#2"));

        let first =
            convert_bytes("sample-two-opus-tracks.webm", &input, OutputFormat::Wav).unwrap();
        assert_eq!(parse_wav(&first[0].bytes).samples.len(), 48_000);

        let second = convert_bytes_with_options(
            "sample-two-opus-tracks.webm",
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let second = parse_wav(&second[0].bytes);
        assert_eq!(second.samples.len(), 48_480);
        assert_audible(&second.samples);

        let error = convert_bytes_with_options(
            "sample-two-opus-tracks.webm",
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(2),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::AudioTrackNotFound {
                requested: 3,
                available: 2
            }
        ));
    }
    #[test]
    fn path_and_memory_track_lists_agree() {
        for (name, extension) in [
            ("sample-two-opus-tracks.webm", "webm"),
            ("sample-interleaved-tracks.mkv", "mkv"),
            ("sample-video.mp4", "mp4"),
        ] {
            let input = fixture(name);
            let path = fixture_path(name);
            assert_eq!(
                super::audio_tracks(&input, extension).unwrap(),
                super::audio_tracks_from_path(&path).unwrap(),
                "{name}"
            );
        }
    }
    #[test]
    fn default_track_matches_explicit_first_in_buffered_and_streaming_paths() {
        let name = "sample-two-opus-tracks.webm";
        let input = fixture(name);
        let path = fixture_path(name);

        let default = convert_bytes(name, &input, OutputFormat::Wav).unwrap();
        let first = convert_bytes_with_options(
            name,
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        let second = convert_bytes_with_options(
            name,
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(default[0].bytes, first[0].bytes);
        assert_ne!(default[0].bytes, second[0].bytes);
        assert_eq!(parse_wav(&default[0].bytes).samples.len(), 48_000);
        assert_eq!(parse_wav(&second[0].bytes).samples.len(), 48_480);

        let default_stream = stream_wav(&path, ConversionOptions::default());
        let first_stream = stream_wav(
            &path,
            ConversionOptions {
                audio_track: Some(0),
                ..Default::default()
            },
        );
        let second_stream = stream_wav(
            &path,
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        );
        assert_eq!(default_stream, first_stream);
        assert_ne!(default_stream, second_stream);
        assert_eq!(parse_wav(&default_stream).samples.len(), 48_000);
        assert_eq!(parse_wav(&second_stream).samples.len(), 48_480);
    }
    #[test]
    fn selected_audio_track_changes_the_decoded_tone() {
        let name = "sample-two-opus-tracks.webm";
        let input = fixture(name);
        let path = fixture_path(name);

        let default = parse_wav(&convert_bytes(name, &input, OutputFormat::Wav).unwrap()[0].bytes);
        let first = parse_wav(
            &convert_bytes_with_options(
                name,
                &input,
                OutputFormat::Wav,
                ConversionOptions {
                    audio_track: Some(0),
                    ..Default::default()
                },
            )
            .unwrap()[0]
                .bytes,
        );
        let second = parse_wav(
            &convert_bytes_with_options(
                name,
                &input,
                OutputFormat::Wav,
                ConversionOptions {
                    audio_track: Some(1),
                    ..Default::default()
                },
            )
            .unwrap()[0]
                .bytes,
        );

        for parsed in [&default, &first, &second] {
            assert_eq!(parsed.channels, 1);
            assert_eq!(parsed.sample_rate, 48_000);
        }
        assert_eq!(default.samples.len(), 48_000);
        assert_eq!(first.samples.len(), 48_000);
        assert_eq!(second.samples.len(), 48_480);
        assert_eq!(default.samples, first.samples);
        assert_ne!(first.samples, second.samples);
        assert!((decoded_tone_frequency(&default) - 440.0).abs() < 5.0);
        assert!((decoded_tone_frequency(&first) - 440.0).abs() < 5.0);
        assert!((decoded_tone_frequency(&second) - 880.0).abs() < 5.0);
        for parsed in [&default, &first, &second] {
            let rms = decoded_rms(parsed);
            assert!(
                (1_000.0..5_000.0).contains(&rms),
                "decoded tone RMS is {rms:.1}"
            );
            assert_audible(&parsed.samples);
        }

        let default_stream = parse_wav(&stream_wav(&path, ConversionOptions::default()));
        let second_stream = parse_wav(&stream_wav(
            &path,
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        ));
        assert!((decoded_tone_frequency(&default_stream) - 440.0).abs() < 5.0);
        assert!((decoded_tone_frequency(&second_stream) - 880.0).abs() < 5.0);
        assert_eq!(second_stream.samples.len(), 48_480);

        let mut output = Cursor::new(Vec::new());
        let error = crate::streaming::convert_audio_file_to_writer(
            &path,
            name,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(2),
                ..Default::default()
            },
            &mut output,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::AudioTrackNotFound {
                requested: 3,
                available: 2
            }
        ));
        assert!(
            output.get_ref().is_empty(),
            "an invalid track selection wrote audio before returning an error"
        );
    }
    #[test]
    fn selects_the_audio_track_in_an_mp4_video() {
        let name = "sample-video.mp4";
        let input = fixture(name);
        let tracks = super::audio_tracks(&input, "mp4").unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].codec, "AAC");
        assert_eq!(tracks[0].channels, 1);
        assert_eq!(tracks[0].sample_rate, 44_100);

        let selected = convert_bytes_with_options(
            name,
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        let parsed = parse_wav(&selected[0].bytes);
        assert_eq!(parsed.channels, 1);
        assert_audible(&parsed.samples);

        let error = convert_bytes_with_options(
            name,
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::AudioTrackNotFound {
                requested: 2,
                available: 1
            }
        ));
    }
    #[test]
    fn selected_audio_track_reaches_every_audio_encoder() {
        let name = "sample-two-opus-tracks.webm";
        let input = fixture(name);
        for target in [
            OutputFormat::Wav,
            OutputFormat::Flac,
            OutputFormat::Aiff,
            OutputFormat::Au,
            OutputFormat::Opus,
            OutputFormat::Mp3,
        ] {
            let encoded = convert_bytes_with_options(
                name,
                &input,
                target,
                ConversionOptions {
                    audio_track: Some(1),
                    ..Default::default()
                },
            )
            .unwrap();
            let parsed = if target == OutputFormat::Au {
                // AU is an output-only format here, so inspect its PCM payload
                // directly instead of asking the decoder to read it back.
                assert_eq!(&encoded[0].bytes[0..4], b".snd", "{target:?}");
                let channels =
                    u32::from_be_bytes(encoded[0].bytes[20..24].try_into().unwrap()) as u16;
                let sample_rate = u32::from_be_bytes(encoded[0].bytes[16..20].try_into().unwrap());
                let samples = encoded[0].bytes[24..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|bytes| i16::from_be_bytes(*bytes))
                    .collect();
                ParsedWav {
                    channels,
                    sample_rate,
                    bits_per_sample: 16,
                    samples,
                }
            } else {
                let decoded = convert_bytes(
                    encoded[0].name.to_string_lossy().as_ref(),
                    &encoded[0].bytes,
                    OutputFormat::Wav,
                )
                .unwrap();
                parse_wav(&decoded[0].bytes)
            };
            assert_eq!(parsed.channels, 1, "{target:?}");
            assert_eq!(parsed.sample_rate, 48_000, "{target:?}");
            assert!(
                (decoded_tone_frequency(&parsed) - 880.0).abs() < 20.0,
                "{target:?} did not encode the second track: {} Hz",
                decoded_tone_frequency(&parsed)
            );
            assert_audible(&parsed.samples);
        }
    }
    #[test]
    fn selects_audio_tracks_after_non_audio_and_unsupported_streams() {
        let name = "sample-interleaved-tracks.mkv";
        let path = fixture_path(name);
        let input = fixture(name);

        let tracks = super::audio_tracks_from_path(&path).unwrap();
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].index, 0);
        assert_eq!(tracks[0].id, 3);
        assert_eq!(tracks[0].language.as_deref(), Some("eng"));
        assert_eq!(tracks[0].codec, "AC-3");
        assert_eq!(tracks[0].channels, 6);
        assert_eq!(tracks[0].sample_rate, 48_000);
        assert!(tracks[0].label().starts_with("#1 · stream 3 · eng · AC-3"));
        assert_eq!(tracks[1].index, 1);
        assert_eq!(tracks[1].id, 4);
        assert_eq!(tracks[1].language.as_deref(), Some("fra"));
        assert_eq!(tracks[1].codec, "AC-3");
        assert_eq!(tracks[1].channels, 6);
        assert_eq!(tracks[1].sample_rate, 48_000);
        assert!(tracks[1].label().starts_with("#2 · stream 4 · fra · AC-3"));

        let default = convert_bytes(name, &input, OutputFormat::Wav).unwrap();
        let first = convert_bytes_with_options(
            name,
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        let second = convert_bytes_with_options(
            name,
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(default[0].bytes, first[0].bytes);
        assert_ne!(default[0].bytes, second[0].bytes);
        let first_wav = parse_wav(&default[0].bytes);
        let second_wav = parse_wav(&second[0].bytes);
        assert_eq!(first_wav.channels, 6);
        assert_eq!(first_wav.sample_rate, 48_000);
        assert_eq!(first_wav.samples.len(), 96_768 * 6);
        assert_eq!(second_wav.channels, 6);
        assert_eq!(second_wav.sample_rate, 48_000);
        assert_eq!(second_wav.samples.len(), 153_600 * 6);

        let default_stream = stream_wav(&path, ConversionOptions::default());
        let first_stream = stream_wav(
            &path,
            ConversionOptions {
                audio_track: Some(0),
                ..Default::default()
            },
        );
        let second_stream = stream_wav(
            &path,
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        );
        assert_eq!(default_stream, default[0].bytes);
        assert_eq!(first_stream, first[0].bytes);
        assert_eq!(second_stream, second[0].bytes);
        assert_eq!(parse_wav(&second_stream).samples.len(), 153_600 * 6);

        let error = convert_bytes_with_options(
            name,
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(2),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::AudioTrackNotFound {
                requested: 3,
                available: 2
            }
        ));

        let mut output = Cursor::new(Vec::new());
        let error = crate::streaming::convert_audio_file_to_writer(
            &path,
            name,
            OutputFormat::Wav,
            ConversionOptions {
                audio_track: Some(2),
                ..Default::default()
            },
            &mut output,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::AudioTrackNotFound {
                requested: 3,
                available: 2
            }
        ));
    }
    #[test]
    fn decodes_ac3_and_eac3_audio() {
        for (name, codec) in [
            ("sample-ac3.ac3", "AC-3"),
            ("sample-ac3.mkv", "AC-3"),
            ("sample-ac3.mp4", "AC-3"),
            ("sample-eac3.mkv", "E-AC-3"),
        ] {
            let input = fixture(name);
            let extension = std::path::Path::new(name)
                .extension()
                .unwrap()
                .to_str()
                .unwrap();
            let tracks = super::audio_tracks(&input, extension).unwrap();
            assert_eq!(tracks.len(), 1, "{name}");
            assert_eq!(tracks[0].codec, codec, "{name}");
            assert_eq!(tracks[0].channels, 6, "{name}");
            assert_eq!(tracks[0].sample_rate, 48_000, "{name}");

            let wav = convert_bytes(name, &input, OutputFormat::Wav).unwrap();
            let parsed = parse_wav(&wav[0].bytes);
            assert_eq!(parsed.sample_rate, 48_000, "{name}");
            assert_eq!(parsed.channels, 6, "{name}");
            let expected_frames = if name == "sample-ac3.mp4" {
                // The MP4 edit list skips 256 samples and presents 2 s.
                96_000
            } else {
                // The elementary stream and Matroska fixture have no edit list.
                96_768
            };
            assert_eq!(parsed.samples.len(), expected_frames * 6, "{name}");
            assert_audible(&parsed.samples);

            let stereo = convert_bytes_with_options(
                name,
                &input,
                OutputFormat::Wav,
                ConversionOptions {
                    audio_channels: Some(AudioChannels::Stereo),
                    ..Default::default()
                },
            )
            .unwrap();
            let stereo = parse_wav(&stereo[0].bytes);
            assert_eq!(stereo.channels, 2, "{name}");
            assert_eq!(stereo.samples.len(), expected_frames * 2, "{name}");
            assert_audible(&stereo.samples);
        }
    }
    #[test]
    fn downmixes_centre_and_surround_channels() {
        // Only the centre channel carries audio. Keeping the first two channels
        // would produce silence, which is the bug this test guards against.
        let input = fixture("sample-ac3-center.ac3");
        let decoded = convert_bytes("sample-ac3-center.ac3", &input, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&decoded[0].bytes);
        assert_eq!(decoded.channels, 6);
        assert_eq!(decoded.sample_rate, 48_000);
        let centre_peak = decoded
            .samples
            .iter()
            .skip(2)
            .step_by(6)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(centre_peak > 1_000, "the AC-3 centre channel is silent");

        let stereo = convert_bytes_with_options(
            "sample-ac3-center.ac3",
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        )
        .unwrap();
        let stereo = parse_wav(&stereo[0].bytes);
        assert_eq!(stereo.channels, 2);
        assert_eq!(stereo.sample_rate, 48_000);
        assert_eq!(stereo.samples.len(), decoded.samples.len() / 3);
        let left_peak = stereo
            .samples
            .iter()
            .step_by(2)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        let right_peak = stereo
            .samples
            .iter()
            .skip(1)
            .step_by(2)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(left_peak > 1_000, "the centre tone was lost from the left");
        assert!(
            right_peak > 1_000,
            "the centre tone was lost from the right"
        );
        assert!(
            left_peak.abs_diff(right_peak) < left_peak / 20,
            "the centre tone was not balanced: {left_peak} vs {right_peak}"
        );
        let ratio = f32::from(left_peak) / f32::from(centre_peak);
        assert!(
            (ratio - 0.707).abs() < 0.02,
            "the centre channel was downmixed at {ratio}, expected 0.707"
        );

        let mono = convert_bytes_with_options(
            "sample-ac3-center.ac3",
            &input,
            OutputFormat::Wav,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Mono),
                ..Default::default()
            },
        )
        .unwrap();
        let mono = parse_wav(&mono[0].bytes);
        assert_eq!(mono.channels, 1);
        assert_eq!(mono.sample_rate, 48_000);
        assert_audible(&mono.samples);
    }
    #[test]
    fn surround_downmix_uses_standard_coefficients_and_avoids_clipping() {
        let centre_only = super::DecodedAudio {
            samples: vec![0.0, 0.0, 0.5, 0.0, 0.0, 0.0],
            channels: 6,
            sample_rate: 48_000,
            source_bits: None,
        };
        let stereo = super::remix_channels(centre_only.clone(), AudioChannels::Stereo);
        assert!((stereo.samples[0] - 0.353_553).abs() < 0.001);
        assert!((stereo.samples[1] - 0.353_553).abs() < 0.001);
        let mono = super::remix_channels(centre_only, AudioChannels::Mono);
        assert!((mono.samples[0] - 0.353_553).abs() < 0.001);

        let loud = super::DecodedAudio {
            samples: vec![1.0; 6 * 100],
            channels: 6,
            sample_rate: 48_000,
            source_bits: None,
        };
        let stereo = super::remix_channels(loud, AudioChannels::Stereo);
        let peak = stereo
            .samples
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        assert!(peak <= 1.0, "the downmix clipped at {peak}");
        assert!(peak > 0.9, "the downmix was attenuated more than needed");
    }
    #[test]
    fn downmixes_51_wav_with_expected_centre_and_surround_gains() {
        // Values are constant so the output is exactly the downmix coefficient
        // applied to each channel. Channel order is L, R, C, LFE, BL, BR.
        let cases: [(&str, [f32; 6], f32, f32); 7] = [
            ("left only", [0.5, 0.0, 0.0, 0.0, 0.0, 0.0], 0.5, 0.0),
            ("right only", [0.0, 0.5, 0.0, 0.0, 0.0, 0.0], 0.0, 0.5),
            (
                "centre only",
                [0.0, 0.0, 0.5, 0.0, 0.0, 0.0],
                0.353_553,
                0.353_553,
            ),
            (
                "back left only",
                [0.0, 0.0, 0.0, 0.0, 0.5, 0.0],
                0.353_553,
                0.0,
            ),
            (
                "back right only",
                [0.0, 0.0, 0.0, 0.0, 0.0, 0.5],
                0.0,
                0.353_553,
            ),
            ("LFE only", [0.0, 0.0, 0.0, 0.5, 0.0, 0.0], 0.25, 0.25),
            (
                "stereo passthrough",
                [0.25, 0.5, 0.0, 0.0, 0.0, 0.0],
                0.25,
                0.5,
            ),
        ];

        for (name, channels, expected_left, expected_right) in cases {
            let input = wav_channels_at(48_000, 128, &channels);
            let converted = convert_bytes_with_options(
                "surround.wav",
                &input,
                OutputFormat::Wav,
                ConversionOptions {
                    audio_channels: Some(AudioChannels::Stereo),
                    ..Default::default()
                },
            )
            .unwrap();
            let parsed = parse_wav(&converted[0].bytes);
            assert_eq!(parsed.channels, 2, "{name}");
            assert_eq!(parsed.sample_rate, 48_000, "{name}");
            assert_eq!(parsed.samples.len(), 128 * 2, "{name}");
            let left = f32::from(parsed.samples[0]) / i16::MAX as f32;
            let right = f32::from(parsed.samples[1]) / i16::MAX as f32;
            assert!(
                (left - expected_left).abs() < 0.002,
                "{name}: left was {left}, expected {expected_left}"
            );
            assert!(
                (right - expected_right).abs() < 0.002,
                "{name}: right was {right}, expected {expected_right}"
            );
        }
    }
    #[test]
    fn multichannel_audio_converts_to_every_audio_target() {
        let input = wav_channels_at(48_000, 48_000, &[0.0, 0.0, 0.5, 0.0, 0.0, 0.0]);
        for target in [
            OutputFormat::Wav,
            OutputFormat::Flac,
            OutputFormat::Aiff,
            OutputFormat::Au,
            OutputFormat::Opus,
            OutputFormat::Mp3,
        ] {
            let converted = convert_bytes_with_options(
                "surround.wav",
                &input,
                target,
                ConversionOptions::default(),
            )
            .unwrap();
            let bytes = &converted[0].bytes;
            assert!(!bytes.is_empty(), "{target:?} produced no output");

            match target {
                OutputFormat::Wav => {
                    let parsed = parse_wav(bytes);
                    assert_eq!(parsed.channels, 6, "{target:?}");
                    assert_eq!(parsed.sample_rate, 48_000, "{target:?}");
                    assert_eq!(parsed.samples.len(), 48_000 * 6, "{target:?}");
                    assert_audible(&parsed.samples);
                }
                OutputFormat::Flac => {
                    let wav = convert_bytes("surround.flac", bytes, OutputFormat::Wav).unwrap();
                    let parsed = parse_wav(&wav[0].bytes);
                    assert_eq!(parsed.channels, 6, "{target:?}");
                    assert_eq!(parsed.sample_rate, 48_000, "{target:?}");
                    assert_eq!(parsed.samples.len(), 48_000 * 6, "{target:?}");
                    assert_audible(&parsed.samples);
                }
                OutputFormat::Aiff => {
                    assert_eq!(&bytes[..4], b"FORM", "{target:?}");
                    assert_eq!(u16::from_be_bytes(bytes[20..22].try_into().unwrap()), 6);
                    assert_eq!(
                        u32::from_be_bytes(bytes[22..26].try_into().unwrap()),
                        48_000
                    );
                    assert_eq!(
                        u32::from_be_bytes(bytes[42..46].try_into().unwrap()),
                        48_000 * 6 * 2 + 8
                    );
                }
                OutputFormat::Au => {
                    assert_eq!(&bytes[..4], b".snd", "{target:?}");
                    assert_eq!(
                        u32::from_be_bytes(bytes[8..12].try_into().unwrap()),
                        48_000 * 6 * 2
                    );
                    assert_eq!(u32::from_be_bytes(bytes[20..24].try_into().unwrap()), 6);
                }
                OutputFormat::Opus => {
                    let wav = convert_bytes("surround.opus", bytes, OutputFormat::Wav).unwrap();
                    let parsed = parse_wav(&wav[0].bytes);
                    assert_eq!(parsed.channels, 6, "{target:?}");
                    assert_eq!(parsed.sample_rate, 48_000, "{target:?}");
                    assert!(
                        parsed.samples.len() / 6 >= 48_000,
                        "{target:?} was truncated"
                    );
                    assert_audible(&parsed.samples);
                }
                OutputFormat::Mp3 => {
                    let wav = convert_bytes("surround.mp3", bytes, OutputFormat::Wav).unwrap();
                    let parsed = parse_wav(&wav[0].bytes);
                    assert_eq!(parsed.channels, 2, "{target:?}");
                    assert_eq!(parsed.sample_rate, 48_000, "{target:?}");
                    assert!(
                        parsed.samples.len() / 2 >= 47_000,
                        "{target:?} was truncated"
                    );
                    assert_audible(&parsed.samples);
                    let left_peak = parsed
                        .samples
                        .iter()
                        .step_by(2)
                        .map(|sample| sample.unsigned_abs())
                        .max()
                        .unwrap();
                    let right_peak = parsed
                        .samples
                        .iter()
                        .skip(1)
                        .step_by(2)
                        .map(|sample| sample.unsigned_abs())
                        .max()
                        .unwrap();
                    assert!(left_peak > 1_000, "the centre tone was lost from the left");
                    assert!(
                        right_peak > 1_000,
                        "the centre tone was lost from the right"
                    );
                }
                _ => unreachable!("unexpected audio target {target:?}"),
            }
        }
    }
    #[test]
    fn downmix_layouts_use_explicit_coefficients() {
        // Each case has one non-zero channel, so the result is exactly the
        // coefficient for that layout rather than an accidental slice.
        // Expected values: stereo left/right and mono.
        let cases: &[(&str, &[f32], f32, f32, f32)] = &[
            ("mono up-mix", &[0.5], 0.5, 0.5, 0.5),
            ("stereo", &[0.5, 0.25], 0.5, 0.25, 0.375),
            ("3.0 centre", &[0.0, 0.0, 0.5], 0.3535, 0.3535, 0.3535),
            ("quad back left", &[0.0, 0.0, 0.5, 0.0], 0.3535, 0.0, 0.25),
            ("quad back right", &[0.0, 0.0, 0.0, 0.5], 0.0, 0.3535, 0.25),
            (
                "5.0 back left",
                &[0.0, 0.0, 0.0, 0.5, 0.0],
                0.3535,
                0.0,
                0.25,
            ),
            ("5.1 LFE", &[0.0, 0.0, 0.0, 0.5, 0.0, 0.0], 0.25, 0.25, 0.25),
            (
                "6.1 back centre",
                &[0.0, 0.0, 0.0, 0.0, 0.5, 0.0, 0.0],
                0.25,
                0.25,
                0.25,
            ),
            (
                "6.1 side left",
                &[0.0, 0.0, 0.0, 0.0, 0.0, 0.5, 0.0],
                0.3535,
                0.0,
                0.25,
            ),
            (
                "7.1 back left",
                &[0.0, 0.0, 0.0, 0.0, 0.5, 0.0, 0.0, 0.0],
                0.3535,
                0.0,
                0.25,
            ),
            (
                "7.1 side left",
                &[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.5, 0.0],
                0.3535,
                0.0,
                0.25,
            ),
            (
                "7.1 side right",
                &[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.5],
                0.0,
                0.3535,
                0.25,
            ),
            (
                "extra channel above 7.1",
                &[0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.5],
                0.25,
                0.25,
                0.25,
            ),
        ];

        for (name, frame, expected_left, expected_right, expected_mono) in cases {
            let audio = super::DecodedAudio {
                samples: frame.to_vec(),
                channels: frame.len(),
                sample_rate: 48_000,
                source_bits: None,
            };
            let stereo = super::remix_channels(audio.clone(), AudioChannels::Stereo);
            assert_eq!(stereo.channels, 2, "{name}");
            assert_eq!(stereo.samples.len(), 2, "{name}");
            assert!(
                (stereo.samples[0] - expected_left).abs() < 1e-5,
                "{name}: left was {}, expected {expected_left}",
                stereo.samples[0]
            );
            assert!(
                (stereo.samples[1] - expected_right).abs() < 1e-5,
                "{name}: right was {}, expected {expected_right}",
                stereo.samples[1]
            );

            let mono = super::remix_channels(audio, AudioChannels::Mono);
            assert_eq!(mono.channels, 1, "{name}");
            assert_eq!(mono.samples.len(), 1, "{name}");
            assert!(
                (mono.samples[0] - expected_mono).abs() < 1e-5,
                "{name}: mono was {}, expected {expected_mono}",
                mono.samples[0]
            );
        }
    }
    #[test]
    fn full_scale_multichannel_downmix_does_not_wrap_or_clip() {
        for value in [1.0_f32, -1.0_f32] {
            let input = wav_channels_at(48_000, 256, &[value; 6]);
            let converted = convert_bytes_with_options(
                "loud.wav",
                &input,
                OutputFormat::Wav,
                ConversionOptions {
                    audio_channels: Some(AudioChannels::Stereo),
                    ..Default::default()
                },
            )
            .unwrap();
            let parsed = parse_wav(&converted[0].bytes);
            assert_eq!(parsed.channels, 2);
            assert_eq!(parsed.samples.len(), 256 * 2);
            assert!(
                !parsed.samples.contains(&i16::MIN),
                "a full-scale sample wrapped to i16::MIN"
            );
            let peak = parsed
                .samples
                .iter()
                .map(|sample| sample.unsigned_abs())
                .max()
                .unwrap();
            assert!(peak > 30_000, "the downmix was unexpectedly quiet");

            let direct = super::remix_channels(
                super::DecodedAudio {
                    samples: vec![value; 6 * 256],
                    channels: 6,
                    sample_rate: 48_000,
                    source_bits: None,
                },
                AudioChannels::Stereo,
            );
            assert!(
                direct.samples.iter().all(|sample| sample.is_finite()),
                "the downmix produced NaN or infinity"
            );
            let direct_peak = direct
                .samples
                .iter()
                .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
            assert!(
                direct_peak <= 1.0 + 1e-6,
                "the downmix exceeded full scale at {direct_peak}"
            );
            assert!(direct_peak > 0.99, "the downmix was over-attenuated");
        }
    }
    #[test]
    fn hot_downmix_preserves_relative_levels() {
        // The left side is twice the right before the mix (L + BL vs R + BR).
        // Peak scaling must keep that 2:1 balance.
        let frame = [1.0_f32, 0.5, 0.0, 0.0, 0.5, 0.25];
        let samples = frame.iter().copied().cycle().take(6 * 256).collect();
        let direct = super::remix_channels(
            super::DecodedAudio {
                samples,
                channels: 6,
                sample_rate: 48_000,
                source_bits: None,
            },
            AudioChannels::Stereo,
        );
        assert!(direct.samples.iter().all(|sample| sample.is_finite()));
        let left = direct.samples[0];
        let right = direct.samples[1];
        assert!(left <= 1.0 + 1e-6, "the downmix exceeded full scale");
        assert!(left > 0.99, "the downmix was over-attenuated");
        assert!(
            (right / left - 0.5).abs() < 1e-5,
            "peak scaling changed the channel balance: {left} vs {right}"
        );
    }
    #[test]
    fn multichannel_opus_decodes_and_downmixes_centre_and_surround() {
        let centre = opus_surround_fixture([0.0, 0.5, 0.0, 0.0, 0.0, 0.0]);
        let decoded = convert_bytes("centre.opus", &centre, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&decoded[0].bytes);
        assert_eq!(decoded.channels, 6);
        assert!(decoded.samples.len() > 10_000 * 6);
        assert_audible(&decoded.samples);
        let centre_peak = decoded
            .samples
            .iter()
            .skip(2)
            .step_by(6)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(centre_peak > 1_000, "the centre channel is silent");

        let stereo = convert_bytes_with_options(
            "centre.opus",
            &centre,
            OutputFormat::Wav,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        )
        .unwrap();
        let stereo = parse_wav(&stereo[0].bytes);
        assert_eq!(stereo.channels, 2);
        let left_peak = stereo
            .samples
            .iter()
            .step_by(2)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        let right_peak = stereo
            .samples
            .iter()
            .skip(1)
            .step_by(2)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(left_peak > 1_000, "centre audio was lost from the left");
        assert!(right_peak > 1_000, "centre audio was lost from the right");
        assert!(
            left_peak.abs_diff(right_peak) < left_peak / 10,
            "centre audio was not balanced: {left_peak} vs {right_peak}"
        );

        let directory = stream_temp_dir("multichannel-opus");
        let path = directory.join("centre.opus");
        std::fs::write(&path, &centre).unwrap();
        let streamed = stream_wav(
            &path,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        );
        let streamed = parse_wav(&streamed);
        assert_eq!(streamed.channels, 2);
        assert_audible(&streamed.samples);
        std::fs::remove_dir_all(&directory).unwrap();

        let surround = opus_surround_fixture([0.0, 0.0, 0.0, 0.5, 0.0, 0.0]);
        let stereo = convert_bytes_with_options(
            "surround.opus",
            &surround,
            OutputFormat::Wav,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        )
        .unwrap();
        let stereo = parse_wav(&stereo[0].bytes);
        let left_peak = stereo
            .samples
            .iter()
            .step_by(2)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        let right_peak = stereo
            .samples
            .iter()
            .skip(1)
            .step_by(2)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(left_peak > 1_000, "surround audio was lost from the left");
        assert!(
            left_peak > right_peak * 2,
            "a back-left signal leaked into the right channel: {left_peak} vs {right_peak}"
        );
    }
    #[test]
    fn multichannel_opus_output_preserves_51_channels() {
        let input = wav_channels_at(48_000, 960, &[0.0, 0.0, 0.5, 0.0, 0.0, 0.0]);
        let opus = convert_bytes("centre.wav", &input, OutputFormat::Opus).unwrap();
        assert!(opus[0].bytes.starts_with(b"OggS"));
        let decoded = convert_bytes("centre.opus", &opus[0].bytes, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&decoded[0].bytes);
        assert_eq!(decoded.channels, 6);
        assert_audible(&decoded.samples);
        let centre_peak = decoded
            .samples
            .iter()
            .skip(2)
            .step_by(6)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(centre_peak > 1_000, "the centre channel was not preserved");

        let directory = stream_temp_dir("opus-51-stream");
        let path = directory.join("centre.wav");
        std::fs::write(&path, &input).unwrap();
        let mut output = std::io::Cursor::new(Vec::new());
        crate::streaming::convert_audio_file_to_writer(
            &path,
            "centre.wav",
            OutputFormat::Opus,
            ConversionOptions::default(),
            &mut output,
        )
        .unwrap();
        let streamed =
            convert_bytes("centre.opus", &output.into_inner(), OutputFormat::Wav).unwrap();
        let streamed = parse_wav(&streamed[0].bytes);
        assert_eq!(streamed.channels, 6);
        let centre_peak = streamed
            .samples
            .iter()
            .skip(2)
            .step_by(6)
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(
            centre_peak > 1_000,
            "streaming Opus did not preserve the centre channel"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }
    #[test]
    fn multichannel_opus_output_can_be_downmixed_to_stereo() {
        let input = wav_channels_at(48_000, 960, &[0.0, 0.0, 0.5, 0.0, 0.0, 0.0]);
        let opus = convert_bytes_with_options(
            "centre.wav",
            &input,
            OutputFormat::Opus,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(opus[0].bytes.starts_with(b"OggS"));
        let decoded = convert_bytes("centre.opus", &opus[0].bytes, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&decoded[0].bytes);
        assert_eq!(decoded.channels, 2);
        assert_audible(&decoded.samples);
    }
    #[test]
    fn rejects_invalid_opus_layouts_with_a_typed_error() {
        assert!(matches!(
            super::opus_channels(3, 0),
            Err(ConvertError::UnsupportedChannelLayout {
                codec: "Opus",
                channels: 3
            })
        ));
        assert!(matches!(
            super::opus_channels(9, 1),
            Err(ConvertError::UnsupportedChannelLayout {
                codec: "Opus",
                channels: 9
            })
        ));
        assert_eq!(super::opus_channels(6, 1).unwrap(), 6);
    }
    #[test]
    fn multichannel_aac_reports_a_clear_error() {
        let input = fixture("sample-aac-51.m4a");
        let tracks = super::audio_tracks(&input, "m4a").unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].channels, 6);
        let error = convert_bytes("sample-aac-51.m4a", &input, OutputFormat::Wav).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("multichannel AAC"),
            "unexpected error: {message}"
        );
    }
    #[test]
    fn applies_mp4_edit_list_gapless_trimming() {
        for (name, expected, channels, sample_rate) in [
            // 2 s AAC: 1024 samples of encoder priming are removed, then the
            // edit list's 88,200-sample presentation length is applied.
            ("sample-aac.m4a", 88_200, 1, 44_100),
            // No edit list here. The audio track's mdhd duration is 135,168
            // samples; the movie duration is only 3,036 ms and would be wrong.
            ("sample.m4a", 135_168, 1, 44_100),
            // The video track's movie timing must not determine the audio
            // track's length. This track is also 135,168 samples.
            ("sample-video.mp4", 135_168, 1, 44_100),
            // MP4 Opus: edit list media_time 336, duration 2,001 ms. The
            // decoder already removed the 312-sample Opus pre-skip.
            ("sample-vp9-opus.mp4", 96_048, 1, 48_000),
            // MP4 AC-3: edit list skips 256 samples and presents 2 s.
            ("sample-ac3.mp4", 96_000, 6, 48_000),
        ] {
            let path = fixture_path(name);
            let input = std::fs::read(&path).unwrap();

            let wav = convert_bytes(name, &input, OutputFormat::Wav).unwrap();
            let parsed = parse_wav(&wav[0].bytes);
            assert_eq!(parsed.sample_rate, sample_rate, "{name}");
            assert_eq!(parsed.channels, channels, "{name}");
            assert_eq!(
                parsed.samples.len(),
                expected * usize::from(channels),
                "{name} buffered"
            );
            assert_audible(&parsed.samples);

            let streamed = parse_wav(&stream_wav(&path, ConversionOptions::default()));
            assert_eq!(streamed.sample_rate, sample_rate, "{name}");
            assert_eq!(streamed.channels, channels, "{name}");
            assert_eq!(
                streamed.samples.len(),
                expected * usize::from(channels),
                "{name} streaming"
            );
            assert_eq!(streamed.samples, parsed.samples, "{name} differs");

            let float_options = ConversionOptions {
                audio_bit_depth: AudioBitDepth::Float32,
                ..Default::default()
            };
            let buffered_float = wav_float_samples(
                &convert_bytes_with_options(name, &input, OutputFormat::Wav, float_options)
                    .unwrap()[0]
                    .bytes,
            );
            let streamed_float = wav_float_samples(&stream_wav(&path, float_options));
            assert_eq!(
                buffered_float, streamed_float,
                "{name} float samples differ"
            );
            let truncation = undithered_i16(&buffered_float);
            let buffered_dithered = parsed.samples != truncation;
            let streamed_dithered = streamed.samples != truncation;
            assert!(
                    buffered_dithered && streamed_dithered,
                    "{name} must differ from the undithered truncation (buffered: {buffered_dithered}, streaming: {streamed_dithered})"
                );
        }

        let aac = fixture("sample-aac.m4a");
        let aac_info =
            super::mp4_gapless_info(crate::container::mp4_find(&aac, b"moov").unwrap(), 0)
                .expect("the AAC edit list is present");
        assert_eq!(aac_info.skip, 1_024, "AAC priming was not read");
        assert_eq!(aac_info.limit, Some(88_200));
        assert!(aac_info.limit_is_presentation);

        let m4a = fixture("sample.m4a");
        let m4a_info =
            super::mp4_gapless_info(crate::container::mp4_find(&m4a, b"moov").unwrap(), 0)
                .expect("the audio track duration is present");
        assert_eq!(m4a_info.skip, 0);
        assert_eq!(m4a_info.limit, Some(135_168));
        assert!(!m4a_info.limit_is_presentation);

        let opus = fixture("sample-vp9-opus.mp4");
        let opus_info =
            super::mp4_gapless_info(crate::container::mp4_find(&opus, b"moov").unwrap(), 1)
                .expect("the Opus edit list is present");
        assert_eq!(opus_info.skip, 336);
        assert_eq!(opus_info.limit, Some(96_048));
        assert!(opus_info.limit_is_presentation);

        let ac3 = fixture("sample-ac3.mp4");
        let ac3_info =
            super::mp4_gapless_info(crate::container::mp4_find(&ac3, b"moov").unwrap(), 0)
                .expect("the AC-3 edit list is present");
        assert_eq!(ac3_info.skip, 256);
        assert_eq!(ac3_info.limit, Some(96_000));
        assert!(ac3_info.limit_is_presentation);

        // The Opus decoder already consumed the 312-sample header delay, so
        // only the remaining 24 samples of the edit list offset are skipped.
        assert_eq!(
            super::mp4_trim_for_kind(opus_info, super::AudioCodecKind::Opus, 312),
            super::AudioTrim {
                skip_frames: 24,
                limit_frames: Some(96_048),
                tail_padding: 0,
            }
        );
    }
    #[test]
    fn resampler_preserves_the_passband_and_rejects_aliases() {
        let tone = |rate: u32, frequency: f64, seconds: f64| super::DecodedAudio {
            samples: (0..(rate as f64 * seconds) as usize)
                .map(|frame| {
                    (std::f64::consts::TAU * frequency * frame as f64 / f64::from(rate)).sin()
                        as f32
                        * 0.5
                })
                .collect(),
            channels: 1,
            sample_rate: rate,
            source_bits: None,
        };
        let rms = |samples: &[f32]| {
            (samples
                .iter()
                .map(|sample| f64::from(*sample) * f64::from(*sample))
                .sum::<f64>()
                / samples.len() as f64)
                .sqrt()
        };

        let passband = super::resample_audio(tone(48_000, 1_000.0, 0.5), 24_000).unwrap();
        assert_eq!(passband.sample_rate, 24_000);
        assert_eq!(passband.samples.len(), 12_000);
        assert!(
            (rms(&passband.samples) - 0.353_553).abs() < 0.03,
            "passband RMS changed to {}",
            rms(&passband.samples)
        );
        let crossings = passband
            .samples
            .windows(2)
            .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
            .count();
        assert!(
            (crossings as i64 - 1_000).abs() < 30,
            "expected about 1000 zero crossings, got {crossings}"
        );

        // 20 kHz cannot exist at 24 kHz. A linear interpolator folds it to
        // 4 kHz; the band-limited filter must remove it instead.
        let alias = super::resample_audio(tone(48_000, 20_000.0, 0.5), 24_000).unwrap();
        assert!(
            rms(&alias.samples) < 0.01,
            "20 kHz aliased through at RMS {}",
            rms(&alias.samples)
        );
    }
    fn linear_resample_reference(
        samples: &[f32],
        channels: usize,
        source_rate: u32,
        target_rate: u32,
    ) -> Vec<f32> {
        if channels == 0 {
            return Vec::new();
        }
        let frames = samples.len() / channels;
        if frames == 0 {
            return Vec::new();
        }
        let ratio = f64::from(target_rate) / f64::from(source_rate);
        let target_frames = ((frames as f64 * ratio).round() as usize).max(1);
        let mut output = Vec::with_capacity(target_frames * channels);
        for frame in 0..target_frames {
            let position = frame as f64 / ratio;
            let left = position.floor() as usize;
            let right = (left + 1).min(frames - 1);
            let fraction = (position - left as f64) as f32;
            for channel in 0..channels {
                let a = samples[left * channels + channel];
                let b = samples[right * channels + channel];
                output.push(a + (b - a) * fraction);
            }
        }
        output
    }
    fn mono_tone(rate: u32, frequency: f64, seconds: f64) -> super::DecodedAudio {
        let frames = (rate as f64 * seconds).round() as usize;
        super::DecodedAudio {
            samples: (0..frames)
                .map(|frame| {
                    (std::f64::consts::TAU * frequency * frame as f64 / f64::from(rate)).sin()
                        as f32
                        * 0.5
                })
                .collect(),
            channels: 1,
            sample_rate: rate,
            source_bits: None,
        }
    }
    fn zero_crossings(samples: &[f32]) -> usize {
        samples
            .windows(2)
            .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
            .count()
    }
    #[test]
    fn resampler_rejects_high_frequency_images_at_44_1_and_48_khz() {
        // Sweep test tones across the region above the new 22.05 kHz Nyquist.
        // Linear interpolation folds each one back into the output, while the
        // band-limited path rejects it. (Frequencies must stay below the 24 kHz
        // source Nyquist; a 30 kHz test tone would already alias at 48 kHz.)
        for (target_rate, frequency) in [
            (44_100, 23_000.0),
            (44_100, 23_300.0),
            (44_100, 23_600.0),
            (44_100, 23_900.0),
            // A non-integer 48 -> 32 kHz ratio has a 16 kHz output Nyquist.
            (32_000, 23_000.0),
        ] {
            let source = mono_tone(48_000, frequency, 0.5);
            let old = linear_resample_reference(&source.samples, 1, 48_000, target_rate);
            let new = super::resample_audio(source, target_rate).unwrap();
            assert!(
                rms(&old) > 0.2,
                "{frequency} Hz -> {target_rate}: the linear reference did not alias: RMS {}",
                rms(&old)
            );
            assert!(
                rms(&new.samples) < 0.03,
                "{frequency} Hz -> {target_rate} aliased through the band-limited path at RMS {}",
                rms(&new.samples)
            );
            if target_rate == 32_000 {
                // 23 kHz sampled at 48 kHz and resampled to 32 kHz folds to
                // 9 kHz with linear interpolation: a clearly low image.
                assert!(
                    (zero_crossings(&old) as i64 - 9_000).abs() < 100,
                    "the linear reference did not produce the 9 kHz image: {} crossings",
                    zero_crossings(&old)
                );
            }
        }
    }
    #[test]
    fn resampler_preserves_passband_amplitude_and_frequency() {
        for (source_rate, target_rate) in [(44_100, 48_000), (48_000, 44_100), (48_000, 32_000)] {
            let source = mono_tone(source_rate, 10_000.0, 0.5);
            let output = super::resample_audio(source, target_rate).unwrap();
            assert_eq!(output.sample_rate, target_rate);
            assert_eq!(
                output.samples.len(),
                ((source_rate as f64 * 0.5 * target_rate as f64 / source_rate as f64).round()
                    as usize)
            );
            assert!(
                (rms(&output.samples) - 0.353_553).abs() < 0.03,
                "{source_rate} -> {target_rate}: passband RMS {}",
                rms(&output.samples)
            );
            let crossings = zero_crossings(&output.samples);
            let expected = (10_000.0 * 0.5 * 2.0) as i64;
            assert!(
                    (crossings as i64 - expected).abs() < 20,
                    "{source_rate} -> {target_rate}: expected about {expected} crossings, got {crossings}"
                );
        }
    }
    #[test]
    fn resampler_preserves_exact_frame_counts_and_streaming_continuity() {
        let directory = stream_temp_dir("resampler-counts");
        for (source_rate, frames, target_rate, expected) in [
            (44_100, 44_100, 48_000, 48_000),
            (48_000, 48_000, 44_100, 44_100),
            // 48 kHz -> 32 kHz is a non-integer ratio (2/3).
            (48_000, 48_000, 32_000, 32_000),
            (44_100, 44_100, 32_000, 32_000),
        ] {
            let source = mono_tone(source_rate, 440.0, frames as f64 / source_rate as f64);
            let direct = super::resample_audio(source, target_rate).unwrap();
            assert_eq!(
                direct.samples.len(),
                expected,
                "direct {source_rate}->{target_rate}"
            );

            let input = directory.join(format!("tone-{source_rate}-{frames}.wav"));
            std::fs::write(&input, wav_fixture_at(source_rate, frames)).unwrap();
            let streamed = stream_wav(
                &input,
                ConversionOptions {
                    audio_sample_rate: Some(target_rate),
                    ..Default::default()
                },
            );
            let parsed = parse_wav(&streamed);
            assert_eq!(parsed.sample_rate, target_rate);
            assert_eq!(
                parsed.samples.len(),
                expected * usize::from(parsed.channels),
                "streaming {source_rate}->{target_rate}"
            );

            let buffered = convert_bytes_with_options(
                "tone.wav",
                &wav_fixture_at(source_rate, frames),
                OutputFormat::Wav,
                ConversionOptions {
                    audio_sample_rate: Some(target_rate),
                    ..Default::default()
                },
            )
            .unwrap();
            let buffered = parse_wav(&buffered[0].bytes);
            assert_eq!(buffered.samples.len(), parsed.samples.len());
            let max_difference = buffered
                .samples
                .iter()
                .zip(&parsed.samples)
                .map(|(a, b)| (i32::from(*a) - i32::from(*b)).abs())
                .max()
                .unwrap();
            assert!(
                    max_difference <= 2,
                    "streaming chunking changed samples by {max_difference} for {source_rate}->{target_rate}"
                );
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }
    #[test]
    fn writes_24_bit_and_float_wav() {
        let wav = wav_fixture();
        let original = parse_wav(&wav);

        let pcm24 = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Wav,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Bit24,
                ..Default::default()
            },
        )
        .unwrap();
        let (format, channels, rate, bits, offset, size) = wav_layout(&pcm24[0].bytes);
        assert_eq!((format, channels, rate, bits), (1, 2, 8_000, 24));
        assert_eq!(size, 16_000 * 3);
        let sample = 300;
        let bytes = &pcm24[0].bytes[offset + sample * 3..offset + sample * 3 + 3];
        let raw = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]);
        let value = (raw << 8) >> 8;
        assert!(
            (i32::from(original.samples[sample]) * 256 - value).abs() <= 300,
            "24-bit sample {value} does not match {}",
            original.samples[sample]
        );

        let float = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Wav,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Float32,
                ..Default::default()
            },
        )
        .unwrap();
        let (format, channels, rate, bits, offset, size) = wav_layout(&float[0].bytes);
        assert_eq!((format, channels, rate, bits), (3, 2, 8_000, 32));
        assert_eq!(size, 16_000 * 4);
        assert!(
            float[0].bytes.windows(4).any(|window| window == b"fact"),
            "a float WAV should carry a fact chunk"
        );
        let value = f32::from_le_bytes(
            float[0].bytes[offset + sample * 4..offset + sample * 4 + 4]
                .try_into()
                .unwrap(),
        );
        let expected = f32::from(original.samples[sample]) / f32::from(i16::MAX);
        assert!((value - expected).abs() < 0.001);
    }
    #[test]
    fn writes_24_bit_flac_and_rejects_float_flac() {
        let wav = wav_fixture();
        let flac = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Flac,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Bit24,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(&flac[0].bytes[0..4], b"fLaC");
        let streaminfo = &flac[0].bytes[8..8 + 34];
        let packed = u64::from_be_bytes(streaminfo[10..18].try_into().unwrap());
        assert_eq!(((packed >> 36) & 0x1F) + 1, 24);

        let back = convert_bytes("tone.flac", &flac[0].bytes, OutputFormat::Wav).unwrap();
        let decoded = parse_wav(&back[0].bytes);
        assert_eq!(decoded.samples.len(), 16_000);
        assert_audible(&decoded.samples);

        let error = convert_bytes_with_options(
            "tone.wav",
            &wav,
            OutputFormat::Flac,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Float32,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::UnsupportedBitDepth {
                format: "FLAC",
                depth: "32-bit float"
            }
        ));
    }
    #[test]
    fn writes_wav_at_each_supported_depth_with_decodable_samples() {
        let sample_rate = 48_000;
        let frames = 512;
        let mut source = Vec::with_capacity(frames);
        for frame in 0..frames {
            let value = ((frame as f64 / f64::from(sample_rate) * 440.0 * std::f64::consts::TAU)
                .sin()
                * 0.5
                * 8_388_607.0)
                .round() as i32;
            source.push(value);
        }
        // A very low-level sample, both signs at full scale, and a tone.
        source[0] = 1;
        source[1] = -1;
        source[2] = 8_388_607;
        source[3] = -8_388_608;
        let wav = wav_24_bit_at(sample_rate, 1, &source);
        let expected: Vec<f32> = source
            .iter()
            .map(|sample| *sample as f32 / 8_388_608.0)
            .collect();

        for (depth, expected_format, expected_bits) in [
            (AudioBitDepth::Bit16, 1, 16),
            (AudioBitDepth::Bit24, 1, 24),
            (AudioBitDepth::Float32, 3, 32),
        ] {
            let converted = convert_bytes_with_options(
                "source.wav",
                &wav,
                OutputFormat::Wav,
                ConversionOptions {
                    audio_bit_depth: depth,
                    ..Default::default()
                },
            )
            .unwrap();
            let (format, channels, rate, bits, _, size) = wav_layout(&converted[0].bytes);
            assert_eq!(
                (format, channels, rate, bits),
                (expected_format, 1, sample_rate, expected_bits),
                "unexpected WAV header for {depth:?}"
            );
            assert_eq!(size, frames * usize::from(depth.bits() / 8));

            match depth {
                AudioBitDepth::Bit16 => {
                    let parsed = parse_wav(&converted[0].bytes);
                    assert_eq!(parsed.samples.len(), frames);
                    for (index, (&actual, &expected)) in
                        parsed.samples.iter().zip(&expected).enumerate()
                    {
                        let actual = f32::from(actual) / f32::from(i16::MAX);
                        assert!(
                            (actual - expected).abs() <= 2.5 / f32::from(i16::MAX),
                            "16-bit sample {index} was {actual} instead of {expected}"
                        );
                    }
                }
                AudioBitDepth::Bit24 => {
                    let decoded = wav_24_bit_samples(&converted[0].bytes);
                    assert_eq!(decoded.len(), frames);
                    for (index, (&actual, &expected)) in decoded.iter().zip(&source).enumerate() {
                        assert!(
                            (actual - expected).abs() <= 2,
                            "24-bit sample {index} was {actual} instead of {expected}"
                        );
                    }
                }
                AudioBitDepth::Float32 => {
                    let decoded = wav_float_samples(&converted[0].bytes);
                    assert_eq!(decoded.len(), frames);
                    for (index, (&actual, &expected)) in decoded.iter().zip(&expected).enumerate() {
                        assert!(
                            (actual - expected).abs() <= 1.0e-6,
                            "float sample {index} was {actual} instead of {expected}"
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn preserves_detail_below_16_bit_in_24_bit_flac_and_float_wav() {
        let frames = 8_000;
        let mut source = vec![1_i32; frames];
        source[0] = -1;
        source[1] = 2;
        source[2] = -2;
        source[3] = 8_388_607;
        source[4] = -8_388_608;
        let wav = wav_24_bit_at(48_000, 1, &source);

        let flac = convert_bytes_with_options(
            "detail.wav",
            &wav,
            OutputFormat::Flac,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Bit24,
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = flac_24_bit_samples(&flac[0].bytes);
        assert_eq!(decoded.len(), frames);
        for (index, (&actual, &expected)) in decoded.iter().zip(&source).enumerate() {
            assert!(
                (actual - expected).abs() <= 1,
                "24-bit FLAC sample {index} was {actual} instead of {expected}"
            );
        }
        assert_eq!(decoded[100], 1);

        let float = convert_bytes_with_options(
            "detail.wav",
            &wav,
            OutputFormat::Wav,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Float32,
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = wav_float_samples(&float[0].bytes);
        assert_eq!(decoded.len(), frames);
        for (index, (&actual, &expected)) in decoded.iter().zip(&source).enumerate() {
            let expected = expected as f32 / 8_388_608.0;
            assert!(
                (actual - expected).abs() <= 1.0e-7,
                "float sample {index} was {actual} instead of {expected}"
            );
        }
        assert!(decoded[100] > 0.0);
        assert!(
            decoded[100] < 1.0 / 32_768.0,
            "the float output lost detail below the 16-bit LSB"
        );
    }
    #[test]
    fn dithers_16_bit_output_deterministically_without_dithering_24_bit() {
        let source = vec![1_i32; 512];
        let wav = wav_24_bit_at(48_000, 1, &source);
        let convert = || {
            convert_bytes_with_options(
                "quiet.wav",
                &wav,
                OutputFormat::Wav,
                ConversionOptions {
                    audio_bit_depth: AudioBitDepth::Bit16,
                    ..Default::default()
                },
            )
            .unwrap()
            .remove(0)
            .bytes
        };
        let first = convert();
        let second = convert();
        assert_eq!(first, second, "dither must be reproducible");

        let parsed = parse_wav(&first);
        let mut distinct = parsed.samples.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(
            distinct.len() > 1,
            "a near-silent 16-bit signal was crushed to one value instead of dithered"
        );

        let pcm24 = convert_bytes_with_options(
            "quiet.wav",
            &wav,
            OutputFormat::Wav,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Bit24,
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = wav_24_bit_samples(&pcm24[0].bytes);
        assert!(
            decoded.iter().all(|sample| *sample == 1),
            "24-bit output should be quantized without dither"
        );

        // The same rule applies when the source is a 32-bit f32 pipeline.
        let deep = DecodedAudio {
            samples: vec![1.0 / 8_388_608.0; 512],
            channels: 1,
            sample_rate: 48_000,
            source_bits: Some(32),
        };
        let pcm24 = wav_24_bit_samples(&encode_wav(&deep, AudioBitDepth::Bit24).unwrap());
        assert!(
            pcm24.iter().all(|sample| *sample == 1),
            "24-bit output from a 32-bit source should not be dithered"
        );
        let flac = flac_24_bit_samples(&encode_flac(&deep, AudioBitDepth::Bit24).unwrap());
        assert!(
            flac.iter().all(|sample| *sample == 1),
            "24-bit FLAC from a 32-bit source should not be dithered"
        );
        let float = wav_float_samples(&encode_wav(&deep, AudioBitDepth::Float32).unwrap());
        assert!(float
            .iter()
            .all(|sample| (*sample - 1.0 / 8_388_608.0).abs() <= 1.0e-7));
    }
    #[test]
    fn unknown_precision_f32_to_16_bit_is_dithered() {
        let samples = vec![1.0 / 1_048_576.0; 8_000];
        let unknown = DecodedAudio {
            samples: samples.clone(),
            channels: 1,
            sample_rate: 48_000,
            source_bits: None,
        };
        let known_24 = DecodedAudio {
            source_bits: Some(24),
            ..unknown.clone()
        };
        let known_32 = DecodedAudio {
            source_bits: Some(32),
            ..unknown.clone()
        };

        let unknown_wav = encode_wav(&unknown, AudioBitDepth::Bit16).unwrap();
        assert_eq!(
            unknown_wav,
            encode_wav(&known_24, AudioBitDepth::Bit16).unwrap(),
            "unknown and 24-bit sources should dither identically"
        );
        assert_eq!(
            unknown_wav,
            encode_wav(&known_32, AudioBitDepth::Bit16).unwrap(),
            "unknown and 32-bit sources should dither identically"
        );
        let decoded = parse_wav(&unknown_wav);
        let truncation = undithered_i16(&samples);
        assert_ne!(
            decoded.samples, truncation,
            "unknown-precision WAV matches the undithered truncation"
        );
        let mut distinct = decoded.samples.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(
            distinct.len() > 1,
            "unknown-precision 16-bit output was crushed instead of dithered"
        );
        assert_eq!(
            unknown_wav,
            encode_wav(&unknown, AudioBitDepth::Bit16).unwrap(),
            "dither must be reproducible"
        );

        let unknown_flac = encode_flac(&unknown, AudioBitDepth::Bit16).unwrap();
        assert_eq!(
            unknown_flac,
            encode_flac(&known_24, AudioBitDepth::Bit16).unwrap(),
            "unknown and 24-bit FLAC should dither identically"
        );
        assert_eq!(
            unknown_flac,
            encode_flac(&unknown, AudioBitDepth::Bit16).unwrap(),
            "FLAC dither must be reproducible"
        );
        let mut reader = claxon::FlacReader::new(std::io::Cursor::new(unknown_flac)).unwrap();
        assert_eq!(reader.streaminfo().bits_per_sample, 16);
        let decoded: Vec<i32> = reader.samples().map(Result::unwrap).collect();
        let truncation: Vec<i32> = truncation.iter().map(|&sample| i32::from(sample)).collect();
        assert_ne!(
            decoded, truncation,
            "unknown-precision FLAC matches the undithered truncation"
        );
        let mut distinct = decoded;
        distinct.sort_unstable();
        distinct.dedup();
        assert!(
            distinct.len() > 1,
            "unknown-precision 16-bit FLAC was crushed instead of dithered"
        );
    }
    #[test]
    fn aac_16_bit_output_is_dithered_and_streaming_matches() {
        let path = fixture_path("sample.m4a");
        let input = std::fs::read(&path).unwrap();
        let buffered =
            parse_wav(&convert_bytes("sample.m4a", &input, OutputFormat::Wav).unwrap()[0].bytes);
        let streamed = parse_wav(&stream_wav(&path, ConversionOptions::default()));
        assert_eq!(buffered.samples, streamed.samples, "sample.m4a differs");

        let float_options = ConversionOptions {
            audio_bit_depth: AudioBitDepth::Float32,
            ..Default::default()
        };
        let buffered_float = wav_float_samples(
            &convert_bytes_with_options("sample.m4a", &input, OutputFormat::Wav, float_options)
                .unwrap()[0]
                .bytes,
        );
        let streamed_float = wav_float_samples(&stream_wav(&path, float_options));
        assert_eq!(
            buffered_float, streamed_float,
            "sample.m4a float samples differ"
        );
        let truncation = undithered_i16(&buffered_float);
        let buffered_dithered = buffered.samples != truncation;
        let streamed_dithered = streamed.samples != truncation;
        assert!(
                buffered_dithered && streamed_dithered,
                "AAC 16-bit output must differ from the undithered truncation (buffered: {buffered_dithered}, streaming: {streamed_dithered})"
            );

        let again =
            parse_wav(&convert_bytes("sample.m4a", &input, OutputFormat::Wav).unwrap()[0].bytes);
        assert_eq!(
            buffered.samples, again.samples,
            "dither must be reproducible"
        );

        // The AAC fixture starts with the encoder's quiet pre-roll. A 16-bit
        // conversion without dither crushes that region to zero.
        let quiet = &buffered.samples[..512];
        assert!(
            quiet
                .iter()
                .map(|sample| sample.unsigned_abs())
                .max()
                .unwrap()
                <= 4,
            "the AAC fixture's quiet pre-roll is no longer quiet"
        );
        let mut distinct = quiet.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        assert!(distinct.len() > 1, "AAC 16-bit output was not dithered");
        assert!(
            quiet.iter().any(|sample| *sample != 0),
            "AAC 16-bit output was a silent crush"
        );

        let buffered_flac = convert_bytes("sample.m4a", &input, OutputFormat::Flac)
            .unwrap()
            .remove(0)
            .bytes;
        let mut streamed_flac = std::io::Cursor::new(Vec::new());
        crate::streaming::convert_audio_file_to_writer(
            &path,
            "sample.m4a",
            OutputFormat::Flac,
            ConversionOptions::default(),
            &mut streamed_flac,
        )
        .unwrap();
        let buffered_flac = flac_16_bit_samples(&buffered_flac);
        let streamed_flac = flac_16_bit_samples(&streamed_flac.into_inner());
        assert_eq!(buffered_flac, streamed_flac, "sample.m4a FLAC differs");
        let truncation: Vec<i32> = truncation.iter().map(|&sample| i32::from(sample)).collect();
        let buffered_dithered = buffered_flac != truncation;
        let streamed_dithered = streamed_flac != truncation;
        assert!(
                buffered_dithered && streamed_dithered,
                "AAC FLAC output must differ from the undithered truncation (buffered: {buffered_dithered}, streaming: {streamed_dithered})"
            );
        let mut quiet = buffered_flac[..512].to_vec();
        quiet.sort_unstable();
        quiet.dedup();
        assert!(quiet.len() > 1, "AAC 16-bit FLAC was not dithered");
    }
    #[test]
    fn clamps_out_of_range_samples_at_every_wav_depth() {
        let audio = DecodedAudio {
            samples: vec![2.0, -2.0],
            channels: 1,
            sample_rate: 48_000,
            source_bits: Some(32),
        };
        let pcm16 = parse_wav(&encode_wav(&audio, AudioBitDepth::Bit16).unwrap());
        assert_eq!(pcm16.samples, [i16::MAX, -i16::MAX]);
        let pcm24 = wav_24_bit_samples(&encode_wav(&audio, AudioBitDepth::Bit24).unwrap());
        assert_eq!(pcm24, [8_388_607, -8_388_607]);
        let float = wav_float_samples(&encode_wav(&audio, AudioBitDepth::Float32).unwrap());
        assert_eq!(float, [1.0, -1.0]);
    }
    #[test]
    fn converts_wav_audio_to_mp3_and_back() {
        let wav = wav_fixture_at(44_100, 44_100);
        let mp3 = convert_bytes("tone.wav", &wav, OutputFormat::Mp3).unwrap();
        assert_eq!(mp3[0].name, PathBuf::from("tone.mp3"));
        assert!(
            mp3[0].bytes.starts_with(b"ID3") || mp3[0].bytes[0] == 0xFF,
            "the MP3 output has no MPEG frame or ID3 header"
        );

        let back = convert_bytes("tone.mp3", &mp3[0].bytes, OutputFormat::Wav).unwrap();
        let parsed = parse_wav(&back[0].bytes);
        assert_eq!(parsed.sample_rate, 44_100);
        assert_eq!(parsed.channels, 2);
        assert!(
            (parsed.samples.len() as i64 - 88_200).abs() < 2_500,
            "MP3 round trip produced {} samples",
            parsed.samples.len()
        );
        assert_audible(&parsed.samples);

        // LAME has to clamp its bitrate for low sample rates instead of
        // failing the conversion, even when the UI offers a high bitrate.
        for bitrate in Mp3Bitrate::ALL {
            let low_rate = convert_bytes_with_options(
                "tone.wav",
                &wav_fixture(),
                OutputFormat::Mp3,
                ConversionOptions {
                    mp3_bitrate: bitrate,
                    ..Default::default()
                },
            )
            .unwrap_or_else(|error| panic!("8 kHz MP3 at {} failed: {error}", bitrate.label()));
            assert!(low_rate[0].bytes.len() > 100);
        }
    }
    #[test]
    fn preserves_id3v2_tags_when_re_encoding_mp3() {
        let mp3 = convert_bytes(
            "tone.wav",
            &wav_fixture_at(44_100, 8_000),
            OutputFormat::Mp3,
        )
        .unwrap();
        let mut tagged = b"ID3\x04\x00\x00\x00\x00\x00\x0a".to_vec();
        tagged.extend([0_u8; 10]);
        tagged.extend_from_slice(&mp3[0].bytes);

        let reencoded = convert_bytes("tagged.mp3", &tagged, OutputFormat::Mp3).unwrap();
        assert!(reencoded[0].bytes.starts_with(&tagged[..20]));
    }
    #[test]
    fn preserves_flac_vorbis_comments_and_pictures() {
        let flac = convert_bytes(
            "tone.wav",
            &wav_fixture_at(44_100, 8_000),
            OutputFormat::Flac,
        )
        .unwrap();
        let mut blocks = flac_metadata_blocks(&flac[0].bytes).unwrap();
        let mut comment = Vec::new();
        comment.extend_from_slice(&4_u32.to_le_bytes());
        comment.extend_from_slice(b"test");
        comment.extend_from_slice(&0_u32.to_le_bytes());
        blocks.insert(1, (4, comment.clone()));
        let audio_start = flac_metadata_end(&flac[0].bytes).unwrap();
        let tagged = rebuild_flac(&blocks, &flac[0].bytes[audio_start..]);

        let reencoded = convert_bytes("tagged.flac", &tagged, OutputFormat::Flac).unwrap();
        let output_blocks = flac_metadata_blocks(&reencoded[0].bytes).unwrap();
        assert!(output_blocks
            .iter()
            .any(|(kind, data)| *kind == 4 && data == &comment));
    }
    #[test]
    fn mp3_output_preserves_tone_channels_duration_and_bitrate() {
        for (sample_rate, channels) in [(44_100, 1), (44_100, 2), (48_000, 1), (48_000, 2)] {
            let frames = sample_rate;
            let wav = wav_tone_at(sample_rate, frames, channels, 1_000.0, 0.5);
            let mp3 = convert_bytes_with_options(
                "tone.wav",
                &wav,
                OutputFormat::Mp3,
                ConversionOptions {
                    mp3_bitrate: Mp3Bitrate::Kbps192,
                    ..Default::default()
                },
            )
            .unwrap()
            .remove(0)
            .bytes;
            assert_mp3_tone(
                &mp3,
                sample_rate,
                channels,
                frames as usize,
                Mp3Bitrate::Kbps192,
            );
        }

        let wav = wav_tone_at(44_100, 44_100, 2, 1_000.0, 0.5);
        let encode = |bitrate| {
            convert_bytes_with_options(
                "tone.wav",
                &wav,
                OutputFormat::Mp3,
                ConversionOptions {
                    mp3_bitrate: bitrate,
                    ..Default::default()
                },
            )
            .unwrap()
            .remove(0)
            .bytes
        };
        let low = encode(Mp3Bitrate::Kbps128);
        let high = encode(Mp3Bitrate::Kbps320);
        assert!(
            high.len() > low.len() * 2,
            "320 kbps produced {} bytes but 128 kbps produced {}",
            high.len(),
            low.len()
        );
        assert_mp3_tone(&low, 44_100, 2, 44_100, Mp3Bitrate::Kbps128);
        assert_mp3_tone(&high, 44_100, 2, 44_100, Mp3Bitrate::Kbps320);
    }
    #[test]
    fn converts_movie_audio_to_mp3() {
        for name in ["sample-ac3.mkv", "sample-eac3.mkv"] {
            let input = fixture(name);
            let mp3 = convert_bytes_with_options(
                name,
                &input,
                OutputFormat::Mp3,
                ConversionOptions {
                    mp3_bitrate: Mp3Bitrate::Kbps256,
                    ..Default::default()
                },
            )
            .unwrap()
            .remove(0)
            .bytes;
            assert_eq!(mp3_first_frame_bitrate(&mp3), Some(256), "{name}");

            let decoded = convert_bytes("movie.mp3", &mp3, OutputFormat::Wav).unwrap();
            let parsed = parse_wav(&decoded[0].bytes);
            assert_eq!(parsed.sample_rate, 48_000, "{name}");
            assert_eq!(parsed.channels, 2, "{name}");
            assert!(
                parsed.samples.len() / 2 >= 90_000,
                "{name} was truncated to {} frames",
                parsed.samples.len() / 2
            );
            assert_audible(&parsed.samples);
        }
    }
    #[test]
    fn malformed_containers_return_errors_instead_of_panicking() {
        for (name, bytes) in [
            ("broken.webm", b"\x1a\x45\xdf\xa3".as_slice()),
            ("broken.mkv", &[0x1A, 0x45, 0xDF, 0xA3, 0x80][..]),
            ("broken.mp4", b"\0\0\0\x08moov".as_slice()),
            ("broken.ac3", b"\x0b\x77".as_slice()),
            ("empty.flac", b"".as_slice()),
        ] {
            let _ = convert_bytes(name, bytes, OutputFormat::Wav);
            let _ = super::audio_tracks(bytes, "webm");
        }
        assert_eq!(
            super::matroska_discard_padding(b"\x1a\x45\xdf\xa3", 1),
            None
        );
        assert_eq!(super::matroska_discard_padding(b"not matroska", 1), None);
        assert_eq!(super::mp4_gapless_info(b"\0\0\0\x08moov", 0), None);
    }
}
