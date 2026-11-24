// SPDX-License-Identifier: GPL-2.0-or-later

//! Stream-copy remuxing, thumbnails and video route validation.

use crate::container::{
    ebml_element_end, is_mp4_extension, mp4_child_boxes, mp4_find, read_ebml_element,
    read_ebml_uint, skip_ebml_element,
};
use crate::encode_image;
use crate::error::ConvertError;
use crate::ffmpeg::{ffmpeg_command, find_on_path, resolve_ffmpeg, run_ffmpeg, TempVideoPath};
use crate::format::OutputFormat;
use crate::options::ConversionOptions;
use crate::shared::{file_extension, output_name};
use crate::ConvertedFile;
use std::{path::PathBuf, sync::atomic::AtomicBool, time::Duration};

/// One progress update from a future video transcode path. `total` is `None`
/// when ffmpeg cannot determine the input duration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoProgress {
    pub out_time: Duration,
    pub total: Option<Duration>,
}

impl VideoProgress {
    pub fn fraction(self) -> Option<f32> {
        let total = self.total?.as_secs_f64();
        if total <= 0.0 {
            return None;
        }
        Some((self.out_time.as_secs_f64() / total).clamp(0.0, 1.0) as f32)
    }
}

/// Cancellation and progress hooks reserved for a future ffmpeg transcode
/// path. This slice only performs stream-copy remuxes and thumbnails, which
/// are synchronous and ignore these hooks.
pub struct VideoControl<'a> {
    pub cancel: &'a AtomicBool,
    pub on_progress: &'a mut dyn FnMut(VideoProgress),
}

/// Location of the external ffmpeg binary. An explicit path wins, then the
/// `CONVERT2IT_FFMPEG` / `FFMPEG` environment variables, then PATH.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VideoToolConfig {
    pub ffmpeg: Option<PathBuf>,
}

impl VideoToolConfig {
    pub fn with_ffmpeg(path: impl Into<PathBuf>) -> Self {
        Self {
            ffmpeg: Some(path.into()),
        }
    }

    /// Resolves the ffmpeg executable using the override chain above.
    pub fn resolve(&self) -> Option<PathBuf> {
        if let Some(path) = &self.ffmpeg {
            return Some(path.clone());
        }
        for variable in ["CONVERT2IT_FFMPEG", "FFMPEG"] {
            if let Some(path) = std::env::var_os(variable) {
                if !path.is_empty() {
                    return Some(PathBuf::from(path));
                }
            }
        }
        find_on_path("ffmpeg")
    }
}

/// The automatic route selected for a video conversion.
///
/// This is deliberately not a user-facing mode. The current video slice only
/// advertises stream-copy remuxes; the transcode variant is reserved for a
/// future slice and is never selected by the conversion entry points.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoRoute {
    Remux,
    Transcode,
}

impl VideoRoute {
    pub fn label(self) -> &'static str {
        match self {
            Self::Remux => "remux",
            Self::Transcode => "transcode",
        }
    }
}

/// One advertised video route. The same [`VideoRoute`] enum is used by the
/// registry and the CLI listing so they cannot disagree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoRouteInfo {
    pub source_extension: &'static str,
    pub target: OutputFormat,
    pub route: VideoRoute,
    pub video_codecs: &'static [&'static str],
    pub audio_codecs: &'static [&'static str],
}

pub(crate) const REMUX_VIDEO_TO_MP4: &[&str] = &["h264", "hevc", "vp9", "av1"];

pub(crate) const REMUX_AUDIO_TO_MP4: &[&str] = &["aac", "opus", "mp3", "ac3", "eac3"];

pub(crate) const REMUX_VIDEO_TO_WEBM: &[&str] = &["vp8", "vp9", "av1"];

pub(crate) const REMUX_AUDIO_TO_WEBM: &[&str] = &["opus", "vorbis"];

/// Routes advertised by the native registry. Every route in this slice is a
/// stream copy; incompatible codec/container pairs are rejected rather than
/// transcoded.
pub fn native_video_routes() -> Vec<VideoRouteInfo> {
    vec![
        VideoRouteInfo {
            source_extension: "mp4",
            target: OutputFormat::Webm,
            route: VideoRoute::Remux,
            video_codecs: REMUX_VIDEO_TO_WEBM,
            audio_codecs: REMUX_AUDIO_TO_WEBM,
        },
        VideoRouteInfo {
            source_extension: "mkv",
            target: OutputFormat::Webm,
            route: VideoRoute::Remux,
            video_codecs: REMUX_VIDEO_TO_WEBM,
            audio_codecs: REMUX_AUDIO_TO_WEBM,
        },
        VideoRouteInfo {
            source_extension: "webm",
            target: OutputFormat::Mp4,
            route: VideoRoute::Remux,
            video_codecs: REMUX_VIDEO_TO_MP4,
            audio_codecs: REMUX_AUDIO_TO_MP4,
        },
        VideoRouteInfo {
            source_extension: "mkv",
            target: OutputFormat::Mp4,
            route: VideoRoute::Remux,
            video_codecs: REMUX_VIDEO_TO_MP4,
            audio_codecs: REMUX_AUDIO_TO_MP4,
        },
    ]
}

pub(crate) fn convert_video(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    video: &VideoToolConfig,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    remux_video(input_name, input, target, video)
}

pub(crate) fn remux_video(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    video: &VideoToolConfig,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    let extension = file_extension(input_name);
    validate_remux(input, &extension, target)?;
    let ffmpeg = resolve_ffmpeg(video)?;
    let input_path = TempVideoPath::new(if extension.is_empty() {
        "bin"
    } else {
        &extension
    });
    std::fs::write(input_path.path(), input)?;
    let output_path = TempVideoPath::new(target.extension());

    let mut command = ffmpeg_command(&ffmpeg);
    command
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(input_path.path())
        .args(["-map", "0:v?", "-map", "0:a?", "-c", "copy"]);
    match target {
        OutputFormat::Mp4 => {
            command.args(["-movflags", "+faststart", "-f", "mp4"]);
        }
        OutputFormat::Webm => {
            command.args(["-f", "webm"]);
        }
        _ => unreachable!("only video containers reach remux_video"),
    }
    command.arg(output_path.path());
    run_ffmpeg(&mut command, &ffmpeg)?;

    let bytes = std::fs::read(output_path.path()).map_err(|_| ConvertError::FfmpegEmptyOutput)?;
    if bytes.is_empty() {
        return Err(ConvertError::FfmpegEmptyOutput);
    }
    let valid_container = match target {
        OutputFormat::Mp4 => bytes.get(4..8) == Some(b"ftyp"),
        OutputFormat::Webm => bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]),
        _ => true,
    };
    if !valid_container {
        return Err(ConvertError::FfmpegInvalidOutput {
            container: target.extension(),
        });
    }
    Ok(vec![ConvertedFile {
        name: output_name(input_name, target.extension()),
        bytes,
    }])
}

pub(crate) fn extract_video_frame(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    options: ConversionOptions,
    video: &VideoToolConfig,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    let ffmpeg = resolve_ffmpeg(video)?;
    let extension = file_extension(input_name);
    let input_path = TempVideoPath::new(if extension.is_empty() {
        "bin"
    } else {
        &extension
    });
    std::fs::write(input_path.path(), input)?;

    let frame_time_ms = options
        .video_frame_time_ms
        .min(ConversionOptions::MAX_VIDEO_FRAME_TIME_MS);
    let mut command = ffmpeg_command(&ffmpeg);
    command
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-ss"])
        .arg(format!("{:.3}", frame_time_ms as f64 / 1_000.0))
        .arg("-i")
        .arg(input_path.path())
        .args(["-map", "0:v:0", "-frames:v", "1", "-an"]);
    let width = options
        .video_thumbnail_width
        .min(ConversionOptions::MAX_VIDEO_THUMBNAIL_WIDTH);
    if width > 0 {
        command.args(["-vf", &format!("scale={}:-2:flags=lanczos", width.max(16))]);
    }
    command.args(["-f", "image2pipe", "-vcodec", "png", "pipe:1"]);

    let output = run_ffmpeg(&mut command, &ffmpeg)?;
    if output.stdout.is_empty() {
        return Err(ConvertError::FfmpegEmptyOutput);
    }
    let image = image::load_from_memory(&output.stdout)?;
    let bytes = encode_image(image, target, options)?;
    Ok(vec![ConvertedFile {
        name: output_name(input_name, target.extension()),
        bytes,
    }])
}

#[derive(Default)]
pub(crate) struct VideoCodecs {
    pub(crate) video: Vec<String>,
    pub(crate) audio: Vec<String>,
    pub(crate) unknown_video: bool,
    pub(crate) unknown_audio: bool,
}

pub(crate) fn validate_remux(
    input: &[u8],
    extension: &str,
    target: OutputFormat,
) -> Result<(), ConvertError> {
    let codecs = if is_mp4_extension(extension) {
        mp4_video_codecs(input)
    } else if matches!(extension, "mkv" | "webm") {
        matroska_video_codecs(input)
    } else {
        None
    };
    let Some(codecs) = codecs else {
        return Ok(());
    };
    let (video_allowed, audio_allowed): (&[&str], &[&str]) = match target {
        OutputFormat::Mp4 => (REMUX_VIDEO_TO_MP4, REMUX_AUDIO_TO_MP4),
        OutputFormat::Webm => (REMUX_VIDEO_TO_WEBM, REMUX_AUDIO_TO_WEBM),
        _ => return Ok(()),
    };
    for codec in &codecs.video {
        if !video_allowed.contains(&codec.as_str()) {
            return Err(ConvertError::UnsupportedVideoCodec {
                stream: "video",
                codec: codec.clone(),
                container: target.extension(),
            });
        }
    }
    for codec in &codecs.audio {
        if !audio_allowed.contains(&codec.as_str()) {
            return Err(ConvertError::UnsupportedVideoCodec {
                stream: "audio",
                codec: codec.clone(),
                container: target.extension(),
            });
        }
    }
    Ok(())
}

pub(crate) fn mp4_video_codecs(input: &[u8]) -> Option<VideoCodecs> {
    let moov = mp4_find(input, b"moov")?;
    let mut codecs = VideoCodecs::default();
    for (kind, trak) in mp4_child_boxes(moov) {
        if kind != *b"trak" {
            continue;
        }
        let mdia = match mp4_find(trak, b"mdia") {
            Some(mdia) => mdia,
            None => continue,
        };
        let handler = mp4_find(mdia, b"hdlr")
            .and_then(|hdlr| hdlr.get(8..12))
            .unwrap_or(b"");
        let stsd = mp4_find(mdia, b"minf")
            .and_then(|minf| mp4_find(minf, b"stbl"))
            .and_then(|stbl| mp4_find(stbl, b"stsd"));
        let Some(stsd) = stsd else {
            continue;
        };
        let entries = stsd
            .get(4..8)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_be_bytes)
            .unwrap_or(0) as usize;
        let mut position = 8;
        for _ in 0..entries {
            let Some(size_bytes) = stsd.get(position..position + 4) else {
                break;
            };
            let size = u32::from_be_bytes(size_bytes.try_into().ok()?) as usize;
            if size < 8 || position + size > stsd.len() {
                break;
            }
            let format = stsd.get(position + 4..position + 8)?;
            match handler {
                b"vide" => match mp4_codec_name(format) {
                    Some(codec) => codecs.video.push(codec.to_owned()),
                    None => codecs.unknown_video = true,
                },
                b"soun" => match mp4_codec_name(format) {
                    Some(codec) => codecs.audio.push(codec.to_owned()),
                    None => codecs.unknown_audio = true,
                },
                _ => {}
            }
            position += size;
        }
    }
    Some(codecs)
}

pub(crate) fn mp4_codec_name(format: &[u8]) -> Option<&'static str> {
    match format {
        b"avc1" | b"avc3" => Some("h264"),
        b"hvc1" | b"hev1" => Some("hevc"),
        b"vp08" => Some("vp8"),
        b"vp09" => Some("vp9"),
        b"av01" => Some("av1"),
        b"mp4a" => Some("aac"),
        b"Opus" => Some("opus"),
        b"ac-3" => Some("ac3"),
        b"ec-3" => Some("eac3"),
        b"mp3 " => Some("mp3"),
        b"vorb" => Some("vorbis"),
        _ => None,
    }
}

pub(crate) fn matroska_video_codecs(input: &[u8]) -> Option<VideoCodecs> {
    let mut position = 0;
    let (segment_start, segment_end) = loop {
        let (id, size) = read_ebml_element(input, &mut position)?;
        if id == 0x1853_8067 {
            let end = ebml_element_end(position, size, input.len());
            break (position, end);
        }
        skip_ebml_element(input, &mut position, size)?;
    };
    let mut codecs = VideoCodecs::default();
    let mut scan = segment_start;
    while scan < segment_end {
        let (id, size) = read_ebml_element(input, &mut scan)?;
        let end = ebml_element_end(scan, size, segment_end);
        if id == 0x1654_AE6B {
            parse_matroska_tracks(&input[scan..end], &mut codecs);
        }
        scan = end;
    }
    Some(codecs)
}

pub(crate) fn parse_matroska_tracks(data: &[u8], codecs: &mut VideoCodecs) {
    let mut position = 0;
    while position < data.len() {
        let Some((id, size)) = read_ebml_element(data, &mut position) else {
            break;
        };
        let end = ebml_element_end(position, size, data.len());
        if id == 0xAE {
            parse_matroska_track(&data[position..end], codecs);
        }
        position = end;
    }
}

pub(crate) fn parse_matroska_track(data: &[u8], codecs: &mut VideoCodecs) {
    let mut track_type = None;
    let mut codec_id = None;
    let mut position = 0;
    while position < data.len() {
        let Some((id, size)) = read_ebml_element(data, &mut position) else {
            break;
        };
        let end = ebml_element_end(position, size, data.len());
        match id {
            0x83 => track_type = size.and_then(|size| read_ebml_uint(data, position, size)),
            0x86 => {
                codec_id = size.and_then(|size| {
                    data.get(position..position + size as usize)
                        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                })
            }
            _ => {}
        }
        position = end;
    }
    let Some(codec) = codec_id.as_deref().and_then(matroska_codec_name) else {
        match track_type {
            Some(1) => codecs.unknown_video = true,
            Some(2) => codecs.unknown_audio = true,
            _ => {}
        }
        return;
    };
    match track_type {
        Some(1) => codecs.video.push(codec.to_owned()),
        Some(2) => codecs.audio.push(codec.to_owned()),
        _ => {}
    }
}

pub(crate) fn matroska_codec_name(codec: &str) -> Option<&'static str> {
    match codec {
        "V_VP8" => Some("vp8"),
        "V_VP9" => Some("vp9"),
        "V_AV1" => Some("av1"),
        "V_MPEG4/ISO/AVC" => Some("h264"),
        "V_MPEGH/ISO/HEVC" => Some("hevc"),
        "A_OPUS" => Some("opus"),
        "A_VORBIS" => Some("vorbis"),
        "A_AAC" => Some("aac"),
        "A_MPEG/L3" => Some("mp3"),
        "A_AC3" => Some("ac3"),
        "A_EAC3" => Some("eac3"),
        _ => None,
    }
}
