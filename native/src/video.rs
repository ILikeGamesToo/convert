// SPDX-License-Identifier: GPL-2.0-or-later

//! Video conversion: stream-copy remuxing where the container can take the
//! source codecs, ffmpeg re-encoding where it cannot, thumbnails and route
//! validation.

use crate::container::{
    ebml_element_end, is_mp4_extension, mp4_child_boxes, mp4_find, read_ebml_element,
    read_ebml_uint, skip_ebml_element,
};
use crate::convert::ConvertedFile;
use crate::error::ConvertError;
use crate::ffmpeg::{
    ffmpeg_command, find_on_path, resolve_ffmpeg, run_ffmpeg, run_ffmpeg_tracked, TempVideoPath,
};
use crate::format::OutputFormat;
use crate::images::encode_image;
use crate::options::ConversionOptions;
use crate::shared::{file_extension, output_name};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

/// One progress update from the ffmpeg encode path. `total` stays `None`
/// because the encode is told not to report a duration: reading it would mean
/// running ffmpeg at `info` verbosity, which buries the error line in a failed
/// encode under stream and format banner lines.
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

/// Cancellation and progress hooks for the ffmpeg encode path. A conversion
/// that passes one gets `on_progress` called with each `out_time_us` ffmpeg
/// reports, and the child process is killed as soon as `cancel` is set.
/// Stream-copy remuxes and thumbnails are single fast invocations and ignore
/// the hooks.
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
/// This is deliberately not a user-facing mode: the conversion picks a route
/// from the source codecs alone. `Remux` copies the streams, `Transcode` asks
/// ffmpeg to encode them into the target container's codecs.
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
    /// Codecs the target container takes without re-encoding. Empty when the
    /// route always encodes.
    pub video_codecs: &'static [&'static str],
    pub audio_codecs: &'static [&'static str],
    /// The route taken when the source codecs are outside the lists above.
    /// `None` when there is nothing better to fall back to.
    pub fallback: Option<VideoRoute>,
}

pub(crate) const REMUX_VIDEO_TO_MP4: &[&str] = &["h264", "hevc", "vp9", "av1"];

pub(crate) const REMUX_AUDIO_TO_MP4: &[&str] = &["aac", "opus", "mp3", "ac3", "eac3"];

pub(crate) const REMUX_VIDEO_TO_WEBM: &[&str] = &["vp8", "vp9", "av1"];

pub(crate) const REMUX_AUDIO_TO_WEBM: &[&str] = &["opus", "vorbis"];

/// Containers whose codecs are read in-process, so a compatible pair can be
/// stream-copied without starting ffmpeg.
const PARSED_VIDEO_CONTAINERS: &[&str] = &["mp4", "m4v", "mov", "mkv", "webm"];

/// Containers ffmpeg alone reads. Every conversion from one of these encodes,
/// because the native parser cannot prove that a stream copy would be legal.
const ENCODED_VIDEO_CONTAINERS: &[&str] = &["avi", "wmv", "mpeg", "mpg"];

fn remux_codecs(
    target: OutputFormat,
) -> Option<(&'static [&'static str], &'static [&'static str])> {
    match target {
        OutputFormat::Mp4 => Some((REMUX_VIDEO_TO_MP4, REMUX_AUDIO_TO_MP4)),
        OutputFormat::Webm => Some((REMUX_VIDEO_TO_WEBM, REMUX_AUDIO_TO_WEBM)),
        _ => None,
    }
}

/// Routes advertised by the native registry. A parsed container whose codecs
/// the target can hold is stream-copied and re-encoded otherwise; every other
/// video container and every GIF output is encoded outright.
pub fn native_video_routes() -> Vec<VideoRouteInfo> {
    let mut routes = Vec::new();
    for source_extension in PARSED_VIDEO_CONTAINERS
        .iter()
        .chain(ENCODED_VIDEO_CONTAINERS)
    {
        let parsed = PARSED_VIDEO_CONTAINERS.contains(source_extension);
        for target in [OutputFormat::Mp4, OutputFormat::Webm] {
            let (video_codecs, audio_codecs) = match remux_codecs(target) {
                Some((video, audio)) if parsed => (video, audio),
                _ => (&[][..], &[][..]),
            };
            routes.push(VideoRouteInfo {
                source_extension,
                target,
                route: if parsed {
                    VideoRoute::Remux
                } else {
                    VideoRoute::Transcode
                },
                video_codecs,
                audio_codecs,
                fallback: parsed.then_some(VideoRoute::Transcode),
            });
        }
        routes.push(VideoRouteInfo {
            source_extension,
            target: OutputFormat::Gif,
            route: VideoRoute::Transcode,
            video_codecs: &[],
            audio_codecs: &[],
            fallback: None,
        });
    }
    routes
}

/// The route a conversion takes, from the bytes and the target alone.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VideoPlan {
    /// The target container takes every source codec, so copy the streams.
    Remux,
    /// Encode into the target container's codecs.
    Encode,
}

pub(crate) fn convert_video(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    video: &VideoToolConfig,
    control: Option<&mut VideoControl<'_>>,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    match plan_video(input, &file_extension(input_name), target) {
        VideoPlan::Remux => remux_video(input_name, input, target, video),
        VideoPlan::Encode => encode_video(input_name, input, target, video, control),
    }
}

/// Chooses the route. A GIF is always encoded, and so is a container the
/// native parser cannot read; a parsed container is copied only when every
/// stream's codec is on the target's copy list.
fn plan_video(input: &[u8], extension: &str, target: OutputFormat) -> VideoPlan {
    let Some((video_allowed, audio_allowed)) = remux_codecs(target) else {
        return VideoPlan::Encode;
    };
    let Some(codecs) = source_codecs(input, extension) else {
        return VideoPlan::Encode;
    };
    let copyable = !codecs.unknown_video
        && !codecs.unknown_audio
        && codecs
            .video
            .iter()
            .all(|codec| video_allowed.contains(&codec.as_str()))
        && codecs
            .audio
            .iter()
            .all(|codec| audio_allowed.contains(&codec.as_str()));
    if copyable {
        VideoPlan::Remux
    } else {
        VideoPlan::Encode
    }
}

fn source_codecs(input: &[u8], extension: &str) -> Option<VideoCodecs> {
    if is_mp4_extension(extension) {
        mp4_video_codecs(input)
    } else if matches!(extension, "mkv" | "webm") {
        matroska_video_codecs(input)
    } else {
        None
    }
}

pub(crate) fn remux_video(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    video: &VideoToolConfig,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    let extension = file_extension(input_name);
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
        _ => unreachable!("only a video container reaches remux_video"),
    }
    command.arg(output_path.path());
    run_ffmpeg(&mut command, &ffmpeg)?;

    let bytes = std::fs::read(output_path.path()).map_err(|_| ConvertError::FfmpegEmptyOutput)?;
    validate_video_output(&bytes, target)?;
    Ok(vec![ConvertedFile {
        name: output_name(input_name, target.extension()),
        bytes,
    }])
}

/// Default frame rate for an animated GIF. Twelve frames a second is what a
/// video-derived GIF usually ships at: motion still reads, and dropping five
/// frames in six from a 30 fps source is the single largest size saving after
/// the palette.
const GIF_FPS: u32 = 12;

/// Animated GIFs wider than this are downscaled. A GIF is 8-bit palette data
/// with no inter-frame compression worth the name, so a 1080p source is
/// several megabytes a second; 480 px keeps a shareable file and is the width
/// the web app's own video-to-GIF conversion settles on.
const GIF_MAX_WIDTH: u32 = 480;

/// The shared first half of both GIF passes. `min()` keeps a small source from
/// being upscaled, and the comma inside it is escaped for the filter parser.
fn gif_filter() -> String {
    format!("fps={GIF_FPS},scale=min({GIF_MAX_WIDTH}\\,iw):-1:flags=lanczos")
}

/// ffmpeg's own libx264 equivalent is absent from LGPL-only builds, and MP4 is
/// the most requested video target, so a failed H.264 encode is retried with
/// the MPEG-4 Part 2 encoder every build carries.
fn h264_missing(stderr: &str) -> bool {
    stderr.contains("Unknown encoder") && stderr.contains("libx264")
}

fn encode_video(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    video: &VideoToolConfig,
    control: Option<&mut VideoControl<'_>>,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    if control
        .as_deref()
        .is_some_and(|control| control.cancel.load(Ordering::Relaxed))
    {
        return Err(ConvertError::Cancelled);
    }
    let extension = file_extension(input_name);
    let codecs = source_codecs(input, &extension);
    if let Some(codecs) = &codecs {
        if codecs.video.is_empty() && !codecs.unknown_video {
            return Err(ConvertError::UnsupportedVideoCodec {
                stream: "video",
                codec: "none".to_owned(),
                container: target.extension(),
            });
        }
    }
    let ffmpeg = resolve_ffmpeg(video)?;
    let input_path = TempVideoPath::new(if extension.is_empty() {
        "bin"
    } else {
        &extension
    });
    std::fs::write(input_path.path(), input)?;
    let output_path = TempVideoPath::new(target.extension());

    match target {
        OutputFormat::Gif => encode_gif(&ffmpeg, input_path.path(), output_path.path(), control)?,
        OutputFormat::Mp4 | OutputFormat::Webm => encode_container(
            &ffmpeg,
            input_path.path(),
            output_path.path(),
            target,
            codecs.as_ref(),
            control,
        )?,
        _ => unreachable!("only a video target or GIF reaches encode_video"),
    }

    let bytes = std::fs::read(output_path.path()).map_err(|_| ConvertError::FfmpegEmptyOutput)?;
    validate_video_output(&bytes, target)?;
    Ok(vec![ConvertedFile {
        name: output_name(input_name, target.extension()),
        bytes,
    }])
}

fn validate_video_output(bytes: &[u8], target: OutputFormat) -> Result<(), ConvertError> {
    if bytes.is_empty() {
        return Err(ConvertError::FfmpegEmptyOutput);
    }
    let valid_container = match target {
        OutputFormat::Mp4 => bytes.get(4..8) == Some(b"ftyp"),
        OutputFormat::Webm => bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]),
        OutputFormat::Gif => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        _ => true,
    };
    if !valid_container {
        return Err(ConvertError::FfmpegInvalidOutput {
            container: target.extension(),
        });
    }
    Ok(())
}

/// `-progress pipe:1 -nostats` for a run that is being tracked. An untracked
/// run omits them so its stdout stays free for a frame pipeline.
fn progress_args(tracked: bool) -> &'static [&'static str] {
    if tracked {
        &["-progress", "pipe:1", "-nostats"]
    } else {
        &[]
    }
}

/// Runs one encoding invocation, streaming progress and honouring cancellation
/// only when a control was supplied.
fn run_encode(
    command: &mut std::process::Command,
    ffmpeg: &Path,
    control: Option<&mut VideoControl<'_>>,
) -> Result<(), ConvertError> {
    match control {
        Some(control) => run_ffmpeg_tracked(
            command,
            ffmpeg,
            control.cancel,
            Some(&mut *control.on_progress),
        ),
        None => run_ffmpeg(command, ffmpeg).map(|_| ()),
    }
}

fn encode_container(
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    target: OutputFormat,
    codecs: Option<&VideoCodecs>,
    mut control: Option<&mut VideoControl<'_>>,
) -> Result<(), ConvertError> {
    let (video_allowed, audio_allowed) =
        remux_codecs(target).expect("only a video container reaches encode_container");
    let copy_video = codecs.is_some_and(|codecs| {
        !codecs.unknown_video
            && !codecs.video.is_empty()
            && codecs
                .video
                .iter()
                .all(|codec| video_allowed.contains(&codec.as_str()))
    });
    let copy_audio = codecs.is_some_and(|codecs| {
        !codecs.unknown_audio
            && codecs
                .audio
                .iter()
                .all(|codec| audio_allowed.contains(&codec.as_str()))
    });

    // `scale` rounds an odd width or height down instead of failing: the
    // yuv420p pixel format both encoders are given cannot hold an odd size.
    let mut command = ffmpeg_command(ffmpeg);
    command
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(progress_args(control.is_some()))
        .arg("-i")
        .arg(input)
        .args(["-map", "0:v?", "-map", "0:a?"]);
    if copy_video {
        command.args(["-c:v", "copy"]);
    } else if target == OutputFormat::Mp4 {
        command.args([
            "-c:v",
            "libx264",
            "-preset",
            "medium",
            "-crf",
            "23",
            "-pix_fmt",
            "yuv420p",
            "-vf",
            "scale=trunc(iw/2)*2:trunc(ih/2)*2",
        ]);
    } else {
        command.args([
            "-c:v",
            "libvpx-vp9",
            "-crf",
            "32",
            "-b:v",
            "0",
            "-pix_fmt",
            "yuv420p",
            "-vf",
            "scale=trunc(iw/2)*2:trunc(ih/2)*2",
        ]);
    }
    if copy_audio {
        command.args(["-c:a", "copy"]);
    } else if target == OutputFormat::Mp4 {
        command.args(["-c:a", "aac", "-b:a", "128k"]);
    } else {
        command.args(["-c:a", "libopus", "-b:a", "128k"]);
    }
    if target == OutputFormat::Mp4 {
        command.args(["-movflags", "+faststart", "-f", "mp4"]);
    } else {
        command.args(["-f", "webm"]);
    }
    command.arg(output);

    let result = run_encode(&mut command, ffmpeg, control.as_deref_mut());
    let Err(error) = result else {
        return Ok(());
    };
    // Only a build without libx264 is worth a second attempt: any other
    // failure would fail the same way with a different encoder.
    if target != OutputFormat::Mp4
        || !matches!(&error, ConvertError::FfmpegFailed { stderr, .. } if h264_missing(stderr))
    {
        return Err(error);
    }
    let mut retry = ffmpeg_command(ffmpeg);
    retry
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(progress_args(control.is_some()))
        .arg("-i")
        .arg(input)
        .args(["-map", "0:v?", "-map", "0:a?"]);
    if copy_video {
        retry.args(["-c:v", "copy"]);
    } else {
        retry.args([
            "-c:v",
            "mpeg4",
            "-q:v",
            "3",
            "-pix_fmt",
            "yuv420p",
            "-vf",
            "scale=trunc(iw/2)*2:trunc(ih/2)*2",
        ]);
    }
    if copy_audio {
        retry.args(["-c:a", "copy"]);
    } else {
        retry.args(["-c:a", "aac", "-b:a", "128k"]);
    }
    retry
        .args(["-movflags", "+faststart", "-f", "mp4"])
        .arg(output);
    run_encode(&mut retry, ffmpeg, control)
}

/// Encodes a GIF in two passes: `palettegen` derives a 256-colour palette from
/// the frames the GIF will hold, and `paletteuse` maps each frame onto it. A
/// single-pass GIF shares ffmpeg's default 32-colour web palette, which is what
/// makes a naive video-to-GIF conversion look banded.
fn encode_gif(
    ffmpeg: &Path,
    input: &Path,
    output: &Path,
    mut control: Option<&mut VideoControl<'_>>,
) -> Result<(), ConvertError> {
    let palette = TempVideoPath::new("png");
    let filter = gif_filter();
    let mut analysis = ffmpeg_command(ffmpeg);
    analysis
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(progress_args(control.is_some()))
        .arg("-i")
        .arg(input)
        .args(["-vf", &format!("{filter},palettegen=stats_mode=diff")])
        .args(["-frames:v", "1"])
        .arg(palette.path());
    run_encode(&mut analysis, ffmpeg, control.as_deref_mut())?;

    let mut render = ffmpeg_command(ffmpeg);
    render
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args(progress_args(control.is_some()))
        .arg("-i")
        .arg(input)
        .arg("-i")
        .arg(palette.path())
        .args([
            "-lavfi",
            &format!("{filter}[x];[x][1:v]paletteuse=dither=sierra2_4a"),
        ])
        .args(["-loop", "0"])
        .arg(output);
    run_encode(&mut render, ffmpeg, control)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::convert_bytes_with_video_config;
    use crate::error::ConvertError;
    use crate::format::OutputFormat;
    use crate::options::ConversionOptions;

    use crate::test_support::png_fixture;
    use std::{
        ffi::{OsStr, OsString},
        path::{Path, PathBuf},
        sync::{Mutex, OnceLock},
    };
    static VIDEO_ENV_LOCK: Mutex<()> = Mutex::new(());
    fn video_testdata(name: &str) -> Vec<u8> {
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("testdata")
                .join(name),
        )
        .unwrap()
    }
    fn real_ffmpeg() -> Option<PathBuf> {
        let path = VideoToolConfig::default().resolve()?;
        let output = std::process::Command::new(&path)
            .arg("-version")
            .output()
            .ok()?;
        let version = String::from_utf8_lossy(&output.stdout);
        (output.status.success() && version.contains("ffmpeg version")).then_some(path)
    }
    /// A test that cannot run has to say so where the log will show it.
    /// libtest captures the `print!` macros for a test that passes, so an
    /// `eprintln!` notice never leaves the harness and the skip reads as `ok`.
    /// Writing to the handle directly is not captured. Setting
    /// `CONVERT2IT_REQUIRE_FFMPEG=1` turns the skip into a failure, so a job
    /// that is supposed to have ffmpeg cannot go green without running these.
    fn ffmpeg_for_real_test(test: &str) -> Option<PathBuf> {
        if let Some(ffmpeg) = real_ffmpeg() {
            return Some(ffmpeg);
        }
        let notice = format!("skipped real-ffmpeg {test}: ffmpeg is not available");
        {
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(), "{notice}");
        }
        if std::env::var("CONVERT2IT_REQUIRE_FFMPEG").is_ok_and(|value| value == "1") {
            panic!("{notice} (CONVERT2IT_REQUIRE_FFMPEG is set)");
        }
        None
    }

    fn generate_real_video(ffmpeg: &Path, path: &Path) {
        let output = std::process::Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=10:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:v",
                "libvpx-vp9",
                "-b:v",
                "300k",
                "-c:a",
                "libopus",
                "-shortest",
            ])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "ffmpeg could not generate the synthetic sample: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn generate_real_avi(ffmpeg: &Path, path: &Path) {
        // MPEG-4 Part 2 video and uncompressed PCM audio: neither stream may be
        // copied into MP4, and both encoders are built into ffmpeg, so the
        // fixture needs no external encoder of its own.
        let output = std::process::Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=10:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:v",
                "mpeg4",
                "-q:v",
                "5",
                "-c:a",
                "pcm_s16le",
                "-shortest",
            ])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "ffmpeg could not generate the AVI sample: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn ffprobe_streams(ffprobe: &Path, path: &Path) -> String {
        let output = std::process::Command::new(ffprobe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name,codec_type,width,height,nb_frames",
                "-of",
                "default=noprint_wrappers=1",
            ])
            .arg(path)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
    fn ffprobe_for(ffmpeg: &Path) -> Option<PathBuf> {
        let name = if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        };
        let path = ffmpeg.with_file_name(name);
        path.is_file().then_some(path)
    }
    fn ffmpeg_stub() -> &'static Path {
        static STUB: OnceLock<PathBuf> = OnceLock::new();
        STUB.get_or_init(|| {
            let directory =
                std::env::temp_dir().join(format!("convert2it-ffmpeg-stub-{}", std::process::id()));
            std::fs::create_dir_all(&directory).unwrap();
            let source = directory.join("ffmpeg_stub.rs");
            std::fs::write(
                &source,
                r#"
    use std::{env, fs, io::{self, Write}, process, thread, time::Duration};
    fn main() {
        let args: Vec<String> = env::args().skip(1).collect();
        if let Ok(log) = env::var("STUB_LOG") {
            let mut options = fs::OpenOptions::new();
            options.create(true).write(true);
            if env::var_os("STUB_LOG_APPEND").is_some() {
                options.append(true);
            } else {
                options.truncate(true);
            }
            if let Ok(mut file) = options.open(log) {
                let _ = writeln!(file, "{}", args.join("\n"));
            }
        }
        if let Ok(marker) = env::var("STUB_FAIL_ONCE") {
            if !fs::metadata(&marker).is_ok_and(|metadata| metadata.len() > 0) {
                let _ = fs::write(&marker, b"1");
                if let Ok(stderr) = env::var("STUB_STDERR") {
                    eprintln!("{stderr}");
                }
                process::exit(1);
            }
        }
        if let Ok(code) = env::var("STUB_EXIT") {
            if let Ok(stderr) = env::var("STUB_STDERR") {
                eprintln!("{stderr}");
            }
            process::exit(code.parse().unwrap_or(1));
        }
        if let Ok(duration) = env::var("STUB_DURATION") {
            eprintln!("Duration: {duration}");
        }
        if let Ok(progress) = env::var("STUB_PROGRESS") {
            for line in progress.split('|') {
                println!("{line}");
            }
        }
        if let Ok(milliseconds) = env::var("STUB_SLEEP_MS") {
            thread::sleep(Duration::from_millis(milliseconds.parse().unwrap_or(0)));
        }
        if args.iter().any(|arg| arg == "image2pipe") {
            let path = env::var("STUB_PNG").unwrap();
            io::stdout().write_all(&fs::read(path).unwrap()).unwrap();
            return;
        }
        if let Some(output) = args.last() {
            if env::var_os("STUB_EMPTY").is_some() {
                let _ = fs::write(output, b"");
            } else if let Ok(prepared) = env::var("STUB_OUTPUT") {
                fs::copy(prepared, output).unwrap();
            }
        }
    }
    "#,
            )
            .unwrap();
            let executable = directory.join(if cfg!(windows) {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            });
            let status = std::process::Command::new("rustc")
                .arg("--edition=2021")
                .arg(&source)
                .arg("-o")
                .arg(&executable)
                .status()
                .expect("rustc must be available to build the ffmpeg test stub");
            assert!(status.success(), "could not build the ffmpeg test stub");
            executable
        })
    }
    struct EnvVars(Vec<(&'static str, Option<OsString>)>);
    impl EnvVars {
        fn new() -> Self {
            Self(Vec::new())
        }

        fn set(&mut self, key: &'static str, value: impl AsRef<OsStr>) -> &mut Self {
            self.0.push((key, std::env::var_os(key)));
            std::env::set_var(key, value);
            self
        }

        fn remove(&mut self, key: &'static str) -> &mut Self {
            self.0.push((key, std::env::var_os(key)));
            std::env::remove_var(key);
            self
        }
    }
    impl Drop for EnvVars {
        fn drop(&mut self) {
            for (key, value) in self.0.drain(..).rev() {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
    #[test]
    fn video_thumbnail_uses_the_ffmpeg_argument_pipeline() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-thumb-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let png = directory.join("thumb.png");
        let log = directory.join("args.log");
        std::fs::write(&png, png_fixture()).unwrap();
        let mut env = EnvVars::new();
        env.set("STUB_LOG", &log)
            .set("STUB_PNG", &png)
            .remove("STUB_EXIT")
            .remove("STUB_EMPTY");

        let result = convert_bytes_with_video_config(
            "movie.mp4",
            &video_testdata("sample-video.mp4"),
            OutputFormat::Png,
            ConversionOptions {
                video_frame_time_ms: 1_500,
                video_thumbnail_width: 640,
                ..Default::default()
            },
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
        )
        .unwrap();
        assert_eq!(result[0].name, PathBuf::from("movie.png"));
        assert_eq!(
            image::load_from_memory(&result[0].bytes).unwrap().width(),
            2
        );

        let args = std::fs::read_to_string(&log).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert!(args.windows(2).any(|pair| pair == ["-ss", "1.500"]));
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-vf", "scale=640:-2:flags=lanczos"]));
        assert!(args.contains(&"image2pipe"));
        assert!(args.windows(2).any(|pair| pair == ["-frames:v", "1"]));
        std::fs::remove_dir_all(&directory).unwrap();
    }
    #[test]
    fn remux_copies_streams_and_rejects_empty_ffmpeg_output() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-remux-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let prepared = directory.join("prepared.webm");
        let log = directory.join("args.log");
        let prepared_bytes = b"\x1aE\xdf\xa3stub-webm".to_vec();
        std::fs::write(&prepared, &prepared_bytes).unwrap();
        let mut env = EnvVars::new();
        env.set("STUB_LOG", &log)
            .set("STUB_OUTPUT", &prepared)
            .remove("STUB_EXIT")
            .remove("STUB_EMPTY");

        let result = convert_bytes_with_video_config(
            "sample-vp9-opus.mp4",
            &video_testdata("sample-vp9-opus.mp4"),
            OutputFormat::Webm,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
        )
        .unwrap();
        assert_eq!(result[0].name, PathBuf::from("sample-vp9-opus.webm"));
        assert_eq!(result[0].bytes, prepared_bytes);
        let args = std::fs::read_to_string(&log).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert!(args.windows(2).any(|pair| pair == ["-c", "copy"]));
        assert!(args.windows(2).any(|pair| pair == ["-map", "0:v?"]));
        assert!(args.windows(2).any(|pair| pair == ["-map", "0:a?"]));
        assert!(args.windows(2).any(|pair| pair == ["-f", "webm"]));

        env.set("STUB_EMPTY", "1");
        let error = convert_bytes_with_video_config(
            "sample-vp9-opus.webm",
            &video_testdata("sample-vp9-opus.webm"),
            OutputFormat::Mp4,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
        )
        .unwrap_err();
        assert!(matches!(error, ConvertError::FfmpegEmptyOutput));

        let invalid = directory.join("invalid.webm");
        std::fs::write(&invalid, b"not-a-container").unwrap();
        env.remove("STUB_EMPTY").set("STUB_OUTPUT", &invalid);
        let error = convert_bytes_with_video_config(
            "sample-vp9-opus.mp4",
            &video_testdata("sample-vp9-opus.mp4"),
            OutputFormat::Webm,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::FfmpegInvalidOutput { container: "webm" }
        ));
        std::fs::remove_dir_all(&directory).unwrap();
    }
    #[test]
    fn remux_surfaces_ffmpeg_stderr_and_missing_binary() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-failure-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut env = EnvVars::new();
        env.set("STUB_EXIT", "7")
            .set("STUB_STDERR", "stub failure details")
            .remove("STUB_EMPTY");

        let error = convert_bytes_with_video_config(
            "sample-vp9-opus.mp4",
            &video_testdata("sample-vp9-opus.mp4"),
            OutputFormat::Webm,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
        )
        .unwrap_err();
        match error {
            ConvertError::FfmpegFailed { status, stderr } => {
                assert_eq!(status, "7");
                assert!(stderr.contains("stub failure details"), "{stderr}");
            }
            other => panic!("expected FfmpegFailed, got {other}"),
        }

        let missing = directory.join("does-not-exist");
        let error = convert_bytes_with_video_config(
            "sample-vp9-opus.mp4",
            &video_testdata("sample-vp9-opus.mp4"),
            OutputFormat::Webm,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(missing),
        )
        .unwrap_err();
        assert!(matches!(error, ConvertError::FfmpegNotFound { .. }));
        std::fs::remove_dir_all(&directory).unwrap();
    }
    #[test]
    fn ffmpeg_is_discovered_on_path_and_the_route_follows_the_codecs() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-path-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let prepared = directory.join("prepared.mp4");
        let log = directory.join("args.log");
        std::fs::write(&prepared, b"\0\0\0\x18ftypstub").unwrap();
        let stub_directory = ffmpeg_stub().parent().unwrap();
        let mut env = EnvVars::new();
        env.set("PATH", stub_directory)
            .set("STUB_LOG", &log)
            .set("STUB_OUTPUT", &prepared)
            .remove("CONVERT2IT_FFMPEG")
            .remove("FFMPEG")
            .remove("STUB_EXIT")
            .remove("STUB_EMPTY");

        let result = convert_bytes_with_video_config(
            "sample-vp9-opus.webm",
            &video_testdata("sample-vp9-opus.webm"),
            OutputFormat::Mp4,
            ConversionOptions::default(),
            &VideoToolConfig::default(),
        )
        .unwrap();
        assert_eq!(result[0].bytes, b"\0\0\0\x18ftypstub");

        assert_eq!(
            plan_video(
                &video_testdata("sample-video.mp4"),
                "mp4",
                OutputFormat::Webm
            ),
            VideoPlan::Encode
        );
        assert_eq!(
            plan_video(
                &video_testdata("sample-vp9-opus.mp4"),
                "mp4",
                OutputFormat::Webm
            ),
            VideoPlan::Remux
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }
    #[test]
    fn incompatible_pairs_need_ffmpeg_instead_of_the_stream_copy() {
        let missing_ffmpeg = std::env::temp_dir().join("convert2it-definitely-missing-ffmpeg");
        for name in ["sample-even-h264.mp4", "sample-multitrack-h264.mp4"] {
            let error = convert_bytes_with_video_config(
                name,
                &video_testdata(name),
                OutputFormat::Webm,
                ConversionOptions::default(),
                &VideoToolConfig::with_ffmpeg(&missing_ffmpeg),
            )
            .unwrap_err();
            match error {
                ConvertError::FfmpegNotFound { path } => assert!(path.contains("ffmpeg"), "{path}"),
                other => panic!("{name}: expected a missing-ffmpeg error, got {other:?}"),
            }
        }
    }

    #[test]
    fn h264_mp4_to_webm_is_encoded_instead_of_rejected() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-encode-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let prepared = directory.join("prepared.webm");
        let log = directory.join("args.log");
        std::fs::write(&prepared, b"\x1aE\xdf\xa3stub-encoded-webm").unwrap();
        let mut env = EnvVars::new();
        env.set("STUB_LOG", &log)
            .set("STUB_OUTPUT", &prepared)
            .remove("STUB_EXIT")
            .remove("STUB_EMPTY");

        let result = convert_bytes_with_video_config(
            "sample-even-h264.mp4",
            &video_testdata("sample-even-h264.mp4"),
            OutputFormat::Webm,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
        )
        .unwrap();
        assert_eq!(result[0].name, PathBuf::from("sample-even-h264.webm"));
        let args = std::fs::read_to_string(&log).unwrap();
        assert!(args.contains("-c:v\nlibvpx-vp9"), "{args}");
        assert!(args.contains("-c:a\nlibopus"), "{args}");
        assert!(args.contains("-crf\n32"), "{args}");
        assert!(!args.contains("-c\ncopy"), "{args}");
        assert!(!args.contains("copy"), "{args}");
        assert!(args.contains("-map\n0:v?"), "{args}");
        assert!(args.contains("-f\nwebm"), "{args}");
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn gif_output_encodes_a_palette_and_the_animation() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-gif-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let prepared = directory.join("prepared.gif");
        let log = directory.join("args.log");
        std::fs::write(&prepared, b"GIF89a-stub-animation").unwrap();
        let mut env = EnvVars::new();
        env.set("STUB_LOG", &log)
            .set("STUB_LOG_APPEND", "1")
            .set("STUB_OUTPUT", &prepared)
            .remove("STUB_EXIT")
            .remove("STUB_EMPTY");

        let result = convert_bytes_with_video_config(
            "sample-even-h264.mp4",
            &video_testdata("sample-even-h264.mp4"),
            OutputFormat::Gif,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
        )
        .unwrap();
        assert_eq!(result[0].name, PathBuf::from("sample-even-h264.gif"));
        assert_eq!(result[0].bytes, b"GIF89a-stub-animation");

        let log = std::fs::read_to_string(&log).unwrap();
        let palette_pass = log
            .split("paletteuse")
            .next()
            .expect("the log holds both passes");
        assert!(palette_pass.contains("palettegen=stats_mode=diff"), "{log}");
        assert!(palette_pass.contains("fps=12"), "{log}");
        assert!(palette_pass.contains("scale=min(480\\,iw)"), "{log}");
        assert!(palette_pass.contains("-frames:v"), "{log}");
        assert!(log.contains("paletteuse=dither=sierra2_4a"), "{log}");
        assert!(log.contains("-loop\n0"), "{log}");
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn cancelling_an_encode_kills_ffmpeg_and_reports_progress() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-cancel-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut env = EnvVars::new();
        env.set("STUB_PROGRESS", "out_time_us=500000|progress=continue")
            .set("STUB_SLEEP_MS", "5000")
            .remove("STUB_LOG")
            .remove("STUB_OUTPUT")
            .remove("STUB_EXIT")
            .remove("STUB_EMPTY");

        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut updates = Vec::new();
        let started = std::time::Instant::now();
        let mut on_progress = |progress: VideoProgress| {
            updates.push(progress);
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        };
        let mut control = VideoControl {
            cancel: &cancel,
            on_progress: &mut on_progress,
        };
        let error = crate::convert::convert_bytes_with_video_control(
            "sample-even-h264.mp4",
            &video_testdata("sample-even-h264.mp4"),
            OutputFormat::Webm,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
            Some(&mut control),
        )
        .unwrap_err();
        assert!(matches!(error, ConvertError::Cancelled), "{error:?}");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(2_500),
            "the stub sleeps for 5 s, so the cancel did not kill it"
        );
        assert_eq!(
            updates.first(),
            Some(&VideoProgress {
                out_time: std::time::Duration::from_millis(500),
                total: None,
            })
        );
        assert_eq!(updates[0].fraction(), None);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn ffmpeg_without_libx264_falls_back_to_the_mpeg4_encoder() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-fallback-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let prepared = directory.join("prepared.mp4");
        let log = directory.join("args.log");
        let marker = directory.join("first-run");
        std::fs::write(&prepared, b"\0\0\0\x18ftypstub").unwrap();
        let mut env = EnvVars::new();
        env.set("STUB_LOG", &log)
            .set("STUB_LOG_APPEND", "1")
            .set("STUB_OUTPUT", &prepared)
            .set("STUB_FAIL_ONCE", &marker)
            .set("STUB_STDERR", "Unknown encoder 'libx264'")
            .remove("STUB_EXIT")
            .remove("STUB_EMPTY");

        // An AVI always encodes: the native parser cannot prove a stream copy
        // would be legal, so this is the H.264-first path the fallback covers.
        let result = convert_bytes_with_video_config(
            "sample.avi",
            b"stub avi bytes",
            OutputFormat::Mp4,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
        )
        .unwrap();
        assert_eq!(result[0].name, PathBuf::from("sample.mp4"));
        assert_eq!(result[0].bytes, b"\0\0\0\x18ftypstub");
        let log = std::fs::read_to_string(&log).unwrap();
        assert!(log.contains("libx264"), "{log}");
        assert!(log.contains("-c:v\nmpeg4"), "{log}");
        std::fs::remove_dir_all(&directory).unwrap();
    }
    #[test]
    fn h264_mp4_to_mp4_remuxes_with_stream_copy() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-mp4-remux-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let prepared = directory.join("prepared.mp4");
        let log = directory.join("args.log");
        std::fs::write(&prepared, b"\0\0\0\x18ftypstub").unwrap();
        let mut env = EnvVars::new();
        env.set("STUB_LOG", &log)
            .set("STUB_OUTPUT", &prepared)
            .remove("STUB_EXIT")
            .remove("STUB_EMPTY");

        let result = convert_bytes_with_video_config(
            "sample-odd-h264.mp4",
            &video_testdata("sample-odd-h264.mp4"),
            OutputFormat::Mp4,
            ConversionOptions::default(),
            &VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
        )
        .unwrap();
        assert_eq!(result[0].name, PathBuf::from("sample-odd-h264.mp4"));
        let args = std::fs::read_to_string(&log).unwrap();
        assert!(args.contains("-c\ncopy"), "{args}");
        assert!(args.contains("-map\n0:v?"), "{args}");
        assert!(args.contains("-map\n0:a?"), "{args}");
        assert!(!args.contains("libvpx"), "{args}");
        std::fs::remove_dir_all(&directory).unwrap();
    }
    #[test]
    fn real_ffmpeg_encodes_what_stream_copy_cannot() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let Some(ffmpeg) = ffmpeg_for_real_test("encode test") else {
            return;
        };
        let directory =
            std::env::temp_dir().join(format!("convert2it-real-encode-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let config = VideoToolConfig::with_ffmpeg(&ffmpeg);
        let ffprobe = ffprobe_for(&ffmpeg);

        let avi_path = directory.join("synthetic.avi");
        generate_real_avi(&ffmpeg, &avi_path);
        let avi = std::fs::read(&avi_path).unwrap();

        let mp4 = convert_bytes_with_video_config(
            "synthetic.avi",
            &avi,
            OutputFormat::Mp4,
            ConversionOptions::default(),
            &config,
        )
        .unwrap();
        assert_eq!(mp4[0].name, PathBuf::from("synthetic.mp4"));
        assert_eq!(mp4[0].bytes.get(4..8), Some(b"ftyp".as_slice()));
        let mp4_path = directory.join("encoded.mp4");
        std::fs::write(&mp4_path, &mp4[0].bytes).unwrap();
        if let Some(ffprobe) = ffprobe.as_ref() {
            let probe = ffprobe_streams(ffprobe, &mp4_path);
            // libx264 is what every full ffmpeg build ships; an LGPL-only build
            // falls back to the MPEG-4 Part 2 encoder and is still valid MP4.
            assert!(
                probe.contains("codec_name=h264") || probe.contains("codec_name=mpeg4"),
                "{probe}"
            );
            assert!(probe.contains("codec_name=aac"), "{probe}");
            assert!(probe.contains("codec_type=video"), "{probe}");
            assert!(probe.contains("codec_type=audio"), "{probe}");
            assert!(probe.contains("width=320"), "{probe}");
            assert!(probe.contains("height=240"), "{probe}");
        }

        let webm = convert_bytes_with_video_config(
            "sample-even-h264.mp4",
            &video_testdata("sample-even-h264.mp4"),
            OutputFormat::Webm,
            ConversionOptions::default(),
            &config,
        )
        .unwrap();
        assert_eq!(webm[0].name, PathBuf::from("sample-even-h264.webm"));
        assert!(webm[0].bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]));
        let webm_path = directory.join("encoded.webm");
        std::fs::write(&webm_path, &webm[0].bytes).unwrap();
        if let Some(ffprobe) = ffprobe.as_ref() {
            let probe = ffprobe_streams(ffprobe, &webm_path);
            assert!(probe.contains("codec_name=vp9"), "{probe}");
            assert!(probe.contains("codec_name=opus"), "{probe}");
            assert!(probe.contains("width=320"), "{probe}");
            assert!(probe.contains("height=240"), "{probe}");
        }

        let gif = convert_bytes_with_video_config(
            "synthetic.avi",
            &avi,
            OutputFormat::Gif,
            ConversionOptions::default(),
            &config,
        )
        .unwrap();
        assert_eq!(gif[0].name, PathBuf::from("synthetic.gif"));
        assert!(
            gif[0].bytes.starts_with(b"GIF89a") || gif[0].bytes.starts_with(b"GIF87a"),
            "not a GIF"
        );
        let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(&gif[0].bytes))
            .expect("the encoded bytes decode as a GIF");
        let frames = image::AnimationDecoder::into_frames(decoder)
            .collect_frames()
            .expect("every GIF frame decodes");
        // 12 fps over a one-second source. ffmpeg rounds the last interval, so
        // the count is 12 or 13 depending on its version; the source is 320 px
        // wide, so the width cap must not upscale it.
        assert!(
            (11..=13).contains(&frames.len()),
            "expected about 12 frames for a one-second source, got {}",
            frames.len()
        );
        let first = frames[0].buffer();
        assert_eq!((first.width(), first.height()), (320, 240));
        assert_ne!(
            frames[0].buffer().as_raw(),
            frames[6].buffer().as_raw(),
            "the frames are identical, so the animation carries a single image"
        );
        let gif_path = directory.join("encoded.gif");
        std::fs::write(&gif_path, &gif[0].bytes).unwrap();
        if let Some(ffprobe) = ffprobe.as_ref() {
            let probe = ffprobe_streams(ffprobe, &gif_path);
            assert!(probe.contains("codec_name=gif"), "{probe}");
            let counted: u32 = probe
                .lines()
                .find_map(|line| line.strip_prefix("nb_frames="))
                .and_then(|value| value.parse().ok())
                .expect("ffprobe reports the GIF frame count");
            assert!(
                (11..=13).contains(&counted),
                "expected about 12 frames, ffprobe counted {counted}"
            );
            assert!(probe.contains("width=320"), "{probe}");
        }

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn real_ffmpeg_generated_media_remux_and_thumbnails_when_available() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let Some(ffmpeg) = ffmpeg_for_real_test("library test") else {
            return;
        };
        let directory =
            std::env::temp_dir().join(format!("convert2it-real-video-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let input_path = directory.join("synthetic.mp4");
        generate_real_video(&ffmpeg, &input_path);
        let input = std::fs::read(&input_path).unwrap();
        let config = VideoToolConfig::with_ffmpeg(&ffmpeg);

        let webm = convert_bytes_with_video_config(
            "synthetic.mp4",
            &input,
            OutputFormat::Webm,
            ConversionOptions::default(),
            &config,
        )
        .unwrap();
        assert_eq!(webm[0].name, PathBuf::from("synthetic.webm"));
        assert!(!webm[0].bytes.is_empty());
        assert!(webm[0].bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]));
        let webm_path = directory.join("synthetic.webm");
        std::fs::write(&webm_path, &webm[0].bytes).unwrap();
        let ffprobe = ffprobe_for(&ffmpeg);
        let mut webm_streams = None;
        if let Some(ffprobe) = ffprobe.as_ref() {
            let output = std::process::Command::new(ffprobe)
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "format=format_name:stream=codec_name,codec_type,width,height,channels",
                    "-of",
                    "default=noprint_wrappers=1",
                ])
                .arg(&webm_path)
                .output()
                .unwrap();
            assert!(output.status.success());
            let probe = String::from_utf8_lossy(&output.stdout);
            assert!(probe.contains("format_name=matroska,webm"), "{probe}");
            assert!(probe.contains("codec_name=vp9"), "{probe}");
            assert!(probe.contains("codec_name=opus"), "{probe}");
            assert!(probe.contains("width=320"), "{probe}");
            assert!(probe.contains("height=240"), "{probe}");
            let streams = probe
                .lines()
                .filter(|line| !line.starts_with("format_name="))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(streams.contains("codec_type=video"), "{streams}");
            assert!(streams.contains("codec_type=audio"), "{streams}");
            webm_streams = Some(streams);
        }

        let back = convert_bytes_with_video_config(
            "synthetic.webm",
            &webm[0].bytes,
            OutputFormat::Mp4,
            ConversionOptions::default(),
            &config,
        )
        .unwrap();
        assert_eq!(back[0].name, PathBuf::from("synthetic.mp4"));
        assert_eq!(back[0].bytes.get(4..8), Some(b"ftyp".as_slice()));
        let back_path = directory.join("synthetic-roundtrip.mp4");
        std::fs::write(&back_path, &back[0].bytes).unwrap();
        if let Some(ffprobe) = ffprobe.as_ref() {
            let output = std::process::Command::new(ffprobe)
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "format=format_name:stream=codec_name,codec_type,width,height,channels",
                    "-of",
                    "default=noprint_wrappers=1",
                ])
                .arg(&back_path)
                .output()
                .unwrap();
            assert!(output.status.success());
            let probe = String::from_utf8_lossy(&output.stdout);
            assert!(
                probe.contains("format_name=") && probe.contains("mp4"),
                "{probe}"
            );
            assert!(probe.contains("codec_name=vp9"), "{probe}");
            assert!(probe.contains("codec_name=opus"), "{probe}");
            assert!(probe.contains("width=320"), "{probe}");
            assert!(probe.contains("height=240"), "{probe}");
            let streams = probe
                .lines()
                .filter(|line| !line.starts_with("format_name="))
                .collect::<Vec<_>>()
                .join("\n");
            if let Some(webm_streams) = webm_streams.as_deref() {
                assert_eq!(webm_streams, streams);
            }
        }

        let thumbnails = [
            (OutputFormat::Png, image::ImageFormat::Png),
            (OutputFormat::Jpeg, image::ImageFormat::Jpeg),
            (OutputFormat::Webp, image::ImageFormat::WebP),
        ];
        for (target, format) in thumbnails {
            let thumbnail = convert_bytes_with_video_config(
                "synthetic.mp4",
                &input,
                target,
                ConversionOptions {
                    video_frame_time_ms: 500,
                    video_thumbnail_width: 160,
                    ..Default::default()
                },
                &config,
            )
            .unwrap();
            assert!(!thumbnail[0].bytes.is_empty());
            assert_eq!(image::guess_format(&thumbnail[0].bytes).unwrap(), format);
            let decoded = image::load_from_memory(&thumbnail[0].bytes).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (160, 120));
        }

        let at_zero = convert_bytes_with_video_config(
            "synthetic.mp4",
            &input,
            OutputFormat::Png,
            ConversionOptions {
                video_frame_time_ms: 0,
                video_thumbnail_width: 160,
                ..Default::default()
            },
            &config,
        )
        .unwrap();
        let at_half_second = convert_bytes_with_video_config(
            "synthetic.mp4",
            &input,
            OutputFormat::Png,
            ConversionOptions {
                video_frame_time_ms: 500,
                video_thumbnail_width: 160,
                ..Default::default()
            },
            &config,
        )
        .unwrap();
        assert_ne!(at_zero[0].bytes, at_half_second[0].bytes);

        let error = convert_bytes_with_video_config(
            "synthetic.mp4",
            &input,
            OutputFormat::Png,
            ConversionOptions {
                video_frame_time_ms: 10_000,
                video_thumbnail_width: 160,
                ..Default::default()
            },
            &config,
        )
        .unwrap_err();
        assert!(matches!(error, ConvertError::FfmpegEmptyOutput));
        assert_eq!(error.to_string(), "ffmpeg produced no output");

        let error = convert_bytes_with_video_config(
            "broken.mp4",
            b"not a movie",
            OutputFormat::Webm,
            ConversionOptions::default(),
            &config,
        )
        .unwrap_err();
        match error {
            ConvertError::FfmpegFailed { status, stderr } => {
                assert_ne!(status, "0");
                assert!(!stderr.is_empty());
            }
            other => panic!("expected a real ffmpeg failure, got {other}"),
        }

        std::fs::remove_dir_all(&directory).unwrap();
    }
}
