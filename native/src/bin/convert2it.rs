// SPDX-License-Identifier: GPL-2.0-or-later

//! Scriptable front end for the native conversion engine.
//!
//! This binary deliberately uses the same `convert_batch_with_options` entry
//! point as the GUI, so format support, limits and archive handling cannot
//! drift between the two front ends.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};

use convert_to_it_native::{
    convert_batch_with_video_config, native_format_registry, native_video_routes, AudioBitDepth,
    AudioChannels, ConversionOptions, Mp3Bitrate, OutputFormat, PdfImageFit, PdfOrientation,
    PdfPageSize, VideoToolConfig, MAX_TRACE_DIMENSION, MAX_TRACE_PIXELS,
};

const USAGE: &str = "\
convert2it - convert files without the GUI

Usage:
  convert2it --format <FORMAT> [OPTIONS] <INPUT>...

Required:
  -f, --format <FORMAT>       Output format, e.g. png, jpg, mp3, pdf, zip, files

Options:
  -o, --output <DIR>          Output directory (default: current directory)
      --overwrite             Replace existing output files
      --jpeg-quality <1-100>
      --png-compression <1-9>
      --archive-compression <1-9>
      --audio-sample-rate <HZ>
      --audio-channels <mono|stereo>
      --audio-track <INDEX>   Zero-based audio track index
      --audio-bit-depth <16|24|float32>
      --mp3-bitrate <128|192|256|320>
      --image-background <RRGGBB>
      --no-exif-orientation
      --pdf-page-size <a3|a4|a5|letter|legal>
      --pdf-orientation <portrait|landscape>
      --pdf-image-fit <contain|cover>
      --pdf-font-size <PT>
      --pdf-margin <PT>
      --video-frame-ms <MS>   Frame timestamp for video thumbnails
      --thumbnail-width <PX>  Thumbnail width (0 = source width)
      --ffmpeg <PATH>         ffmpeg executable (default: PATH/CONVERT2IT_FFMPEG/FFMPEG)
  -h, --help
      --list-formats          Print the native input/output registry and exit
      --list-video            Print the automatic video routes and exit

Use `--format files` to extract a ZIP, TAR, TAR.GZ, GZIP, RAR or 7z archive.
Video (`mp4`/`webm`/`gif`) and thumbnails (`png`/`jpg`/`webp` from video)
require ffmpeg on PATH, CONVERT2IT_FFMPEG/FFMPEG, or --ffmpeg. A pair whose
codecs the target already takes is stream-copied; any other pair is re-encoded,
and without ffmpeg there is no conversion.
";

#[derive(Debug)]
struct Cli {
    inputs: Vec<PathBuf>,
    target: OutputFormat,
    output: PathBuf,
    options: ConversionOptions,
    video: VideoToolConfig,
    overwrite: bool,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|argument| argument == "--list-formats") {
        print_format_registry();
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|argument| argument == "--list-video") {
        print_video_routes();
        return ExitCode::SUCCESS;
    }
    match parse_args(args.into_iter()) {
        Ok(Some(cli)) => match run(cli) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("convert2it: {error}");
                ExitCode::FAILURE
            }
        },
        Ok(None) => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("convert2it: {error}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn print_format_registry() {
    for handler in native_format_registry() {
        let extension = handler.canonical_extension.unwrap_or("*");
        let outputs = handler
            .outputs
            .iter()
            .map(|output| output_token(*output))
            .collect::<Vec<_>>()
            .join(",");
        println!("{}\t{}\t{}", handler.input.label(), extension, outputs);
    }
    println!();
    println!(
        "Metadata: images keep EXIF and ICC profiles into jpg, png, webp and tiff \
         (orientation is reset to 1 after the pixels are rotated, and targets that \
         cannot store the blocks drop them); audio keeps title, artist, album, track \
         and embedded cover art into mp3, flac and opus."
    );
    println!();
    println!(
        "Video: png/jpg/webp are frame thumbnails; mp4/webm copy the streams when the codecs \
         already fit and re-encode otherwise; gif is an animated 12 fps GIF. A GIF's frames \
         become an MP4 instead of a single thumbnail, and mp3/m4a are copied into an MP4 \
         container without re-encoding. avi/wmv/mpeg are always re-encoded; all require ffmpeg \
         (PATH, CONVERT2IT_FFMPEG/FFMPEG, or --ffmpeg)."
    );
    println!();
    println!(
        "Office: docx, pptx and odt become Markdown-shaped text (headings, list items and \
         pipe tables in reading order); xlsx, xls and ods become one CSV per sheet, one JSON \
         document, or one text file of tab-separated rows per sheet. Nothing is rendered: a \
         document is read for its content, never for its layout."
    );
    println!();
    println!(
        "SVG: `svg` is the font specimen, `svg-page` is one file per PDF page, and \
         `svg-trace` traces a raster image into paths. Tracing is limited to images of \
         at most {max_side} pixels per side and {max_megapixels} megapixels in total.",
        max_side = MAX_TRACE_DIMENSION,
        max_megapixels = MAX_TRACE_PIXELS / 1_000_000,
    );
}

/// The name a target is asked for on the command line. All three SVG targets
/// write a `.svg` file, so the registry prints the `--format` spelling that selects
/// one of them instead of the extension three of them share.
fn output_token(target: OutputFormat) -> &'static str {
    match target {
        OutputFormat::Svg => "svg",
        OutputFormat::SvgPage => "svg-page",
        OutputFormat::SvgTrace => "svg-trace",
        other => other.extension(),
    }
}

fn print_video_routes() {
    println!("source\ttarget\troute\tvideo\taudio\tfallback");
    for route in native_video_routes() {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            route.source_extension,
            output_token(route.target),
            route.route.label(),
            route.video_codecs.join(","),
            route.audio_codecs.join(","),
            route
                .fallback
                .map(|fallback| fallback.label())
                .unwrap_or("none")
        );
    }
    println!();
    println!(
        "Routing is automatic: a source whose codecs fit the target is stream-copied; any other \
         pair is re-encoded. The video and audio columns list the codecs the target copies \
         without re-encoding. A GIF is encoded because an MP4 holds no frame of an image, and \
         an audio-only source is copied because the MP4 container takes its codec as it is."
    );
}

fn run(cli: Cli) -> Result<(), String> {
    fs::create_dir_all(&cli.output)
        .map_err(|error| format!("{}: {error}", cli.output.display()))?;

    let mut inputs = Vec::with_capacity(cli.inputs.len());
    for path in &cli.inputs {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("{} has no file name", path.display()))?;
        if !cli.target.supports_input(name) {
            return Err(format!(
                "{} cannot be converted to {}",
                path.display(),
                cli.target.label()
            ));
        }
        let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
        inputs.push((path.clone(), bytes));
    }

    let converted = convert_batch_with_video_config(&inputs, cli.target, cli.options, &cli.video)
        .map_err(|error| error.to_string())?;
    for file in converted {
        let relative = safe_relative_path(&file.name)?;
        let destination = cli.output.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        let mut open = OpenOptions::new();
        open.write(true);
        if cli.overwrite {
            open.create(true).truncate(true);
        } else {
            open.create_new(true);
        }
        let mut output = open
            .open(&destination)
            .map_err(|error| format!("{}: {error}", destination.display()))?;
        output
            .write_all(&file.bytes)
            .map_err(|error| format!("{}: {error}", destination.display()))?;
        println!("{}", destination.display());
    }
    Ok(())
}

fn safe_relative_path(path: &Path) -> Result<PathBuf, String> {
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => safe.push(part),
            _ => {
                return Err(format!(
                    "output path {} must be relative and cannot contain parent components",
                    path.display()
                ))
            }
        }
    }
    if safe.as_os_str().is_empty() {
        return Err("output path cannot be empty".to_owned());
    }
    Ok(safe)
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Option<Cli>, String> {
    let mut args = args.peekable();
    let mut inputs = Vec::new();
    let mut target = None;
    let mut output = PathBuf::from(".");
    let mut options = ConversionOptions::default();
    let mut video = VideoToolConfig::default();
    let mut overwrite = false;

    while let Some(argument) = args.next() {
        let (flag, inline_value) = match argument.split_once('=') {
            Some((flag, value)) if flag.starts_with('-') => (flag, Some(value.to_owned())),
            _ => (argument.as_str(), None),
        };
        let mut value = || -> Result<String, String> {
            if let Some(value) = inline_value.clone() {
                return Ok(value);
            }
            args.next()
                .ok_or_else(|| format!("{flag} requires a value"))
        };

        match flag {
            "-h" | "--help" => return Ok(None),
            "-f" | "--format" => {
                let name = value()?;
                target = Some(
                    OutputFormat::from_extension(&name)
                        .ok_or_else(|| format!("unknown output format {name:?}"))?,
                );
            }
            "-o" | "--output" => output = PathBuf::from(value()?),
            "--overwrite" => overwrite = true,
            "--jpeg-quality" => {
                let quality = parse_u8(value()?, "JPEG quality")?;
                if !(1..=100).contains(&quality) {
                    return Err("JPEG quality must be between 1 and 100".to_owned());
                }
                options.jpeg_quality = Some(quality);
            }
            "--png-compression" => {
                options.png_compression = Some(parse_level(value()?, "PNG compression")?);
            }
            "--archive-compression" => {
                options.archive_compression = Some(parse_level(value()?, "archive compression")?);
            }
            "--audio-sample-rate" => {
                let rate = value()?
                    .parse::<u32>()
                    .map_err(|_| "audio sample rate must be a number".to_owned())?;
                if !(ConversionOptions::MIN_AUDIO_SAMPLE_RATE
                    ..=ConversionOptions::MAX_AUDIO_SAMPLE_RATE)
                    .contains(&rate)
                {
                    return Err(format!(
                        "audio sample rate must be between {} and {}",
                        ConversionOptions::MIN_AUDIO_SAMPLE_RATE,
                        ConversionOptions::MAX_AUDIO_SAMPLE_RATE
                    ));
                }
                options.audio_sample_rate = Some(rate);
            }
            "--audio-channels" => {
                options.audio_channels = Some(match value()?.to_ascii_lowercase().as_str() {
                    "mono" | "1" => AudioChannels::Mono,
                    "stereo" | "2" => AudioChannels::Stereo,
                    other => return Err(format!("unknown channel layout {other:?}")),
                });
            }
            "--audio-track" => {
                options.audio_track = Some(
                    value()?
                        .parse::<usize>()
                        .map_err(|_| "audio track must be a non-negative number".to_owned())?,
                );
            }
            "--audio-bit-depth" => {
                options.audio_bit_depth = match value()?.to_ascii_lowercase().as_str() {
                    "16" | "16-bit" => AudioBitDepth::Bit16,
                    "24" | "24-bit" => AudioBitDepth::Bit24,
                    "32" | "float32" | "32-bit-float" => AudioBitDepth::Float32,
                    other => return Err(format!("unknown audio bit depth {other:?}")),
                };
            }
            "--mp3-bitrate" => {
                options.mp3_bitrate = match value()?.as_str() {
                    "128" => Mp3Bitrate::Kbps128,
                    "192" => Mp3Bitrate::Kbps192,
                    "256" => Mp3Bitrate::Kbps256,
                    "320" => Mp3Bitrate::Kbps320,
                    other => return Err(format!("unknown MP3 bitrate {other:?}")),
                };
            }
            "--image-background" => {
                let value = value()?;
                options.image_background = parse_rgb(&value)?;
            }
            "--no-exif-orientation" => options.image_apply_exif_orientation = false,
            "--pdf-page-size" => {
                options.pdf_page_size = match value()?.to_ascii_lowercase().as_str() {
                    "a3" => PdfPageSize::A3,
                    "a4" => PdfPageSize::A4,
                    "a5" => PdfPageSize::A5,
                    "letter" => PdfPageSize::Letter,
                    "legal" => PdfPageSize::Legal,
                    other => return Err(format!("unknown PDF page size {other:?}")),
                };
            }
            "--pdf-orientation" => {
                options.pdf_orientation = match value()?.to_ascii_lowercase().as_str() {
                    "portrait" => PdfOrientation::Portrait,
                    "landscape" => PdfOrientation::Landscape,
                    other => return Err(format!("unknown PDF orientation {other:?}")),
                };
            }
            "--pdf-image-fit" => {
                options.pdf_image_fit = match value()?.to_ascii_lowercase().as_str() {
                    "contain" => PdfImageFit::Contain,
                    "cover" => PdfImageFit::Cover,
                    other => return Err(format!("unknown PDF image fit {other:?}")),
                };
            }
            "--pdf-font-size" => {
                let size = value()?
                    .parse::<u32>()
                    .map_err(|_| "PDF font size must be a number".to_owned())?;
                if !(ConversionOptions::MIN_PDF_FONT_SIZE..=ConversionOptions::MAX_PDF_FONT_SIZE)
                    .contains(&size)
                {
                    return Err("PDF font size is out of range".to_owned());
                }
                options.pdf_font_size = size;
            }
            "--video-frame-ms" => {
                let frame = value()?
                    .parse::<u32>()
                    .map_err(|_| "video frame time must be a number".to_owned())?;
                if frame > ConversionOptions::MAX_VIDEO_FRAME_TIME_MS {
                    return Err("video frame time is out of range".to_owned());
                }
                options.video_frame_time_ms = frame;
            }
            "--thumbnail-width" => {
                let width = value()?
                    .parse::<u32>()
                    .map_err(|_| "thumbnail width must be a number".to_owned())?;
                if width > ConversionOptions::MAX_VIDEO_THUMBNAIL_WIDTH {
                    return Err("thumbnail width is out of range".to_owned());
                }
                options.video_thumbnail_width = width;
            }
            "--ffmpeg" => video = VideoToolConfig::with_ffmpeg(value()?),
            "--pdf-margin" => {
                let margin = value()?
                    .parse::<u32>()
                    .map_err(|_| "PDF margin must be a number".to_owned())?;
                if !(ConversionOptions::MIN_PDF_MARGIN..=ConversionOptions::MAX_PDF_MARGIN)
                    .contains(&margin)
                {
                    return Err("PDF margin is out of range".to_owned());
                }
                options.pdf_margin = margin;
            }
            "--" => {
                inputs.extend(args.map(PathBuf::from));
                break;
            }
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag:?}")),
            input => inputs.push(PathBuf::from(input)),
        }
    }

    let target = target.ok_or_else(|| "--format is required".to_owned())?;
    if inputs.is_empty() {
        return Err("at least one input file is required".to_owned());
    }
    Ok(Some(Cli {
        inputs,
        target,
        output,
        options,
        video,
        overwrite,
    }))
}

fn parse_u8(value: String, label: &str) -> Result<u8, String> {
    value
        .parse::<u8>()
        .map_err(|_| format!("{label} must be a number"))
}

fn parse_level(value: String, label: &str) -> Result<u8, String> {
    let level = parse_u8(value, label)?;
    if !(1..=9).contains(&level) {
        return Err(format!("{label} must be between 1 and 9"));
    }
    Ok(level)
}

fn parse_rgb(value: &str) -> Result<[u8; 3], String> {
    let value = value.trim_start_matches('#');
    if value.len() != 6 {
        return Err("image background must be RRGGBB".to_owned());
    }
    let red = u8::from_str_radix(&value[0..2], 16)
        .map_err(|_| "image background must be RRGGBB".to_owned())?;
    let green = u8::from_str_radix(&value[2..4], 16)
        .map_err(|_| "image background must be RRGGBB".to_owned())?;
    let blue = u8::from_str_radix(&value[4..6], 16)
        .map_err(|_| "image background must be RRGGBB".to_owned())?;
    Ok([red, green, blue])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn ffmpeg_stub() -> &'static Path {
        static STUB: OnceLock<PathBuf> = OnceLock::new();
        STUB.get_or_init(|| {
            let directory = std::env::temp_dir()
                .join(format!("convert2it-cli-ffmpeg-stub-{}", std::process::id()));
            std::fs::create_dir_all(&directory).unwrap();
            let source = directory.join("ffmpeg_stub.rs");
            std::fs::write(
                &source,
                r#"
use std::{env, fs, io::{self, Write}, process};
fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if let Ok(log) = env::var("STUB_LOG") {
        let _ = fs::write(log, args.join("\n"));
    }
    if let Ok(code) = env::var("STUB_EXIT") {
        if let Ok(stderr) = env::var("STUB_STDERR") { eprintln!("{stderr}"); }
        process::exit(code.parse().unwrap_or(1));
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

    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl EnvGuard {
        fn new() -> Self {
            Self(Vec::new())
        }
        fn set(&mut self, key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> &mut Self {
            self.0.push((key, std::env::var_os(key)));
            std::env::set_var(key, value);
            self
        }
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
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.0.drain(..).rev() {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn parse(args: &[&str]) -> Result<Option<Cli>, String> {
        parse_args(args.iter().map(|argument| (*argument).to_owned()))
    }

    #[test]
    fn parses_formats_options_and_inputs() {
        let cli = parse(&[
            "--format",
            "mp3",
            "--output",
            "out",
            "--audio-channels",
            "mono",
            "--mp3-bitrate",
            "320",
            "--overwrite",
            "one.wav",
            "two.wav",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(cli.target, OutputFormat::Mp3);
        assert_eq!(cli.output, PathBuf::from("out"));
        assert_eq!(cli.options.audio_channels, Some(AudioChannels::Mono));
        assert_eq!(cli.options.mp3_bitrate, Mp3Bitrate::Kbps320);
        assert!(cli.overwrite);
        assert_eq!(
            cli.inputs,
            [PathBuf::from("one.wav"), PathBuf::from("two.wav")]
        );
    }

    #[test]
    fn accepts_inline_values_and_archive_extraction() {
        let cli = parse(&["--format=files", "--output=out", "bundle.zip"])
            .unwrap()
            .unwrap();
        assert_eq!(cli.target, OutputFormat::Files);
        assert_eq!(cli.options, ConversionOptions::default());
        assert_eq!(
            parse(&["--format", "gzip", "note.txt"])
                .unwrap()
                .unwrap()
                .target,
            OutputFormat::Gzip
        );
    }

    #[test]
    fn parses_video_options_and_ffmpeg_override() {
        let cli = parse(&[
            "--format",
            "webm",
            "--ffmpeg",
            "/tmp/ffmpeg-stub",
            "--video-frame-ms",
            "1500",
            "--thumbnail-width",
            "640",
            "movie.mp4",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(cli.target, OutputFormat::Webm);
        assert_eq!(cli.options.video_frame_time_ms, 1500);
        assert_eq!(cli.options.video_thumbnail_width, 640);
        assert_eq!(cli.video.ffmpeg, Some(PathBuf::from("/tmp/ffmpeg-stub")));
    }

    #[test]
    fn cli_video_remux_and_thumbnail_use_the_stub_and_overwrite_policy() {
        let _lock = ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-cli-video-test-{}", std::process::id()));
        let output = directory.join("out");
        std::fs::create_dir_all(&output).unwrap();
        let input = directory.join("sample-vp9-opus.mp4");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("testdata")
                .join("sample-vp9-opus.mp4"),
            &input,
        )
        .unwrap();
        let prepared = directory.join("prepared.webm");
        std::fs::write(&prepared, b"\x1aE\xdf\xa3stub-webm").unwrap();
        let log = directory.join("args.log");
        let png = directory.join("thumb.png");
        let image = image::RgbImage::from_pixel(2, 2, image::Rgb([10, 20, 30]));
        let mut png_bytes = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut png_bytes, image::ImageFormat::Png)
            .unwrap();
        std::fs::write(&png, png_bytes.into_inner()).unwrap();

        let mut env = EnvGuard::new();
        env.set("STUB_OUTPUT", &prepared)
            .set("STUB_LOG", &log)
            .set("STUB_PNG", &png);

        let make_cli = |overwrite| Cli {
            inputs: vec![input.clone()],
            target: OutputFormat::Webm,
            output: output.clone(),
            options: ConversionOptions::default(),
            video: VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
            overwrite,
        };
        run(make_cli(false)).unwrap();
        let remuxed = output.join("sample-vp9-opus.webm");
        assert_eq!(std::fs::read(&remuxed).unwrap(), b"\x1aE\xdf\xa3stub-webm");
        assert!(
            run(make_cli(false)).is_err(),
            "existing output must be refused"
        );
        run(make_cli(true)).unwrap();

        let thumbnail = Cli {
            inputs: vec![input.clone()],
            target: OutputFormat::Png,
            output: output.clone(),
            options: ConversionOptions {
                video_frame_time_ms: 1_500,
                video_thumbnail_width: 64,
                ..Default::default()
            },
            video: VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
            overwrite: false,
        };
        run(thumbnail).unwrap();
        let thumbnail_bytes = std::fs::read(output.join("sample-vp9-opus.png")).unwrap();
        assert_eq!(
            image::load_from_memory(&thumbnail_bytes).unwrap().width(),
            2
        );
        let args = std::fs::read_to_string(&log).unwrap();
        assert!(args.contains("image2pipe"));
        assert!(args.contains("1.500"));

        env.set("STUB_EXIT", "9").set("STUB_STDERR", "cli boom");
        let error = run(Cli {
            inputs: vec![input.clone()],
            target: OutputFormat::Webm,
            output: output.clone(),
            options: ConversionOptions::default(),
            video: VideoToolConfig::with_ffmpeg(ffmpeg_stub()),
            overwrite: true,
        })
        .unwrap_err();
        assert!(error.contains("ffmpeg exited with 9"), "{error}");
        assert!(error.contains("cli boom"), "{error}");

        let error = run(Cli {
            inputs: vec![input],
            target: OutputFormat::Webm,
            output,
            options: ConversionOptions::default(),
            video: VideoToolConfig::with_ffmpeg(directory.join("missing-ffmpeg")),
            overwrite: true,
        })
        .unwrap_err();
        assert!(error.contains("ffmpeg was not found"), "{error}");
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn real_ffmpeg_cli_video_when_available() {
        let _lock = ENV_LOCK.lock().unwrap();
        let Some(ffmpeg) = ffmpeg_for_real_test("CLI test") else {
            return;
        };
        let directory =
            std::env::temp_dir().join(format!("convert2it-cli-real-video-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let input = directory.join("synthetic.mp4");
        generate_real_video(&ffmpeg, &input);
        let output = directory.join("out");
        std::fs::create_dir_all(&output).unwrap();
        let ffmpeg_text = ffmpeg.to_string_lossy().into_owned();
        let output_text = output.to_string_lossy().into_owned();
        let input_text = input.to_string_lossy().into_owned();

        let cli = parse(&[
            "--format",
            "webm",
            "--ffmpeg",
            ffmpeg_text.as_str(),
            "--output",
            output_text.as_str(),
            input_text.as_str(),
        ])
        .unwrap()
        .unwrap();
        run(cli).unwrap();
        let webm = std::fs::read(output.join("synthetic.webm")).unwrap();
        assert!(!webm.is_empty());
        assert!(webm.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]));

        let back_output = directory.join("back");
        std::fs::create_dir_all(&back_output).unwrap();
        let webm_text = output.join("synthetic.webm").to_string_lossy().into_owned();
        let back_output_text = back_output.to_string_lossy().into_owned();
        let cli = parse(&[
            "--format",
            "mp4",
            "--ffmpeg",
            ffmpeg_text.as_str(),
            "--output",
            back_output_text.as_str(),
            webm_text.as_str(),
        ])
        .unwrap()
        .unwrap();
        run(cli).unwrap();
        let back = std::fs::read(back_output.join("synthetic.mp4")).unwrap();
        assert_eq!(back.get(4..8), Some(b"ftyp".as_slice()));

        for (name, format) in [
            ("png", image::ImageFormat::Png),
            ("jpg", image::ImageFormat::Jpeg),
            ("webp", image::ImageFormat::WebP),
        ] {
            let cli = parse(&[
                "--format",
                name,
                "--ffmpeg",
                ffmpeg_text.as_str(),
                "--video-frame-ms",
                "500",
                "--thumbnail-width",
                "160",
                "--output",
                output_text.as_str(),
                input_text.as_str(),
            ])
            .unwrap()
            .unwrap();
            run(cli).unwrap();
            let bytes = std::fs::read(output.join(format!("synthetic.{name}"))).unwrap();
            assert!(!bytes.is_empty());
            assert_eq!(image::guess_format(&bytes).unwrap(), format);
            let decoded = image::load_from_memory(&bytes).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (160, 120));
        }

        let empty_output = directory.join("empty-out");
        std::fs::create_dir_all(&empty_output).unwrap();
        let empty_output_text = empty_output.to_string_lossy().into_owned();
        let cli = parse(&[
            "--format",
            "png",
            "--ffmpeg",
            ffmpeg_text.as_str(),
            "--video-frame-ms",
            "10000",
            "--thumbnail-width",
            "160",
            "--output",
            empty_output_text.as_str(),
            input_text.as_str(),
        ])
        .unwrap()
        .unwrap();
        let error = run(cli).unwrap_err();
        assert!(error.contains("ffmpeg produced no output"), "{error}");
        assert!(std::fs::read_dir(&empty_output).unwrap().next().is_none());

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn real_ffmpeg_cli_wraps_audio_and_encodes_a_gif_when_available() {
        let _lock = ENV_LOCK.lock().unwrap();
        let Some(ffmpeg) = ffmpeg_for_real_test("CLI container-source test") else {
            return;
        };
        let directory = std::env::temp_dir().join(format!(
            "convert2it-cli-container-source-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let output = directory.join("out");
        std::fs::create_dir_all(&output).unwrap();
        let ffmpeg_text = ffmpeg.to_string_lossy().into_owned();
        let output_text = output.to_string_lossy().into_owned();

        let mp3 = directory.join("tone.mp3");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("testdata")
                .join("metadata-tone.mp3"),
            &mp3,
        )
        .unwrap();
        let gif = directory.join("animation.gif");
        let generated = std::process::Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=10:duration=1",
                "-loop",
                "0",
            ])
            .arg(&gif)
            .output()
            .unwrap();
        assert!(
            generated.status.success(),
            "ffmpeg could not generate the GIF sample: {}",
            String::from_utf8_lossy(&generated.stderr)
        );

        for input in [&mp3, &gif] {
            let cli = parse(&[
                "--format",
                "mp4",
                "--ffmpeg",
                ffmpeg_text.as_str(),
                "--output",
                output_text.as_str(),
                input.to_string_lossy().as_ref(),
            ])
            .unwrap()
            .unwrap();
            run(cli).unwrap();
        }

        let wrapped = std::fs::read(output.join("tone.mp4")).unwrap();
        assert_eq!(wrapped.get(4..8), Some(b"ftyp".as_slice()));
        let animated = std::fs::read(output.join("animation.mp4")).unwrap();
        assert_eq!(animated.get(4..8), Some(b"ftyp".as_slice()));

        // The CLI wrote a copy of the MP3's audio, not a re-encode.
        let hash = |input: &Path| {
            let hashed = std::process::Command::new(&ffmpeg)
                .args(["-v", "error", "-i"])
                .arg(input)
                .args(["-map", "0:a", "-c", "copy", "-f", "md5", "-"])
                .output()
                .unwrap();
            assert!(hashed.status.success());
            String::from_utf8_lossy(&hashed.stdout).trim().to_owned()
        };
        assert_eq!(
            hash(&mp3),
            hash(&output.join("tone.mp4")),
            "the CLI's MP4 does not carry the source audio intact"
        );

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn rejects_unknown_formats_and_options() {
        assert!(parse(&["--format", "nope", "file.txt"]).is_err());
        assert!(parse(&["--format", "png", "--nope", "file.png"]).is_err());
        assert!(parse(&["file.png"]).is_err());
    }

    #[test]
    fn rejects_escaping_output_paths() {
        assert!(safe_relative_path(Path::new("../evil.txt")).is_err());
        assert_eq!(
            safe_relative_path(Path::new("nested/one.txt")).unwrap(),
            PathBuf::from("nested/one.txt")
        );
    }
}
