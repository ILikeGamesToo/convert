// SPDX-License-Identifier: GPL-2.0-or-later

//! Streaming audio conversion.
//!
//! The in-memory API has to hold the decoded file and the encoded output at
//! the same time. Feature-length movie audio makes that impractical, so this
//! module decodes, remixes, resamples and encodes one small chunk at a time.
//! Encoders write directly to the output file; WAV, AIFF and AU patch their
//! size fields when the stream ends.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::audio::{
    audio_track_entries_from_path, downmix_mono, downmix_stereo, extended_sample_rate,
    is_end_of_stream, matroska_audio_timing_from_path, mp3_lame_bitrate, mp3_sample_rate,
    mp4_gapless_info, mp4_trim_for_kind, opus_channels, opus_sample_rate, opus_stream_info,
    probe_symphonia_file, read_mp4_moov, reorder_film_to_opus_channels, reorder_opus_channels,
    reported_source_bits, resample_kernel_table, sample_to_i16, skip_id3v2_file, AudioBitDepth,
    AudioChannels, AudioCodecKind, AudioTrackEntry, AudioTrim, Dither, Mp3Bitrate, OpusPcmDecoder,
    OpusPcmEncoder, RESAMPLE_PHASES, RESAMPLE_ZERO_CROSSINGS,
};
use crate::container::is_mp4_extension;
use crate::error::ConvertError;
use crate::format::OutputFormat;
use crate::options::ConversionOptions;
use crate::shared::{file_extension, is_audio_extension, unsupported};

/// A chunk of decoded audio. Chunks are bounded by the decoder's packet or
/// frame size, never by the length of the file.
struct AudioChunk {
    samples: Vec<f32>,
    channels: usize,
    sample_rate: u32,
    source_bits: Option<u16>,
}

trait AudioChunkReader {
    fn next_chunk(&mut self) -> Result<Option<AudioChunk>, ConvertError>;
}

trait WriteSeek: Write + Seek {}
impl<T: Write + Seek> WriteSeek for T {}

/// Converts one audio file to `target` without buffering the decoded audio or
/// the encoded result. The caller owns the output file and can use it again
/// after this returns.
pub fn convert_audio_file_to_writer<W: Write + Seek>(
    input_path: &Path,
    input_name: &str,
    target: OutputFormat,
    options: ConversionOptions,
    writer: &mut W,
) -> Result<(), ConvertError> {
    let cancel = AtomicBool::new(false);
    convert_audio_file_to_writer_with_cancel(
        input_path, input_name, target, options, writer, &cancel,
    )
}

/// The cancellable form of [`convert_audio_file_to_writer`]. The flag is
/// checked between decoded chunks, so a feature-length file stops promptly.
pub fn convert_audio_file_to_writer_with_cancel<W: Write + Seek>(
    input_path: &Path,
    input_name: &str,
    target: OutputFormat,
    options: ConversionOptions,
    writer: &mut W,
    cancel: &AtomicBool,
) -> Result<(), ConvertError> {
    if !matches!(
        target,
        OutputFormat::Wav
            | OutputFormat::Flac
            | OutputFormat::Aiff
            | OutputFormat::Au
            | OutputFormat::Opus
            | OutputFormat::Mp3
    ) {
        return Err(unsupported(&file_extension(input_name), target));
    }
    let extension = file_extension(input_name);
    if !is_audio_extension(&extension) {
        return Err(unsupported(&extension, target));
    }

    let entries = audio_track_entries_from_path(input_path)?;
    let index = options.audio_track.unwrap_or(0);
    let entry = entries.get(index).ok_or(ConvertError::AudioTrackNotFound {
        requested: index + 1,
        available: entries.len(),
    })?;
    let trim = audio_trim(input_path, &extension, entry);
    let mut reader = open_reader(input_path, &extension, entry)?;
    let mut converter = AudioConverter::new(target, options, trim, writer, cancel);

    while let Some(chunk) = reader.next_chunk()? {
        if cancel.load(Ordering::Relaxed) {
            return Err(ConvertError::Cancelled);
        }
        converter.push(chunk)?;
    }
    converter.finish()
}

fn audio_trim(input_path: &Path, extension: &str, entry: &AudioTrackEntry) -> AudioTrim {
    if is_mp4_extension(extension) {
        return read_mp4_moov(input_path)
            .and_then(|moov| mp4_gapless_info(&moov, entry.track_index))
            .map(|info| mp4_trim_for_kind(info, entry.kind, entry.pre_skip))
            .unwrap_or_default();
    }

    // Ogg's Trim already handles the pre-skip and the final granule. Matroska
    // stores the end trim on the selected track's final block; scan only the
    // bounded tail so a long movie is still streamed, not buffered.
    if matches!(extension, "mkv" | "webm") && entry.kind == AudioCodecKind::Opus {
        return matroska_audio_timing_from_path(input_path, entry.info.id).to_trim();
    }
    AudioTrim::default()
}

fn open_reader(
    path: &Path,
    extension: &str,
    entry: &AudioTrackEntry,
) -> Result<Box<dyn AudioChunkReader>, ConvertError> {
    if extension == "flac" {
        return Ok(Box::new(FlacChunkReader::new(path)?));
    }
    if entry.kind == AudioCodecKind::Opus && matches!(extension, "ogg" | "opus") {
        return Ok(Box::new(OggOpusChunkReader::open(path)?));
    }
    if entry.kind == AudioCodecKind::Ac3 && matches!(extension, "ac3" | "eac3" | "ec3") {
        return Ok(Box::new(Ac3RawChunkReader::open(path)?));
    }

    let format = probe_symphonia_file(File::open(path)?, extension)?;
    let codec_params = format
        .tracks()
        .iter()
        .find(|track| track.id == entry.info.id)
        .map(|track| track.codec_params.clone())
        .ok_or(ConvertError::NoAudioTrack)?;
    match entry.kind {
        AudioCodecKind::Opus => Ok(Box::new(OpusTrackChunkReader::new(
            format,
            entry.info.id,
            &codec_params,
        )?)),
        AudioCodecKind::Ac3 => Ok(Box::new(Ac3TrackChunkReader::new(format, entry.info.id))),
        AudioCodecKind::Symphonia => Ok(Box::new(SymphoniaChunkReader::new(
            format,
            codec_params,
            entry.info.id,
            entry.info.channels,
        )?)),
    }
}

struct FlacChunkReader {
    reader: claxon::FlacReader<File>,
    buffer: Vec<i32>,
    samples: Vec<f32>,
    sample_rate: u32,
    bits: u16,
}

impl FlacChunkReader {
    fn new(input_path: &Path) -> Result<Self, ConvertError> {
        let mut file = File::open(input_path)?;
        skip_id3v2_file(&mut file)?;
        let reader = claxon::FlacReader::new(file).map_err(|error| {
            ConvertError::Audio(format!("could not open the FLAC stream: {error}"))
        })?;
        let info = reader.streaminfo();
        Ok(Self {
            reader,
            buffer: Vec::new(),
            samples: Vec::new(),
            sample_rate: info.sample_rate,
            bits: info.bits_per_sample as u16,
        })
    }
}

impl AudioChunkReader for FlacChunkReader {
    fn next_chunk(&mut self) -> Result<Option<AudioChunk>, ConvertError> {
        let block = match self
            .reader
            .blocks()
            .read_next_or_eof(std::mem::take(&mut self.buffer))
        {
            Ok(Some(block)) => block,
            Ok(None) => return Ok(None),
            Err(error) => {
                return Err(ConvertError::Audio(format!(
                    "could not decode FLAC audio: {error}"
                )))
            }
        };
        let channels = block.channels() as usize;
        let frames = block.duration() as usize;
        let buffer = block.into_buffer();
        let scale = (1_u64 << (self.bits.saturating_sub(1))) as f32;
        self.samples.clear();
        self.samples.reserve(frames * channels);
        for frame in 0..frames {
            for channel in 0..channels {
                self.samples
                    .push(buffer[channel * frames + frame] as f32 / scale);
            }
        }
        self.buffer = buffer;
        Ok(Some(AudioChunk {
            samples: std::mem::take(&mut self.samples),
            channels,
            sample_rate: self.sample_rate,
            source_bits: Some(self.bits),
        }))
    }
}

struct SymphoniaChunkReader {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
    sample_buffer: Option<symphonia::core::audio::SampleBuffer<f32>>,
    source_bits: Option<u16>,
}

impl SymphoniaChunkReader {
    fn new(
        format: Box<dyn symphonia::core::formats::FormatReader>,
        codec_params: symphonia::core::codecs::CodecParameters,
        track_id: u32,
        track_channels: usize,
    ) -> Result<Self, ConvertError> {
        use symphonia::core::codecs::DecoderOptions;

        let decoder = symphonia::default::get_codecs()
            .make(&codec_params, &DecoderOptions::default())
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
        let source_bits =
            crate::audio::reported_source_bits(codec_params.codec, codec_params.bits_per_sample);
        Ok(Self {
            format,
            decoder,
            track_id,
            sample_buffer: None,
            source_bits,
        })
    }
}

impl AudioChunkReader for SymphoniaChunkReader {
    fn next_chunk(&mut self) -> Result<Option<AudioChunk>, ConvertError> {
        use symphonia::core::errors::Error as SymphoniaError;

        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(error) if is_end_of_stream(&error) => return Ok(None),
                Err(SymphoniaError::ResetRequired) => return Ok(None),
                Err(error) => {
                    return Err(ConvertError::Audio(format!(
                        "could not read audio packets: {error}"
                    )))
                }
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(decoded) => decoded,
                Err(error) if is_end_of_stream(&error) => return Ok(None),
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(error) => {
                    return Err(ConvertError::Audio(format!(
                        "could not decode audio samples: {error}"
                    )))
                }
            };
            let spec = *decoded.spec();
            let channels = spec.channels.count();
            if channels == 0 || spec.rate == 0 {
                continue;
            }
            let capacity = decoded.capacity();
            if self
                .sample_buffer
                .as_ref()
                .is_none_or(|buffer| buffer.capacity() < capacity)
            {
                self.sample_buffer = Some(symphonia::core::audio::SampleBuffer::new(
                    capacity as u64,
                    spec,
                ));
            }
            let buffer = self
                .sample_buffer
                .as_mut()
                .expect("the sample buffer was just initialized");
            buffer.copy_interleaved_ref(decoded);
            return Ok(Some(AudioChunk {
                samples: buffer.samples().to_vec(),
                channels,
                sample_rate: spec.rate,
                source_bits: self.source_bits,
            }));
        }
    }
}

struct OpusTrackChunkReader {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    track_id: u32,
    decoder: OpusPcmDecoder,
    block: Vec<f32>,
    channels: usize,
    pre_skip_remaining: usize,
}

impl OpusTrackChunkReader {
    fn new(
        format: Box<dyn symphonia::core::formats::FormatReader>,
        track_id: u32,
        codec_params: &symphonia::core::codecs::CodecParameters,
    ) -> Result<Self, ConvertError> {
        let info = opus_stream_info(codec_params)?;
        let channels = info.channels;
        let decoder = OpusPcmDecoder::new(channels, info.mapping_family)?;
        Ok(Self {
            format,
            track_id,
            decoder,
            block: vec![0.0; opus_pure::MAX_PACKET_SAMPLES * channels],
            channels,
            pre_skip_remaining: info.pre_skip as usize,
        })
    }
}

impl AudioChunkReader for OpusTrackChunkReader {
    fn next_chunk(&mut self) -> Result<Option<AudioChunk>, ConvertError> {
        use symphonia::core::errors::Error as SymphoniaError;

        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(error) if is_end_of_stream(&error) => return Ok(None),
                Err(SymphoniaError::ResetRequired) => return Ok(None),
                Err(error) => {
                    return Err(ConvertError::Audio(format!(
                        "could not read audio packets: {error}"
                    )))
                }
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let decoded = self
                .decoder
                .decode(&packet.data, opus_pure::MAX_PACKET_SAMPLES, &mut self.block)
                .map_err(|error| {
                    ConvertError::Audio(format!("could not decode Opus audio: {error}"))
                })?;
            reorder_opus_channels(&mut self.block[..decoded * self.channels], self.channels);
            let skip = self.pre_skip_remaining.min(decoded);
            self.pre_skip_remaining -= skip;
            let start = skip * self.channels;
            let end = decoded * self.channels;
            if start >= end {
                continue;
            }
            return Ok(Some(AudioChunk {
                samples: self.block[start..end].to_vec(),
                channels: self.channels,
                sample_rate: 48_000,
                source_bits: None,
            }));
        }
    }
}

struct OggOpusChunkReader {
    reader: opus_pure::OggOpusReader<File>,
    decoder: OpusPcmDecoder,
    trim: opus_pure::Trim,
    block: Vec<f32>,
    channels: usize,
    packet: opus_pure::OggPacket,
}

impl OggOpusChunkReader {
    fn open(path: &Path) -> Result<Self, ConvertError> {
        use opus_pure::OggOpusReader;

        let reader = OggOpusReader::new(File::open(path)?).map_err(|error| {
            ConvertError::Audio(format!("could not open the Opus stream: {error}"))
        })?;
        let head = reader.head().clone();
        let channels = opus_channels(head.channel_count as usize, head.mapping_family)?;
        let decoder = OpusPcmDecoder::new(channels, head.mapping_family)?;
        let trim = opus_pure::Trim::new(&head, 48_000, channels).map_err(|error| {
            ConvertError::Audio(format!("could not read the Opus stream: {error}"))
        })?;
        Ok(Self {
            reader,
            decoder,
            trim,
            block: vec![0.0; opus_pure::MAX_PACKET_SAMPLES * channels],
            channels,
            packet: opus_pure::OggPacket::default(),
        })
    }
}

impl AudioChunkReader for OggOpusChunkReader {
    fn next_chunk(&mut self) -> Result<Option<AudioChunk>, ConvertError> {
        loop {
            if !self
                .reader
                .read_packet_into(&mut self.packet)
                .map_err(|error| {
                    ConvertError::Audio(format!("could not read Opus packets: {error}"))
                })?
            {
                return Ok(None);
            }
            let decoded = self
                .decoder
                .decode(
                    &self.packet.data,
                    opus_pure::MAX_PACKET_SAMPLES,
                    &mut self.block,
                )
                .map_err(|error| {
                    ConvertError::Audio(format!("could not decode Opus audio: {error}"))
                })?;
            reorder_opus_channels(&mut self.block[..decoded * self.channels], self.channels);
            let kept = self
                .trim
                .keep(&self.packet, &self.block[..decoded * self.channels]);
            if kept.is_empty() {
                continue;
            }
            return Ok(Some(AudioChunk {
                samples: kept.to_vec(),
                channels: self.channels,
                sample_rate: 48_000,
                source_bits: None,
            }));
        }
    }
}

struct Ac3RawChunkReader {
    reader: File,
    buffer: Vec<u8>,
    decoder: filmcraft_ac3::Decoder,
    eof: bool,
}

impl Ac3RawChunkReader {
    fn open(path: &Path) -> Result<Self, ConvertError> {
        Ok(Self {
            reader: File::open(path)?,
            buffer: Vec::new(),
            decoder: filmcraft_ac3::Decoder::new(),
            eof: false,
        })
    }

    fn fill(&mut self) -> Result<bool, ConvertError> {
        if self.eof {
            return Ok(false);
        }
        let mut chunk = [0_u8; 64 * 1024];
        let read = self.reader.read(&mut chunk)?;
        if read == 0 {
            self.eof = true;
            return Ok(false);
        }
        self.buffer.extend_from_slice(&chunk[..read]);
        Ok(true)
    }
}

impl AudioChunkReader for Ac3RawChunkReader {
    fn next_chunk(&mut self) -> Result<Option<AudioChunk>, ConvertError> {
        loop {
            let Some(position) = self
                .buffer
                .windows(2)
                .position(|window| window == [0x0B, 0x77])
            else {
                // Keep only the last byte when the current buffer has no
                // syncword: a frame may begin in the next read. If the buffer
                // is empty, fill it and search the new data instead of
                // discarding it.
                if !self.buffer.is_empty() && self.buffer.len() > 1 {
                    let keep = self.buffer.len() - 1;
                    self.buffer.drain(..keep);
                }
                if self.fill()? {
                    continue;
                }
                return Ok(None);
            };
            if position > 0 {
                self.buffer.drain(..position);
            }
            if self.buffer.len() < 7 {
                if self.fill()? {
                    continue;
                }
                return Ok(None);
            }
            let header = filmcraft_ac3::parse_header(&self.buffer).map_err(|error| {
                ConvertError::Audio(format!("could not read the AC-3 stream: {error}"))
            })?;
            if self.buffer.len() < header.frame_bytes {
                if self.fill()? {
                    continue;
                }
                return Ok(None);
            }
            let frame = &self.buffer[..header.frame_bytes];
            let decoded = self.decoder.decode(frame).map_err(|error| {
                ConvertError::Audio(format!("could not decode AC-3 audio: {error}"))
            })?;
            self.buffer.drain(..header.frame_bytes);
            if let Some(chunk) = ac3_chunk(decoded) {
                return Ok(Some(chunk));
            }
        }
    }
}

struct Ac3TrackChunkReader {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    track_id: u32,
    decoder: filmcraft_ac3::Decoder,
    pending: Vec<u8>,
    position: usize,
}

impl Ac3TrackChunkReader {
    fn new(format: Box<dyn symphonia::core::formats::FormatReader>, track_id: u32) -> Self {
        Self {
            format,
            track_id,
            decoder: filmcraft_ac3::Decoder::new(),
            pending: Vec::new(),
            position: 0,
        }
    }

    fn read_packet(&mut self) -> Result<bool, ConvertError> {
        use symphonia::core::errors::Error as SymphoniaError;

        loop {
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(error) if is_end_of_stream(&error) => return Ok(false),
                Err(SymphoniaError::ResetRequired) => return Ok(false),
                Err(error) => {
                    return Err(ConvertError::Audio(format!(
                        "could not read audio packets: {error}"
                    )))
                }
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            self.pending.extend_from_slice(&packet.data);
            return Ok(true);
        }
    }
}

impl AudioChunkReader for Ac3TrackChunkReader {
    fn next_chunk(&mut self) -> Result<Option<AudioChunk>, ConvertError> {
        loop {
            while self.position + 7 <= self.pending.len()
                && !self.pending[self.position..].starts_with(&[0x0B, 0x77])
            {
                self.position += 1;
            }
            if self.position + 7 > self.pending.len() {
                // Keep a partial syncword or header for the next packet.
                self.pending.drain(..self.position);
                self.position = 0;
                if self.read_packet()? {
                    continue;
                }
                return Ok(None);
            }
            let header =
                filmcraft_ac3::parse_header(&self.pending[self.position..]).map_err(|error| {
                    ConvertError::Audio(format!("could not read the AC-3 stream: {error}"))
                })?;
            if self.position + header.frame_bytes > self.pending.len() {
                // The frame continues in the next packet; keep the prefix.
                self.pending.drain(..self.position);
                self.position = 0;
                if self.read_packet()? {
                    continue;
                }
                return Ok(None);
            }
            let frame = &self.pending[self.position..self.position + header.frame_bytes];
            let decoded = self.decoder.decode(frame).map_err(|error| {
                ConvertError::Audio(format!("could not decode AC-3 audio: {error}"))
            })?;
            self.position += header.frame_bytes;
            if let Some(chunk) = ac3_chunk(decoded) {
                return Ok(Some(chunk));
            }
        }
    }
}

fn ac3_chunk(decoded: filmcraft_ac3::Decoded) -> Option<AudioChunk> {
    let channels = decoded.channels.len();
    if channels == 0 {
        return None;
    }
    let frames = decoded.channels[0].len();
    if frames == 0 {
        return None;
    }
    let mut samples = Vec::with_capacity(frames * channels);
    for frame in 0..frames {
        for channel in &decoded.channels {
            samples.push(channel[frame]);
        }
    }
    Some(AudioChunk {
        samples,
        channels,
        sample_rate: decoded.header.sample_rate,
        source_bits: None,
    })
}

struct AudioConverter<'a> {
    target: OutputFormat,
    options: ConversionOptions,
    trim: AudioTrim,
    writer: Option<&'a mut dyn WriteSeek>,
    processor: Option<AudioProcessor>,
    encoder: Option<Box<dyn StreamingEncoder + 'a>>,
    cancel: &'a AtomicBool,
    wrote_audio: bool,
}

impl<'a> AudioConverter<'a> {
    fn new(
        target: OutputFormat,
        options: ConversionOptions,
        trim: AudioTrim,
        writer: &'a mut dyn WriteSeek,
        cancel: &'a AtomicBool,
    ) -> Self {
        Self {
            target,
            options,
            trim,
            writer: Some(writer),
            processor: None,
            encoder: None,
            cancel,
            wrote_audio: false,
        }
    }

    fn push(&mut self, chunk: AudioChunk) -> Result<(), ConvertError> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(ConvertError::Cancelled);
        }
        if self.processor.is_none() {
            let source_channels = chunk.channels;
            let source_rate = chunk.sample_rate;
            if source_channels == 0 || source_rate == 0 {
                return Err(ConvertError::NoAudioTrack);
            }
            let target_channels = target_channels(self.target, source_channels, self.options);
            let target_rate = target_rate(self.target, source_rate, self.options);
            let processor = AudioProcessor::new(
                source_channels,
                target_channels,
                source_rate,
                target_rate,
                self.trim,
            );
            let writer = self
                .writer
                .take()
                .ok_or_else(|| ConvertError::Audio("the output stream is closed".to_owned()))?;
            let encoder = create_encoder(
                self.target,
                self.options,
                target_rate,
                target_channels,
                chunk.source_bits,
                writer,
            )?;
            self.processor = Some(processor);
            self.encoder = Some(encoder);
        }

        let processed = self
            .processor
            .as_mut()
            .expect("the processor was just initialized")
            .push(&chunk.samples, chunk.channels)?;
        if !processed.is_empty() {
            self.wrote_audio = true;
        }
        self.encoder
            .as_mut()
            .expect("the encoder was just initialized")
            .write(&processed)
    }

    fn finish(mut self) -> Result<(), ConvertError> {
        let Some(mut processor) = self.processor.take() else {
            return Err(ConvertError::NoAudioTrack);
        };
        let tail = processor.finish()?;
        if !tail.is_empty() {
            self.wrote_audio = true;
        }
        let Some(mut encoder) = self.encoder.take() else {
            return Err(ConvertError::NoAudioTrack);
        };
        if !self.wrote_audio {
            return Err(ConvertError::NoAudioTrack);
        }
        encoder.write(&tail)?;
        encoder.finish()
    }
}

fn target_channels(
    target: OutputFormat,
    source_channels: usize,
    options: ConversionOptions,
) -> usize {
    let requested = options
        .audio_channels
        .map(AudioChannels::count)
        .unwrap_or(source_channels)
        .max(1);
    // MP3 is the only streaming encoder that cannot carry more than stereo;
    // Opus uses the multistream encoder for surround layouts.
    if target == OutputFormat::Mp3 {
        requested.min(2)
    } else {
        requested
    }
}

fn target_rate(target: OutputFormat, source_rate: u32, options: ConversionOptions) -> u32 {
    let requested = match target {
        OutputFormat::Opus => options
            .audio_sample_rate
            .map(opus_sample_rate)
            .unwrap_or_else(|| opus_sample_rate(source_rate)),
        OutputFormat::Mp3 => options
            .audio_sample_rate
            .map(mp3_sample_rate)
            .unwrap_or_else(|| mp3_sample_rate(source_rate)),
        _ => options.audio_sample_rate.unwrap_or(source_rate),
    };
    requested.clamp(
        ConversionOptions::MIN_AUDIO_SAMPLE_RATE,
        ConversionOptions::MAX_AUDIO_SAMPLE_RATE,
    )
}

struct AudioProcessor {
    source_channels: usize,
    target_channels: usize,
    skip_frames: usize,
    limit_frames: Option<usize>,
    tail_padding: usize,
    frames_seen: usize,
    tail: Vec<f32>,
    resampler: Option<StreamingResampler>,
}

impl AudioProcessor {
    fn new(
        source_channels: usize,
        target_channels: usize,
        source_rate: u32,
        target_rate: u32,
        trim: AudioTrim,
    ) -> Self {
        let resampler = (source_rate != target_rate)
            .then(|| StreamingResampler::new(source_rate, target_rate, target_channels));
        Self {
            source_channels,
            target_channels,
            skip_frames: trim.skip_frames,
            limit_frames: trim.limit_frames,
            tail_padding: trim.tail_padding,
            frames_seen: 0,
            tail: Vec::new(),
            resampler,
        }
    }

    fn push(&mut self, samples: &[f32], channels: usize) -> Result<Vec<f32>, ConvertError> {
        if channels != self.source_channels {
            return Err(ConvertError::Audio(
                "the audio track changes its channel layout".to_owned(),
            ));
        }
        let frames = samples.len() / channels;
        let mut mixed = Vec::with_capacity(frames * self.target_channels);
        for frame_index in 0..frames {
            let absolute = self.frames_seen + frame_index;
            if absolute < self.skip_frames {
                continue;
            }
            if let Some(limit) = self.limit_frames {
                if absolute >= self.skip_frames.saturating_add(limit) {
                    break;
                }
            }
            let frame = &samples[frame_index * channels..(frame_index + 1) * channels];
            match self.target_channels {
                1 => mixed.push(downmix_mono(frame)),
                2 => {
                    let (left, right) = if channels == 1 {
                        (frame[0], frame[0])
                    } else {
                        downmix_stereo(frame)
                    };
                    mixed.push(left);
                    mixed.push(right);
                }
                _ => mixed.extend_from_slice(frame),
            }
        }
        self.frames_seen += frames;

        // A fixed worst-case attenuation would make a centre-only 5.1 tone
        // much quieter than its 0.707 downmix coefficient. Scale each chunk
        // only when it would clip instead, so normal content keeps its level
        // and hot content is attenuated linearly without wrapping.
        if self.target_channels <= 2 && self.source_channels != self.target_channels {
            limit_downmix_peak(&mut mixed);
        }

        self.tail.extend_from_slice(&mixed);
        let padding = self.tail_padding.saturating_mul(self.target_channels);
        if self.tail.len() <= padding {
            return Ok(Vec::new());
        }
        let mut emit = self.tail.len() - padding;
        emit -= emit % self.target_channels;
        let chunk: Vec<f32> = self.tail.drain(..emit).collect();
        let chunk = match self.resampler.as_mut() {
            Some(resampler) => resampler.push(&chunk),
            None => chunk,
        };
        Ok(clamp_stream_samples(chunk))
    }

    fn finish(&mut self) -> Result<Vec<f32>, ConvertError> {
        self.tail.clear();
        let chunk = match self.resampler.as_mut() {
            Some(resampler) => resampler.finish(),
            None => Vec::new(),
        };
        Ok(clamp_stream_samples(chunk))
    }
}

/// Keeps a downmixed chunk within full scale. A linear scale preserves the
/// relative level of every channel in the chunk, unlike clipping each peak.
fn limit_downmix_peak(samples: &mut [f32]) {
    for sample in samples.iter_mut() {
        if !sample.is_finite() {
            *sample = 0.0;
        }
    }
    let peak = samples
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    if peak > 1.0 {
        let scale = 1.0 / peak;
        for sample in samples {
            *sample *= scale;
        }
    }
}

/// The band-limited resampler can overshoot slightly, and a source may carry
/// out-of-range samples. Encoders should never see NaN, infinity or values
/// beyond full scale.
fn clamp_stream_samples(mut samples: Vec<f32>) -> Vec<f32> {
    for sample in &mut samples {
        *sample = if sample.is_finite() {
            sample.clamp(-1.0, 1.0)
        } else {
            0.0
        };
    }
    samples
}

struct StreamingResampler {
    channels: usize,
    ratio: f64,
    table: Vec<f64>,
    taps: usize,
    pending: Vec<f32>,
    pending_start: usize,
    received_frames: usize,
    next_output: usize,
}

impl StreamingResampler {
    fn new(source_rate: u32, target_rate: u32, channels: usize) -> Self {
        let ratio = f64::from(target_rate) / f64::from(source_rate);
        let cutoff = ratio.min(1.0) * 0.95;
        Self {
            channels,
            ratio,
            table: resample_kernel_table(cutoff),
            taps: RESAMPLE_ZERO_CROSSINGS * 2,
            pending: Vec::new(),
            pending_start: 0,
            received_frames: 0,
            next_output: 0,
        }
    }

    fn push(&mut self, samples: &[f32]) -> Vec<f32> {
        if self.channels == 0 {
            return Vec::new();
        }
        self.pending.extend_from_slice(samples);
        self.received_frames += samples.len() / self.channels;
        self.produce(false)
    }

    fn finish(&mut self) -> Vec<f32> {
        self.produce(true)
    }

    fn produce(&mut self, at_end: bool) -> Vec<f32> {
        if self.received_frames == 0 || self.channels == 0 {
            return Vec::new();
        }
        let target_frames = if at_end {
            ((self.received_frames as f64 * self.ratio).round() as usize).max(1)
        } else {
            usize::MAX
        };
        let available_end = self.pending_start + self.pending.len() / self.channels;
        let mut output = Vec::new();

        while self.next_output < target_frames {
            let position = self.next_output as f64 / self.ratio;
            let base = position.floor() as isize;
            let phase = (((position - base as f64) * RESAMPLE_PHASES as f64) as usize)
                .min(RESAMPLE_PHASES - 1);
            let last_needed = base + self.taps as isize - (RESAMPLE_ZERO_CROSSINGS as isize - 1);
            if !at_end && last_needed >= available_end as isize {
                break;
            }
            let weights = &self.table[phase * self.taps..phase * self.taps + self.taps];
            let mut mixed = vec![0.0_f64; self.channels];
            for (tap, weight) in weights.iter().enumerate() {
                let index = base + tap as isize - (RESAMPLE_ZERO_CROSSINGS as isize - 1);
                let index = index.clamp(0, self.received_frames.saturating_sub(1) as isize);
                let frame_start = (index as usize - self.pending_start) * self.channels;
                let frame = &self.pending[frame_start..frame_start + self.channels];
                for channel in 0..self.channels {
                    mixed[channel] += f64::from(frame[channel]) * weight;
                }
            }
            for value in mixed {
                output.push(value as f32);
            }
            self.next_output += 1;
        }

        let next_base = (self.next_output as f64 / self.ratio).floor() as isize;
        let keep_from = (next_base - (RESAMPLE_ZERO_CROSSINGS as isize - 1)).max(0) as usize;
        if keep_from > self.pending_start {
            let drop_frames =
                (keep_from - self.pending_start).min(self.pending.len() / self.channels);
            self.pending.drain(..drop_frames * self.channels);
            self.pending_start += drop_frames;
        }
        output
    }
}

trait StreamingEncoder {
    fn write(&mut self, samples: &[f32]) -> Result<(), ConvertError>;
    fn finish(self: Box<Self>) -> Result<(), ConvertError>;
}

fn create_encoder<'a>(
    target: OutputFormat,
    options: ConversionOptions,
    sample_rate: u32,
    channels: usize,
    source_bits: Option<u16>,
    writer: &'a mut dyn WriteSeek,
) -> Result<Box<dyn StreamingEncoder + 'a>, ConvertError> {
    Ok(match target {
        OutputFormat::Wav => Box::new(WavStreamEncoder::new(
            writer,
            sample_rate,
            channels,
            options.audio_bit_depth,
            source_bits,
        )?),
        OutputFormat::Flac => Box::new(FlacStreamEncoder::new(
            writer,
            sample_rate,
            channels,
            options.audio_bit_depth,
            source_bits,
        )?),
        OutputFormat::Aiff => Box::new(AiffStreamEncoder::new(writer, sample_rate, channels)?),
        OutputFormat::Au => Box::new(AuStreamEncoder::new(writer, sample_rate, channels)?),
        OutputFormat::Opus => Box::new(OpusStreamEncoder::new(writer, sample_rate, channels)?),
        OutputFormat::Mp3 => Box::new(Mp3StreamEncoder::new(
            writer,
            sample_rate,
            channels,
            options.mp3_bitrate,
        )?),
        _ => return Err(unsupported("audio", target)),
    })
}

struct WavStreamEncoder<'a> {
    writer: &'a mut dyn WriteSeek,
    channels: usize,
    bit_depth: AudioBitDepth,
    source_bits: Option<u16>,
    frames: u64,
    data_bytes: u64,
    dither: Dither,
    buffer: Vec<u8>,
    data_size_position: u64,
    fact_frames_position: Option<u64>,
}

impl<'a> WavStreamEncoder<'a> {
    fn new(
        writer: &'a mut dyn WriteSeek,
        sample_rate: u32,
        channels: usize,
        bit_depth: AudioBitDepth,
        source_bits: Option<u16>,
    ) -> Result<Self, ConvertError> {
        let channels_u16 = u16::try_from(channels).map_err(|_| ConvertError::AudioTooLarge)?;
        let bytes_per_sample = match bit_depth {
            AudioBitDepth::Bit16 => 2_u16,
            AudioBitDepth::Bit24 => 3,
            AudioBitDepth::Float32 => 4,
        };
        let block_align = channels_u16
            .checked_mul(bytes_per_sample)
            .ok_or(ConvertError::AudioTooLarge)?;
        let byte_rate = sample_rate
            .checked_mul(u32::from(block_align))
            .ok_or(ConvertError::AudioTooLarge)?;
        let is_float = bit_depth == AudioBitDepth::Float32;
        let fact_size = if is_float { 12_u32 } else { 0 };

        writer.seek(SeekFrom::Start(0))?;
        let mut header = Vec::with_capacity(56);
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&(36 + fact_size).to_le_bytes());
        header.extend_from_slice(b"WAVEfmt ");
        header.extend_from_slice(&16_u32.to_le_bytes());
        header.extend_from_slice(&if is_float { 3_u16 } else { 1 }.to_le_bytes());
        header.extend_from_slice(&channels_u16.to_le_bytes());
        header.extend_from_slice(&sample_rate.to_le_bytes());
        header.extend_from_slice(&byte_rate.to_le_bytes());
        header.extend_from_slice(&block_align.to_le_bytes());
        header.extend_from_slice(&bit_depth.bits().to_le_bytes());
        let fact_frames_position = if is_float {
            header.extend_from_slice(b"fact");
            header.extend_from_slice(&4_u32.to_le_bytes());
            let position = header.len() as u64;
            header.extend_from_slice(&0_u32.to_le_bytes());
            Some(position)
        } else {
            None
        };
        header.extend_from_slice(b"data");
        let data_size_position = header.len() as u64;
        header.extend_from_slice(&0_u32.to_le_bytes());
        writer.write_all(&header)?;

        Ok(Self {
            writer,
            channels,
            bit_depth,
            source_bits,
            frames: 0,
            data_bytes: 0,
            dither: Dither::new(),
            buffer: Vec::with_capacity(64 * 1024),
            data_size_position,
            fact_frames_position,
        })
    }

    fn flush_buffer(&mut self) -> Result<(), ConvertError> {
        if !self.buffer.is_empty() {
            self.writer.write_all(&self.buffer)?;
            self.buffer.clear();
        }
        Ok(())
    }
}

impl StreamingEncoder for WavStreamEncoder<'_> {
    fn write(&mut self, samples: &[f32]) -> Result<(), ConvertError> {
        if self.channels == 0 {
            return Err(ConvertError::NoAudioTrack);
        }
        let usable = samples.len() - samples.len() % self.channels;
        for sample in &samples[..usable] {
            match self.bit_depth {
                AudioBitDepth::Bit16 => self.buffer.extend_from_slice(
                    &self
                        .dither
                        .quantize_i16(*sample, self.source_bits)
                        .to_le_bytes(),
                ),
                AudioBitDepth::Bit24 => {
                    let value = self.dither.quantize_i32(*sample, 24, self.source_bits);
                    self.buffer.extend_from_slice(&value.to_le_bytes()[..3]);
                }
                AudioBitDepth::Float32 => self
                    .buffer
                    .extend_from_slice(&(*sample).clamp(-1.0, 1.0).to_le_bytes()),
            }
            if self.buffer.len() >= 32 * 1024 {
                self.flush_buffer()?;
            }
        }
        self.frames += (usable / self.channels) as u64;
        self.data_bytes += (usable * self.bit_depth.bits() as usize / 8) as u64;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<(), ConvertError> {
        self.flush_buffer()?;
        if self.data_bytes > u64::from(u32::MAX) || self.frames > u64::from(u32::MAX) {
            return Err(ConvertError::AudioTooLarge);
        }
        let fact_size = if self.fact_frames_position.is_some() {
            12_u64
        } else {
            0
        };
        let riff_size: u32 = (36_u64 + fact_size + self.data_bytes)
            .try_into()
            .map_err(|_| ConvertError::AudioTooLarge)?;
        self.writer.seek(SeekFrom::Start(4))?;
        self.writer.write_all(&riff_size.to_le_bytes())?;
        if let Some(position) = self.fact_frames_position {
            self.writer.seek(SeekFrom::Start(position))?;
            self.writer.write_all(&(self.frames as u32).to_le_bytes())?;
        }
        self.writer.seek(SeekFrom::Start(self.data_size_position))?;
        self.writer
            .write_all(&(self.data_bytes as u32).to_le_bytes())?;
        self.writer.seek(SeekFrom::End(0))?;
        self.writer.flush()?;
        Ok(())
    }
}

struct FlacStreamEncoder<'a> {
    writer: &'a mut dyn WriteSeek,
    config: flacenc::error::Verified<flacenc::config::Encoder>,
    stream_info: flacenc::component::StreamInfo,
    frame_buffer: flacenc::source::FrameBuf,
    block_size: usize,
    channels: usize,
    bits: u32,
    source_bits: Option<u16>,
    frame_number: usize,
    frames: u64,
    pending: Vec<f32>,
    integer_samples: Vec<i32>,
    dither: Dither,
}

impl<'a> FlacStreamEncoder<'a> {
    fn new(
        writer: &'a mut dyn WriteSeek,
        sample_rate: u32,
        channels: usize,
        bit_depth: AudioBitDepth,
        source_bits: Option<u16>,
    ) -> Result<Self, ConvertError> {
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
        let config = flacenc::config::Encoder::default()
            .into_verified()
            .map_err(|(_, error)| ConvertError::Audio(error.to_string()))?;
        let block_size = config.block_size;
        let mut stream_info =
            flacenc::component::StreamInfo::new(sample_rate as usize, channels, bits as usize)
                .map_err(|error| ConvertError::Audio(error.to_string()))?;
        stream_info
            .set_block_sizes(block_size, block_size)
            .map_err(|error| ConvertError::Audio(error.to_string()))?;
        let stream = flacenc::component::Stream::with_stream_info(stream_info.clone());
        let mut sink = flacenc::bitsink::ByteSink::new();
        stream
            .write(&mut sink)
            .map_err(|error| ConvertError::Audio(error.to_string()))?;
        writer.write_all(sink.as_slice())?;

        Ok(Self {
            writer,
            config,
            stream_info,
            frame_buffer: flacenc::source::FrameBuf::with_size(channels, block_size)
                .map_err(|error| ConvertError::Audio(error.to_string()))?,
            block_size,
            channels,
            bits,
            source_bits,
            frame_number: 0,
            frames: 0,
            pending: Vec::with_capacity(block_size * channels),
            integer_samples: Vec::with_capacity(block_size * channels),
            dither: Dither::new(),
        })
    }

    fn encode_block(&mut self, samples: &[f32]) -> Result<(), ConvertError> {
        use flacenc::component::BitRepr;
        use flacenc::source::Fill;

        self.integer_samples.clear();
        self.integer_samples.reserve(samples.len());
        for sample in samples {
            if self.bits == 16 {
                self.integer_samples.push(i32::from(
                    self.dither.quantize_i16(*sample, self.source_bits),
                ));
            } else {
                self.integer_samples
                    .push(self.dither.quantize_i32(*sample, 24, self.source_bits));
            }
        }
        self.frame_buffer
            .fill_interleaved(&self.integer_samples)
            .map_err(|error| ConvertError::Audio(error.to_string()))?;
        let frame = flacenc::encode_fixed_size_frame(
            &self.config,
            &self.frame_buffer,
            self.frame_number,
            &self.stream_info,
        )
        .map_err(|error| ConvertError::Audio(error.to_string()))?;
        self.frame_number += 1;
        let mut sink = flacenc::bitsink::ByteSink::new();
        frame
            .write(&mut sink)
            .map_err(|error| ConvertError::Audio(error.to_string()))?;
        self.writer.write_all(sink.as_slice())?;
        Ok(())
    }
}

impl StreamingEncoder for FlacStreamEncoder<'_> {
    fn write(&mut self, samples: &[f32]) -> Result<(), ConvertError> {
        if self.channels == 0 {
            return Err(ConvertError::NoAudioTrack);
        }
        let usable = samples.len() - samples.len() % self.channels;
        self.frames += (usable / self.channels) as u64;
        self.pending.extend_from_slice(&samples[..usable]);
        let block_samples = self.block_size * self.channels;
        while self.pending.len() >= block_samples {
            let block: Vec<f32> = self.pending.drain(..block_samples).collect();
            self.encode_block(&block)?;
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<(), ConvertError> {
        if !self.pending.is_empty() {
            let block = std::mem::take(&mut self.pending);
            self.encode_block(&block)?;
        }
        // STREAMINFO starts after the four-byte marker and the four-byte
        // metadata header. Its last eight bytes hold the sample rate,
        // channels, bit depth and a 36-bit total-sample count. The count was
        // unknown when the header was written, so patch it now that the
        // stream is complete.
        if self.frames < (1_u64 << 36) {
            let value = (self.stream_info.sample_rate() as u64) << 44
                | ((self.stream_info.channels() as u64 - 1) << 41)
                | ((self.stream_info.bits_per_sample() as u64 - 1) << 36)
                | self.frames;
            self.writer.seek(SeekFrom::Start(18))?;
            self.writer.write_all(&value.to_be_bytes())?;
            self.writer.seek(SeekFrom::End(0))?;
        }
        self.writer.flush()?;
        Ok(())
    }
}

struct AiffStreamEncoder<'a> {
    writer: &'a mut dyn WriteSeek,
    channels: usize,
    frames: u64,
    data_bytes: u64,
    buffer: Vec<u8>,
}

impl<'a> AiffStreamEncoder<'a> {
    fn new(
        writer: &'a mut dyn WriteSeek,
        sample_rate: u32,
        channels: usize,
    ) -> Result<Self, ConvertError> {
        let channels_u16 = u16::try_from(channels).map_err(|_| ConvertError::AudioTooLarge)?;
        writer.seek(SeekFrom::Start(0))?;
        let mut header = Vec::with_capacity(54);
        header.extend_from_slice(b"FORM");
        header.extend_from_slice(&46_u32.to_be_bytes());
        header.extend_from_slice(b"AIFF");
        header.extend_from_slice(b"COMM");
        header.extend_from_slice(&18_u32.to_be_bytes());
        header.extend_from_slice(&channels_u16.to_be_bytes());
        header.extend_from_slice(&0_u32.to_be_bytes());
        header.extend_from_slice(&16_u16.to_be_bytes());
        header.extend_from_slice(&extended_sample_rate(sample_rate));
        header.extend_from_slice(b"SSND");
        header.extend_from_slice(&8_u32.to_be_bytes());
        header.extend_from_slice(&0_u32.to_be_bytes());
        header.extend_from_slice(&0_u32.to_be_bytes());
        writer.write_all(&header)?;
        Ok(Self {
            writer,
            channels,
            frames: 0,
            data_bytes: 0,
            buffer: Vec::with_capacity(64 * 1024),
        })
    }
}

impl StreamingEncoder for AiffStreamEncoder<'_> {
    fn write(&mut self, samples: &[f32]) -> Result<(), ConvertError> {
        if self.channels == 0 {
            return Err(ConvertError::NoAudioTrack);
        }
        let usable = samples.len() - samples.len() % self.channels;
        for sample in &samples[..usable] {
            self.buffer
                .extend_from_slice(&sample_to_i16(*sample).to_be_bytes());
            if self.buffer.len() >= 32 * 1024 {
                self.writer.write_all(&self.buffer)?;
                self.buffer.clear();
            }
        }
        self.frames += (usable / self.channels) as u64;
        self.data_bytes += (usable * 2) as u64;
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(), ConvertError> {
        if !self.buffer.is_empty() {
            self.writer.write_all(&self.buffer)?;
        }
        if self.data_bytes > u64::from(u32::MAX) || self.frames > u64::from(u32::MAX) {
            return Err(ConvertError::AudioTooLarge);
        }
        self.writer.seek(SeekFrom::Start(4))?;
        self.writer
            .write_all(&(46_u32 + self.data_bytes as u32).to_be_bytes())?;
        self.writer.seek(SeekFrom::Start(22))?;
        self.writer.write_all(&(self.frames as u32).to_be_bytes())?;
        self.writer.seek(SeekFrom::Start(42))?;
        self.writer
            .write_all(&(8_u32 + self.data_bytes as u32).to_be_bytes())?;
        self.writer.seek(SeekFrom::End(0))?;
        self.writer.flush()?;
        Ok(())
    }
}

struct AuStreamEncoder<'a> {
    writer: &'a mut dyn WriteSeek,
    channels: usize,
    data_bytes: u64,
    buffer: Vec<u8>,
}

impl<'a> AuStreamEncoder<'a> {
    fn new(
        writer: &'a mut dyn WriteSeek,
        sample_rate: u32,
        channels: usize,
    ) -> Result<Self, ConvertError> {
        let channels_u32 = u32::try_from(channels).map_err(|_| ConvertError::AudioTooLarge)?;
        writer.seek(SeekFrom::Start(0))?;
        let mut header = Vec::with_capacity(24);
        header.extend_from_slice(b".snd");
        header.extend_from_slice(&24_u32.to_be_bytes());
        header.extend_from_slice(&0_u32.to_be_bytes());
        header.extend_from_slice(&3_u32.to_be_bytes());
        header.extend_from_slice(&sample_rate.to_be_bytes());
        header.extend_from_slice(&channels_u32.to_be_bytes());
        writer.write_all(&header)?;
        Ok(Self {
            writer,
            channels,
            data_bytes: 0,
            buffer: Vec::with_capacity(64 * 1024),
        })
    }
}

impl StreamingEncoder for AuStreamEncoder<'_> {
    fn write(&mut self, samples: &[f32]) -> Result<(), ConvertError> {
        if self.channels == 0 {
            return Err(ConvertError::NoAudioTrack);
        }
        let usable = samples.len() - samples.len() % self.channels;
        for sample in &samples[..usable] {
            self.buffer
                .extend_from_slice(&sample_to_i16(*sample).to_be_bytes());
            if self.buffer.len() >= 32 * 1024 {
                self.writer.write_all(&self.buffer)?;
                self.buffer.clear();
            }
        }
        self.data_bytes += (usable * 2) as u64;
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<(), ConvertError> {
        if !self.buffer.is_empty() {
            self.writer.write_all(&self.buffer)?;
        }
        if self.data_bytes > u64::from(u32::MAX) {
            return Err(ConvertError::AudioTooLarge);
        }
        self.writer.seek(SeekFrom::Start(8))?;
        self.writer
            .write_all(&(self.data_bytes as u32).to_be_bytes())?;
        self.writer.seek(SeekFrom::End(0))?;
        self.writer.flush()?;
        Ok(())
    }
}

struct Mp3StreamEncoder<'a> {
    writer: &'a mut dyn WriteSeek,
    encoder: mp3lame_encoder::Encoder,
    channels: usize,
    buffer: Vec<u8>,
}

impl<'a> Mp3StreamEncoder<'a> {
    fn new(
        writer: &'a mut dyn WriteSeek,
        sample_rate: u32,
        channels: usize,
        bitrate: Mp3Bitrate,
    ) -> Result<Self, ConvertError> {
        use mp3lame_encoder::{Builder, Quality};

        if channels == 0 {
            return Err(ConvertError::NoAudioTrack);
        }
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
        let encoder = builder.build().map_err(|error| {
            ConvertError::Audio(format!("could not start the MP3 encoder: {error}"))
        })?;
        Ok(Self {
            writer,
            encoder,
            channels,
            buffer: Vec::new(),
        })
    }
}

impl StreamingEncoder for Mp3StreamEncoder<'_> {
    fn write(&mut self, samples: &[f32]) -> Result<(), ConvertError> {
        use mp3lame_encoder::{InterleavedPcm, MonoPcm};

        if samples.is_empty() {
            return Ok(());
        }
        self.buffer.clear();
        self.buffer
            .reserve(mp3lame_encoder::max_required_buffer_size(samples.len()));
        if self.channels == 1 {
            self.encoder
                .encode_to_vec(MonoPcm(samples), &mut self.buffer)
                .map_err(|error| {
                    ConvertError::Audio(format!("could not encode MP3 audio: {error}"))
                })?;
        } else {
            self.encoder
                .encode_to_vec(InterleavedPcm(samples), &mut self.buffer)
                .map_err(|error| {
                    ConvertError::Audio(format!("could not encode MP3 audio: {error}"))
                })?;
        }
        self.writer.write_all(&self.buffer)?;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<(), ConvertError> {
        use mp3lame_encoder::FlushNoGap;

        self.buffer.clear();
        self.buffer.reserve(7_200);
        self.encoder
            .flush_to_vec::<FlushNoGap>(&mut self.buffer)
            .map_err(|error| {
                ConvertError::Audio(format!("could not finish the MP3 stream: {error}"))
            })?;
        self.writer.write_all(&self.buffer)?;
        self.writer.flush()?;
        Ok(())
    }
}

struct OpusStreamEncoder<'a> {
    writer: Option<opus_pure::OggOpusWriter<&'a mut dyn WriteSeek>>,
    encoder: OpusPcmEncoder,
    channels: usize,
    sample_rate: u32,
    frame_size: usize,
    frame_samples: usize,
    pre_skip: u64,
    flush_samples: usize,
    input_frames: usize,
    pending: Vec<f32>,
    held: Option<Vec<f32>>,
    packet: Vec<u8>,
}

impl<'a> OpusStreamEncoder<'a> {
    fn new(
        writer: &'a mut dyn WriteSeek,
        sample_rate: u32,
        channels: usize,
    ) -> Result<Self, ConvertError> {
        use opus_pure::OggOpusWriter;

        let encoder = OpusPcmEncoder::new(sample_rate, channels)?;
        let head = encoder.head(sample_rate);
        let pre_skip = u64::from(head.pre_skip);
        let writer = OggOpusWriter::new(writer, head).map_err(|error| {
            ConvertError::Audio(format!("could not start the Opus stream: {error}"))
        })?;
        let frame_size = (sample_rate / 50) as usize;
        let ticks = 48_000 / sample_rate;
        let flush_samples = ((pre_skip + u64::from(ticks) - 1) / u64::from(ticks)) as usize;
        Ok(Self {
            writer: Some(writer),
            encoder,
            channels,
            sample_rate,
            frame_size,
            frame_samples: frame_size * channels,
            pre_skip,
            flush_samples,
            input_frames: 0,
            pending: Vec::with_capacity(frame_size * channels),
            held: None,
            packet: vec![0_u8; opus_pure::MAX_PACKET_BYTES],
        })
    }

    fn feed(&mut self, samples: &[f32], count_input: bool) -> Result<(), ConvertError> {
        if self.channels == 0 {
            return Err(ConvertError::NoAudioTrack);
        }
        let mut offset = 0;
        while offset < samples.len() {
            let take = (self.frame_samples - self.pending.len()).min(samples.len() - offset);
            self.pending
                .extend_from_slice(&samples[offset..offset + take]);
            offset += take;
            if self.pending.len() == self.frame_samples {
                let frame = std::mem::take(&mut self.pending);
                if let Some(previous) = self.held.replace(frame) {
                    self.write_frame(&previous, false)?;
                }
            }
        }
        if count_input {
            self.input_frames += samples.len() / self.channels;
        }
        Ok(())
    }

    fn write_frame(&mut self, frame: &[f32], final_frame: bool) -> Result<(), ConvertError> {
        // Opus multistream packets use the Vorbis channel order. The rest of
        // the pipeline, including the downmixer, uses film/WAV order.
        let mut reordered;
        let frame = if self.channels > 2 {
            reordered = frame.to_vec();
            reorder_film_to_opus_channels(&mut reordered, self.channels);
            reordered.as_slice()
        } else {
            frame
        };
        let len = self
            .encoder
            .encode(frame, self.frame_size, &mut self.packet)
            .map_err(|error| {
                ConvertError::Audio(format!("could not encode Opus audio: {error}"))
            })?;
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| ConvertError::Audio("the Opus stream is closed".to_owned()))?;
        if final_frame {
            let final_granule =
                self.pre_skip + self.input_frames as u64 * 48_000 / u64::from(self.sample_rate);
            let duration = final_granule.saturating_sub(writer.granule() as u64) as u32;
            writer
                .write_packet_with_duration(&self.packet[..len], duration)
                .map_err(|error| {
                    ConvertError::Audio(format!("could not write the Opus stream: {error}"))
                })?;
        } else {
            writer.write_packet(&self.packet[..len]).map_err(|error| {
                ConvertError::Audio(format!("could not write the Opus stream: {error}"))
            })?;
        }
        Ok(())
    }
}

impl StreamingEncoder for OpusStreamEncoder<'_> {
    fn write(&mut self, samples: &[f32]) -> Result<(), ConvertError> {
        self.feed(samples, true)
    }

    fn finish(mut self: Box<Self>) -> Result<(), ConvertError> {
        if self.input_frames == 0 {
            return Err(ConvertError::NoAudioTrack);
        }
        let mut remaining = self.flush_samples;
        while remaining > 0 {
            let take = remaining.min(self.frame_size);
            let zeros = vec![0.0; take * self.channels];
            self.feed(&zeros, false)?;
            remaining -= take;
        }
        if !self.pending.is_empty() {
            let mut last = std::mem::take(&mut self.pending);
            last.resize(self.frame_samples, 0.0);
            if let Some(previous) = self.held.replace(last) {
                self.write_frame(&previous, false)?;
            }
        }
        let Some(mut last) = self.held.take() else {
            return Err(ConvertError::NoAudioTrack);
        };
        last.resize(self.frame_samples, 0.0);
        self.write_frame(&last, true)?;
        let writer = self
            .writer
            .take()
            .ok_or_else(|| ConvertError::Audio("the Opus stream is closed".to_owned()))?;
        let sink = writer.finish().map_err(|error| {
            ConvertError::Audio(format!("could not finish the Opus stream: {error}"))
        })?;
        sink.flush()?;
        Ok(())
    }
}
