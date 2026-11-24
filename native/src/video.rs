// SPDX-License-Identifier: GPL-2.0-or-later

//! Stream-copy remuxing, thumbnails and video route validation.

use crate::container::{
    ebml_element_end, is_mp4_extension, mp4_child_boxes, mp4_find, read_ebml_element,
    read_ebml_uint, skip_ebml_element,
};
use crate::convert::ConvertedFile;
use crate::error::ConvertError;
use crate::ffmpeg::{ffmpeg_command, find_on_path, resolve_ffmpeg, run_ffmpeg, TempVideoPath};
use crate::format::OutputFormat;
use crate::images::encode_image;
use crate::options::ConversionOptions;
use crate::shared::{file_extension, output_name};
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
            let _ = fs::write(log, args.join("\n"));
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
    fn ffmpeg_is_discovered_on_path_and_incompatible_codecs_are_rejected() {
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

        let error = validate_remux(
            &video_testdata("sample-video.mp4"),
            "mp4",
            OutputFormat::Webm,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::UnsupportedVideoCodec {
                stream: "video",
                codec,
                container: "webm"
            } if codec == "h264"
        ));
        std::fs::remove_dir_all(&directory).unwrap();
    }
    #[test]
    fn incompatible_stream_copy_combinations_return_a_typed_error() {
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
                ConvertError::UnsupportedVideoCodec {
                    stream: "video",
                    codec,
                    container: "webm",
                } => assert_eq!(codec, "h264", "{name}"),
                other => panic!("{name}: expected an unsupported-codec error, got {other:?}"),
            }
        }
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
