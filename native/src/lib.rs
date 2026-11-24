// SPDX-License-Identifier: GPL-2.0-or-later

use image::ImageEncoder;
use pdf_writer::{Content, Filter, Name, Pdf, Rect, Ref, Str};
use std::{
    collections::{BTreeSet, HashSet},
    ffi::{OsStr, OsString},
    fs::File,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::{Mutex, OnceLock},
    time::Duration,
};

mod archive;
mod audio;
mod container;
mod convert;
mod data;
mod document;
mod error;
mod ffmpeg;
mod format;
mod images;
pub mod input_extensions;
pub mod input_format;
mod options;
mod shared;
mod streaming;
mod text;
mod video;

use crate::archive::*;
use crate::audio::*;
use crate::container::*;
use crate::document::*;
use crate::images::*;
use crate::video::*;
pub use archive::extract_archive;
pub use audio::{
    audio_output_name, audio_tracks, audio_tracks_from_path, convert_audio_to_wav_writer,
    wav_output_name, AudioBitDepth, AudioChannels, AudioTrackInfo, Mp3Bitrate,
};
pub use convert::{
    convert_batch, convert_batch_with_options, convert_batch_with_video_config, convert_bytes,
    convert_bytes_with_options, convert_bytes_with_video_config, convert_bytes_with_video_control,
    ConvertedFile,
};
pub use document::{PdfImageFit, PdfOrientation, PdfPageSize};
pub use error::ConvertError;
pub use format::{native_format_registry, FormatHandler, OutputFormat};
pub use input_extensions::supported_input_extensions;
pub use options::ConversionOptions;
pub use streaming::{convert_audio_file_to_writer, convert_audio_file_to_writer_with_cancel};
pub use video::{
    native_video_routes, VideoControl, VideoProgress, VideoRoute, VideoRouteInfo, VideoToolConfig,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::{OsStr, OsString},
        io::Read,
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

    fn png_fixture() -> Vec<u8> {
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            2,
            2,
            image::Rgb([20, 40, 60]),
        ));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    fn detailed_png_fixture() -> Vec<u8> {
        let mut pixels = image::RgbImage::new(64, 64);
        for (x, y, pixel) in pixels.enumerate_pixels_mut() {
            *pixel = image::Rgb([
                (x * 4) as u8,
                (y * 4) as u8,
                ((x * 17 + y * 29) % 256) as u8,
            ]);
        }
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(pixels)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    #[test]
    fn converts_between_supported_image_formats() {
        let result = convert_bytes("sample.png", &png_fixture(), OutputFormat::Webp).unwrap();
        assert_eq!(result[0].name, PathBuf::from("sample.webp"));
        let decoded =
            image::load_from_memory_with_format(&result[0].bytes, image::ImageFormat::WebP)
                .unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 2));
    }

    #[test]
    fn applies_jpeg_quality_and_png_compression_options() {
        let input = detailed_png_fixture();
        let low_quality = convert_bytes_with_options(
            "sample.png",
            &input,
            OutputFormat::Jpeg,
            ConversionOptions {
                jpeg_quality: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        let high_quality = convert_bytes_with_options(
            "sample.png",
            &input,
            OutputFormat::Jpeg,
            ConversionOptions {
                jpeg_quality: Some(95),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(low_quality[0].name, PathBuf::from("sample.jpg"));
        assert_eq!(high_quality[0].name, PathBuf::from("sample.jpg"));
        assert!(
            high_quality[0].bytes.len() > low_quality[0].bytes.len(),
            "higher JPEG quality should produce a larger file"
        );
        assert!(image::load_from_memory_with_format(
            &high_quality[0].bytes,
            image::ImageFormat::Jpeg
        )
        .is_ok());

        let fast_png = convert_bytes_with_options(
            "sample.png",
            &input,
            OutputFormat::Png,
            ConversionOptions {
                png_compression: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let best_png = convert_bytes_with_options(
            "sample.png",
            &input,
            OutputFormat::Png,
            ConversionOptions {
                png_compression: Some(9),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(fast_png[0].name, PathBuf::from("sample.png"));
        assert_eq!(best_png[0].name, PathBuf::from("sample.png"));
        assert!(
            best_png[0].bytes.len() <= fast_png[0].bytes.len(),
            "higher PNG compression should not produce a larger file"
        );
        assert!(
            image::load_from_memory_with_format(&best_png[0].bytes, image::ImageFormat::Png)
                .is_ok()
        );
    }

    #[test]
    fn applies_archive_compression_options() {
        let inputs = vec![(
            PathBuf::from("note.txt"),
            b"the same line repeated many times\n".repeat(2_000),
        )];

        let fast_zip = convert_batch_with_options(
            &inputs,
            OutputFormat::Zip,
            ConversionOptions {
                archive_compression: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let best_zip = convert_batch_with_options(
            &inputs,
            OutputFormat::Zip,
            ConversionOptions {
                archive_compression: Some(9),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            best_zip[0].bytes.len() <= fast_zip[0].bytes.len(),
            "higher ZIP compression should not produce a larger archive"
        );
        assert!(zip::ZipArchive::new(Cursor::new(&best_zip[0].bytes)).is_ok());

        let fast_gzip = convert_batch_with_options(
            &inputs,
            OutputFormat::Gzip,
            ConversionOptions {
                archive_compression: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let best_gzip = convert_batch_with_options(
            &inputs,
            OutputFormat::Gzip,
            ConversionOptions {
                archive_compression: Some(9),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            best_gzip[0].bytes.len() <= fast_gzip[0].bytes.len(),
            "higher GZIP compression should not produce a larger file"
        );
        let mut decoder = flate2::read::GzDecoder::new(Cursor::new(&best_gzip[0].bytes));
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, inputs[0].1);
    }

    #[test]
    fn converts_to_bmp_and_tiff_outputs() {
        for (target, extension, format) in [
            (OutputFormat::Bmp, "bmp", image::ImageFormat::Bmp),
            (OutputFormat::Tiff, "tiff", image::ImageFormat::Tiff),
        ] {
            let result = convert_bytes("sample.png", &png_fixture(), target).unwrap();
            assert_eq!(result[0].name, PathBuf::from(format!("sample.{extension}")));

            let decoded = image::load_from_memory_with_format(&result[0].bytes, format).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (2, 2));
        }
    }

    #[test]
    fn converts_bmp_and_tiff_inputs_to_png() {
        for (extension, format) in [
            ("bmp", image::ImageFormat::Bmp),
            ("tif", image::ImageFormat::Tiff),
            ("tiff", image::ImageFormat::Tiff),
        ] {
            let mut input = Cursor::new(Vec::new());
            image::load_from_memory(&png_fixture())
                .unwrap()
                .write_to(&mut input, format)
                .unwrap();

            let result = convert_bytes(
                &format!("sample.{extension}"),
                &input.into_inner(),
                OutputFormat::Png,
            )
            .unwrap();
            let decoded =
                image::load_from_memory_with_format(&result[0].bytes, image::ImageFormat::Png)
                    .unwrap();
            assert_eq!((decoded.width(), decoded.height()), (2, 2));
        }
    }

    #[test]
    fn converts_csv_to_json_and_back() {
        let csv = b"name,age\nAda,36\nLin,28\n";
        let json = convert_bytes("people.csv", csv, OutputFormat::Json).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json[0].bytes).unwrap();
        assert_eq!(value[0]["name"], "Ada");
        assert_eq!(value[1]["age"], "28");

        let round_trip = convert_bytes("people.json", &json[0].bytes, OutputFormat::Csv).unwrap();
        let mut reader = csv::Reader::from_reader(round_trip[0].bytes.as_slice());
        let headers = reader.headers().unwrap().clone();
        let name_column = headers.iter().position(|header| header == "name").unwrap();
        let age_column = headers.iter().position(|header| header == "age").unwrap();
        let first = reader.records().next().unwrap().unwrap();
        assert_eq!(&first[name_column], "Ada");
        assert_eq!(&first[age_column], "36");
    }

    #[test]
    fn converts_plain_text_to_json_string_and_back() {
        let json = convert_bytes("note.txt", b"hello\nworld", OutputFormat::Json).unwrap();
        assert_eq!(
            serde_json::from_slice::<String>(&json[0].bytes).unwrap(),
            "hello\nworld"
        );
        let text = convert_bytes("note.json", &json[0].bytes, OutputFormat::Text).unwrap();
        assert_eq!(text[0].bytes, b"hello\nworld");
    }

    #[test]
    fn converts_text_between_the_web_supported_unicode_encodings() {
        let content = "Hello 🌍";
        for target in [
            OutputFormat::TextUtf8NoBom,
            OutputFormat::TextUtf8Bom,
            OutputFormat::TextUtf16Le,
            OutputFormat::TextUtf16Be,
            OutputFormat::TextUtf32Le,
            OutputFormat::TextUtf32Be,
        ] {
            let encoded = convert_bytes("sample.txt", content.as_bytes(), target).unwrap();
            let decoded =
                convert_bytes("sample.txt", &encoded[0].bytes, OutputFormat::Text).unwrap();
            assert_eq!(decoded[0].bytes, content.as_bytes());
        }
    }

    #[test]
    fn rejects_invalid_unicode_code_points_in_utf32_text() {
        let input = [0xff, 0xfe, 0x00, 0x00, 0x00, 0xd8, 0x00, 0x00];
        assert!(matches!(
            convert_bytes("sample.txt", &input, OutputFormat::Text),
            Err(ConvertError::InvalidUnicodeCodePoint(0xd800))
        ));
    }

    #[test]
    fn archives_multiple_files() {
        let inputs = vec![
            (PathBuf::from("one.txt"), b"one".to_vec()),
            (PathBuf::from("two.txt"), b"two".to_vec()),
        ];
        let archive = convert_batch(&inputs, OutputFormat::Zip).unwrap();
        assert_eq!(archive[0].name, PathBuf::from("converted.zip"));

        let mut zip = zip::ZipArchive::new(Cursor::new(&archive[0].bytes)).unwrap();
        assert_eq!(zip.len(), 2);
        let mut one = String::new();
        zip.by_name("one.txt")
            .unwrap()
            .read_to_string(&mut one)
            .unwrap();
        assert_eq!(one, "one");
        let mut two = String::new();
        zip.by_name("two.txt")
            .unwrap()
            .read_to_string(&mut two)
            .unwrap();
        assert_eq!(two, "two");
    }

    #[test]
    fn extracts_zip_tar_and_gzip_archives() {
        let zip_bytes = {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            writer.start_file("nested/one.txt", options).unwrap();
            writer.write_all(b"one").unwrap();
            writer.start_file("two.txt", options).unwrap();
            writer.write_all(b"two").unwrap();
            writer.finish().unwrap().into_inner()
        };
        let files = convert_bytes("bundle.zip", &zip_bytes, OutputFormat::Files).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, PathBuf::from("nested/one.txt"));
        assert_eq!(files[0].bytes, b"one");
        assert_eq!(files[1].name, PathBuf::from("two.txt"));
        assert_eq!(files[1].bytes, b"two");

        let tar_bytes = {
            let mut builder = tar::Builder::new(Vec::new());
            for (name, content) in [("nested/one.txt", &b"one"[..]), ("two.txt", &b"two"[..])] {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, name, content).unwrap();
            }
            builder.into_inner().unwrap()
        };
        let files = convert_bytes("bundle.tar", &tar_bytes, OutputFormat::Files).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, PathBuf::from("nested/one.txt"));
        assert_eq!(files[0].bytes, b"one");

        let gzip = convert_bytes("note.txt", b"hello", OutputFormat::Gzip).unwrap();
        let files = convert_bytes("note.txt.gz", &gzip[0].bytes, OutputFormat::Files).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, PathBuf::from("note.txt"));
        assert_eq!(files[0].bytes, b"hello");
    }

    #[test]
    fn rejects_archive_paths_that_escape_the_output_directory() {
        let tar_bytes = {
            let mut builder = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_path("safe.txt").unwrap();
            header.as_mut_bytes()[..11].copy_from_slice(b"../evil.txt");
            header.set_size(3);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, &b"bad"[..]).unwrap();
            builder.into_inner().unwrap()
        };
        assert!(matches!(
            convert_bytes("bundle.tar", &tar_bytes, OutputFormat::Files),
            Err(ConvertError::UnsafeArchivePath(_))
        ));

        for name in [
            "../evil.txt",
            "nested/../../evil.txt",
            "/absolute.txt",
            "C:\\evil.txt",
        ] {
            let zip_bytes = {
                let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
                let options = zip::write::SimpleFileOptions::default();
                writer.start_file(name, options).unwrap();
                writer.write_all(b"bad").unwrap();
                writer.finish().unwrap().into_inner()
            };
            let error = convert_bytes("bundle.zip", &zip_bytes, OutputFormat::Files).unwrap_err();
            assert!(
                matches!(error, ConvertError::UnsafeArchivePath(_)),
                "{name} produced {error}"
            );
        }
    }

    #[test]
    fn rejects_duplicate_archive_entry_names() {
        let inputs = vec![
            (PathBuf::from("first/one.txt"), b"one".to_vec()),
            (PathBuf::from("second/one.txt"), b"two".to_vec()),
        ];
        assert!(matches!(
            convert_batch(&inputs, OutputFormat::Zip),
            Err(ConvertError::DuplicateArchiveEntryName(name)) if name == "one.txt"
        ));
    }

    #[test]
    fn rejects_archives_with_too_many_entries() {
        let inputs: Vec<(PathBuf, Vec<u8>)> = (0..=MAX_ARCHIVE_ENTRIES)
            .map(|index| (PathBuf::from(format!("{index}.txt")), Vec::new()))
            .collect();
        assert!(matches!(
            convert_batch(&inputs, OutputFormat::Zip),
            Err(ConvertError::ArchiveTooManyEntries { max, .. }) if max == MAX_ARCHIVE_ENTRIES
        ));
    }

    #[test]
    fn rejects_empty_batches() {
        assert!(matches!(
            convert_batch(&[], OutputFormat::Json),
            Err(ConvertError::NoInputFiles)
        ));
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
        let Some(ffmpeg) = real_ffmpeg() else {
            eprintln!("skipping real-ffmpeg library test: ffmpeg is not available");
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

    #[test]
    fn reports_unsupported_format_pairs() {
        let error = convert_bytes("movie.mp4", b"not a movie", OutputFormat::Pdf).unwrap_err();
        assert!(matches!(error, ConvertError::Unsupported { .. }));
    }

    #[test]
    fn converts_png_to_the_new_image_formats() {
        for (target, format) in [
            (OutputFormat::Gif, image::ImageFormat::Gif),
            (OutputFormat::Ico, image::ImageFormat::Ico),
            (OutputFormat::Ppm, image::ImageFormat::Pnm),
            (OutputFormat::Tga, image::ImageFormat::Tga),
            (OutputFormat::Qoi, image::ImageFormat::Qoi),
            (OutputFormat::Hdr, image::ImageFormat::Hdr),
            (OutputFormat::Farbfeld, image::ImageFormat::Farbfeld),
        ] {
            let result = convert_bytes("sample.png", &png_fixture(), target).unwrap();
            let decoded = image::load_from_memory_with_format(&result[0].bytes, format).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (2, 2));
        }
    }

    #[test]
    fn converts_png_to_pnm_variants() {
        for (target, extension, magic) in [
            (OutputFormat::Pbm, "pbm", &b"P4"[..]),
            (OutputFormat::Pgm, "pgm", &b"P5"[..]),
            (OutputFormat::Pam, "pam", &b"P7"[..]),
        ] {
            let converted = convert_bytes("sample.png", &png_fixture(), target).unwrap();
            assert_eq!(
                converted[0].name,
                PathBuf::from(format!("sample.{extension}"))
            );
            assert!(
                converted[0].bytes.starts_with(magic),
                "{extension} output is missing its magic number"
            );
            let decoded = image::load_from_memory(&converted[0].bytes).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (2, 2));
            if target == OutputFormat::Pbm {
                assert!(
                    decoded.to_luma8().pixels().all(|pixel| pixel[0] == 0),
                    "dark pixels should stay black in a PBM"
                );
            }
        }
    }

    #[test]
    fn rasterizes_svg_images_to_raster_formats() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#ff0000"/></svg>"##;
        let result = convert_bytes("icon.svg", svg, OutputFormat::Png).unwrap();
        assert_eq!(result[0].name, PathBuf::from("icon.png"));

        let decoded =
            image::load_from_memory_with_format(&result[0].bytes, image::ImageFormat::Png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (4, 4));
        let pixel = decoded.to_rgba8().get_pixel(1, 1).0;
        assert!(pixel[0] > 200 && pixel[1] < 40 && pixel[2] < 40);
    }

    #[test]
    fn converts_between_json_yaml_and_toml() {
        let json = br#"{"name":"Ada","age":36,"tags":["math","code"]}"#;

        let yaml = convert_bytes("person.json", json, OutputFormat::Yaml).unwrap();
        let back = convert_bytes("person.yaml", &yaml[0].bytes, OutputFormat::Json).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&back[0].bytes).unwrap();
        assert_eq!(value["name"], "Ada");
        assert_eq!(value["age"], 36);
        assert_eq!(value["tags"][1], "code");

        let toml = convert_bytes("person.json", json, OutputFormat::Toml).unwrap();
        let back = convert_bytes("person.toml", &toml[0].bytes, OutputFormat::Json).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&back[0].bytes).unwrap();
        assert_eq!(value["name"], "Ada");
        assert_eq!(value["age"], 36);
        assert_eq!(value["tags"][1], "code");
    }

    #[test]
    fn packs_tar_archives() {
        let inputs = vec![
            (PathBuf::from("one.txt"), b"one".to_vec()),
            (PathBuf::from("two.txt"), b"two".to_vec()),
        ];

        for (target, name) in [
            (OutputFormat::Tar, "converted.tar"),
            (OutputFormat::TarGz, "converted.tar.gz"),
        ] {
            let archive = convert_batch(&inputs, target).unwrap();
            assert_eq!(archive[0].name, PathBuf::from(name));

            let reader: Box<dyn Read> = if target == OutputFormat::TarGz {
                Box::new(flate2::read::GzDecoder::new(Cursor::new(&archive[0].bytes)))
            } else {
                Box::new(Cursor::new(&archive[0].bytes))
            };
            let mut tar = tar::Archive::new(reader);
            let mut entries = Vec::new();
            for entry in tar.entries().unwrap() {
                let mut entry = entry.unwrap();
                let path = entry.path().unwrap().to_string_lossy().into_owned();
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                entries.push((path, bytes));
            }
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].0, "one.txt");
            assert_eq!(entries[0].1, b"one");
            assert_eq!(entries[1].0, "two.txt");
            assert_eq!(entries[1].1, b"two");
        }
    }

    #[test]
    fn rejects_oversized_svgs_before_allocating_a_pixmap() {
        let too_wide = br##"<svg xmlns="http://www.w3.org/2000/svg" width="9000" height="10"><rect width="9000" height="10"/></svg>"##;
        assert!(matches!(
            convert_bytes("huge.svg", too_wide, OutputFormat::Png),
            Err(ConvertError::SvgTooLarge)
        ));

        let too_many_pixels = br##"<svg xmlns="http://www.w3.org/2000/svg" width="8000" height="8000"><rect width="8000" height="8000"/></svg>"##;
        assert!(matches!(
            convert_bytes("huge.svg", too_many_pixels, OutputFormat::Png),
            Err(ConvertError::SvgTooLarge)
        ));
    }

    #[test]
    fn compresses_gzip_files() {
        let gzip = convert_bytes("note.txt", b"hello", OutputFormat::Gzip).unwrap();
        assert_eq!(gzip[0].name, PathBuf::from("note.txt.gz"));
        assert_eq!(&gzip[0].bytes[0..2], &[0x1f, 0x8b]);

        let mut decoder = flate2::read::GzDecoder::new(Cursor::new(&gzip[0].bytes));
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, b"hello");
    }

    fn wav_fixture() -> Vec<u8> {
        wav_fixture_at(8_000, 8_000)
    }

    fn wav_fixture_at(sample_rate: u32, frames: u32) -> Vec<u8> {
        wav_tone_at(sample_rate, frames, 2, 440.0, 0.5)
    }

    fn wav_tone_at(
        sample_rate: u32,
        frames: u32,
        channels: u16,
        frequency: f64,
        amplitude: f64,
    ) -> Vec<u8> {
        let mut samples = Vec::with_capacity(frames as usize * usize::from(channels));
        for frame in 0..frames {
            let value =
                ((frame as f64 / f64::from(sample_rate) * frequency * std::f64::consts::TAU).sin()
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
    fn wav_channels_at(sample_rate: u32, frames: usize, channels: &[f32]) -> Vec<u8> {
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
    fn wav_24_bit_at(sample_rate: u32, channels: u16, samples: &[i32]) -> Vec<u8> {
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

    fn wav_24_bit_samples(bytes: &[u8]) -> Vec<i32> {
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

    fn wav_float_samples(bytes: &[u8]) -> Vec<f32> {
        let (format, _, _, bits, offset, size) = wav_layout(bytes);
        assert_eq!((format, bits), (3, 32));
        bytes[offset..offset + size]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| f32::from_le_bytes(*bytes))
            .collect()
    }

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

    fn mp3_first_frame_bitrate(bytes: &[u8]) -> Option<u16> {
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

    fn assert_mp3_tone(
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
        let decoded = super::convert_bytes("tone.mp3", mp3, OutputFormat::Wav).unwrap();
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

    struct ParsedWav {
        channels: u16,
        sample_rate: u32,
        bits_per_sample: u16,
        samples: Vec<i16>,
    }

    fn parse_wav(bytes: &[u8]) -> ParsedWav {
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

    fn assert_audible(samples: &[i16]) {
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

    #[test]
    fn native_registry_lists_pdf_archive_and_media_handlers() {
        let registry = native_format_registry();
        assert_eq!(registry.len(), input_format::InputFormat::SELECTABLE.len());
        assert!(registry.iter().all(|handler| !handler.outputs.is_empty()));

        let pdf = registry
            .iter()
            .find(|handler| handler.input == input_format::InputFormat::Pdf)
            .unwrap();
        assert!(pdf.outputs.contains(&OutputFormat::Text));
        assert!(pdf.outputs.contains(&OutputFormat::TextUtf16Le));
        assert!(!pdf.outputs.contains(&OutputFormat::Png));

        let archive = registry
            .iter()
            .find(|handler| handler.input == input_format::InputFormat::Archive)
            .unwrap();
        assert!(archive.outputs.contains(&OutputFormat::Files));
        assert!(archive.outputs.contains(&OutputFormat::Zip));

        let video = registry
            .iter()
            .find(|handler| handler.input == input_format::InputFormat::Video)
            .unwrap();
        assert!(video.outputs.contains(&OutputFormat::Png));
        assert!(video.outputs.contains(&OutputFormat::Jpeg));
        assert!(video.outputs.contains(&OutputFormat::Webp));
        assert!(video.outputs.contains(&OutputFormat::Mp4));
        assert!(video.outputs.contains(&OutputFormat::Webm));
        assert!(!video.outputs.contains(&OutputFormat::Gif));

        let any = registry
            .iter()
            .find(|handler| handler.input == input_format::InputFormat::Any)
            .unwrap();
        assert_eq!(any.outputs.first(), Some(&OutputFormat::Files));
    }

    #[test]
    fn recognizes_the_new_input_formats() {
        assert!(OutputFormat::Png.supports_input("icon.svg"));
        assert!(OutputFormat::Ico.supports_input("icon.ico"));
        assert!(OutputFormat::Hdr.supports_input("photo.dds"));
        assert!(OutputFormat::Wav.supports_input("song.mp3"));
        assert!(OutputFormat::Flac.supports_input("song.m4a"));
        assert!(OutputFormat::Aiff.supports_input("song.aiff"));
        assert!(OutputFormat::Opus.supports_input("song.wav"));
        assert!(OutputFormat::Mp3.supports_input("song.wav"));
        assert!(OutputFormat::Mp3.supports_input("movie.mkv"));
        assert!(OutputFormat::Wav.supports_input("movie.mkv"));
        assert!(OutputFormat::Pbm.supports_input("photo.png"));
        assert!(OutputFormat::Yaml.supports_input("config.toml"));
    }

    #[test]
    fn file_picker_extensions_stay_in_sync_with_conversion_inputs() {
        let extensions = supported_input_extensions();
        let unique: std::collections::HashSet<_> = extensions.iter().copied().collect();
        assert_eq!(
            unique.len(),
            extensions.len(),
            "the file picker lists a duplicate extension"
        );

        for extension in &extensions {
            assert_eq!(
                *extension,
                extension.to_ascii_lowercase().as_str(),
                "file picker extensions must be lowercase"
            );
            assert!(
                OutputFormat::ALL
                    .iter()
                    .any(|format| format.supports_input(&format!("sample.{extension}"))),
                "the file picker offers .{extension}, but no output format accepts it"
            );
        }

        for format in OutputFormat::ALL.iter() {
            if matches!(
                *format,
                OutputFormat::Zip
                    | OutputFormat::Tar
                    | OutputFormat::TarGz
                    | OutputFormat::Gzip
                    | OutputFormat::Files
            ) {
                continue;
            }
            assert!(
                extensions
                    .iter()
                    .any(|extension| format.supports_input(&format!("sample.{extension}"))),
                "{} has no input extension in the file picker",
                format.label()
            );
        }

        for extension in ["zip", "tar", "tgz", "gz"] {
            assert!(
                extensions.contains(&extension),
                "archives must be selectable so they can be extracted"
            );
        }

        for extension in ["ogg", "m4a", "svg"] {
            assert!(
                extensions.contains(&extension),
                "the file picker is missing .{extension}"
            );
        }
    }

    fn jpeg_fixture() -> Vec<u8> {
        let image = image::RgbImage::from_pixel(4, 3, image::Rgb([180, 90, 40]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn converts_images_to_pdf() {
        let converted = convert_bytes("sample.png", &png_fixture(), OutputFormat::Pdf).unwrap();
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].name, PathBuf::from("sample.pdf"));
        let pdf = String::from_utf8_lossy(&converted[0].bytes);
        assert!(pdf.starts_with("%PDF-"));
        assert!(pdf.contains("/Subtype /Image"));
        assert!(pdf.contains("/Filter /FlateDecode"));
        assert!(pdf.contains("/MediaBox [0 0 595.28 841.89]"));
    }

    #[test]
    fn embeds_jpeg_input_without_reencoding() {
        let jpeg = jpeg_fixture();
        let converted = convert_bytes("photo.jpg", &jpeg, OutputFormat::Pdf).unwrap();
        assert!(converted[0]
            .bytes
            .windows(jpeg.len())
            .any(|window| window == jpeg));
        let pdf = String::from_utf8_lossy(&converted[0].bytes);
        assert!(pdf.contains("/Filter /DCTDecode"));
        assert!(pdf.contains("/MediaBox [0 0 595.28 841.89]"));
    }

    #[test]
    fn image_fit_math_centers_and_crops() {
        let contain = super::image_placement(64.0, 48.0, 595.28, 841.89, PdfImageFit::Contain);
        assert!((contain.width - 595.28).abs() < 0.01);
        assert!((contain.height - 446.46).abs() < 0.01);
        assert!(contain.x.abs() < 0.01);
        assert!((contain.y - 197.715).abs() < 0.01);

        let cover = super::image_placement(64.0, 48.0, 792.0, 612.0, PdfImageFit::Cover);
        assert!((cover.width - 816.0).abs() < 0.01);
        assert!((cover.height - 612.0).abs() < 0.01);
        assert!((cover.x + 12.0).abs() < 0.01);
        assert!(cover.y.abs() < 0.01);
    }

    #[test]
    fn applies_pdf_page_orientation_and_image_fit() {
        let converted = convert_bytes_with_options(
            "sample.png",
            &png_fixture(),
            OutputFormat::Pdf,
            ConversionOptions {
                pdf_page_size: PdfPageSize::Letter,
                pdf_orientation: PdfOrientation::Landscape,
                pdf_image_fit: PdfImageFit::Cover,
                ..Default::default()
            },
        )
        .unwrap();
        let pdf = String::from_utf8_lossy(&converted[0].bytes);
        assert!(pdf.contains("/MediaBox [0 0 792 612]"));
        assert!(pdf.contains("792 0 0 792 0 -90 cm"));
    }

    #[test]
    fn converts_text_to_a_paginated_pdf() {
        let text = "Hello, PDF!\n".repeat(120);
        let converted = convert_bytes("notes.txt", text.as_bytes(), OutputFormat::Pdf).unwrap();
        let pdf = String::from_utf8_lossy(&converted[0].bytes);
        assert!(pdf.contains("(Hello, PDF!) Tj"));
        assert!(pdf.contains("/MediaBox [0 0 595.28 841.89]"));
        assert!(pdf.contains("/F1 11 Tf"));
        let pages = pdf.matches("/Type /Page").count() - pdf.matches("/Type /Pages").count();
        assert_eq!(pages, 3);
    }

    #[test]
    fn extracts_text_from_pdf_input() {
        let pdf = convert_bytes(
            "notes.txt",
            b"Hello from PDF\nSecond line\n",
            OutputFormat::Pdf,
        )
        .unwrap();
        let text = convert_bytes("notes.pdf", &pdf[0].bytes, OutputFormat::Text).unwrap();
        let text = String::from_utf8(text[0].bytes.clone()).unwrap();
        assert!(text.contains("Hello from PDF"), "extracted: {text:?}");
        assert!(text.contains("Second line"), "extracted: {text:?}");

        let utf16 = convert_bytes("notes.pdf", &pdf[0].bytes, OutputFormat::TextUtf16Le).unwrap();
        assert_eq!(&utf16[0].bytes[..2], &[0xff, 0xfe]);

        assert!(matches!(
            convert_bytes("notes.pdf", &pdf[0].bytes, OutputFormat::Png),
            Err(ConvertError::Unsupported { .. })
        ));
        assert!(matches!(
            convert_bytes(
                "broken.pdf",
                b"%PDF-1.7\nnot a document",
                OutputFormat::Text
            ),
            Err(ConvertError::Pdf(_))
        ));
    }

    #[test]
    fn applies_pdf_page_font_and_margin_options() {
        let text = "Hello, PDF!\n".repeat(120);
        let converted = convert_bytes_with_options(
            "notes.txt",
            text.as_bytes(),
            OutputFormat::Pdf,
            ConversionOptions {
                pdf_page_size: PdfPageSize::Letter,
                pdf_font_size: 20,
                pdf_margin: 100,
                ..Default::default()
            },
        )
        .unwrap();
        let pdf = String::from_utf8_lossy(&converted[0].bytes);
        assert!(pdf.contains("/MediaBox [0 0 612 792]"));
        assert!(pdf.contains("/F1 20 Tf"));
        let pages = pdf.matches("/Type /Page").count() - pdf.matches("/Type /Pages").count();
        assert_eq!(pages, 6);
    }

    #[test]
    fn clamps_pdf_options_to_supported_ranges() {
        let converted = convert_bytes_with_options(
            "notes.txt",
            b"Hello\n",
            OutputFormat::Pdf,
            ConversionOptions {
                pdf_font_size: 0,
                pdf_margin: 10_000,
                ..Default::default()
            },
        )
        .unwrap();
        let pdf = String::from_utf8_lossy(&converted[0].bytes);
        assert!(pdf.contains("/F1 1 Tf"));
        assert!(pdf.contains("1 0 0 1 144 "));
    }

    #[test]
    fn rejects_pdf_output_for_unstructured_data() {
        let error = convert_bytes("data.csv", b"name\nvalue\n", OutputFormat::Pdf).unwrap_err();
        assert!(matches!(error, ConvertError::Unsupported { .. }));
    }

    fn fixture_path(name: &str) -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name)
    }

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(fixture_path(name)).unwrap()
    }

    /// Returns (format tag, channels, sample rate, bits, data offset, data size).
    fn wav_layout(bytes: &[u8]) -> (u16, u16, u32, u16, usize, usize) {
        assert!(bytes.len() >= 12);
        let mut offset = 12;
        let mut format: (u16, u16, u32, u16) = (0, 0, 0, 0);
        let mut data: (usize, usize) = (0, 0);
        while offset + 8 <= bytes.len() {
            let id = &bytes[offset..offset + 4];
            let size =
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
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

    fn stream_temp_dir(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("convert2it-stream-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn stream_wav(input: &Path, options: ConversionOptions) -> Vec<u8> {
        let mut output = std::io::Cursor::new(Vec::new());
        let cancel = std::sync::atomic::AtomicBool::new(false);
        super::convert_audio_file_to_writer_with_cancel(
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

    #[test]
    fn streaming_cancellation_stops_the_conversion() {
        let directory = stream_temp_dir("cancel");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let mut output = std::io::Cursor::new(Vec::new());
        let error = super::convert_audio_file_to_writer_with_cancel(
            &input,
            "tone.wav",
            OutputFormat::Wav,
            ConversionOptions::default(),
            &mut output,
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(error, ConvertError::Cancelled));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_wav_to_wav_from_a_file() {
        let directory = stream_temp_dir("wav-wav");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let output = stream_wav(&input, ConversionOptions::default());
        let parsed = parse_wav(&output);
        assert_eq!(parsed.samples.len(), 16_000);
        assert_audible(&parsed.samples);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_wav_to_flac_from_a_file() {
        let directory = stream_temp_dir("wav-flac");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let mut output = std::io::Cursor::new(Vec::new());
        super::convert_audio_file_to_writer(
            &input,
            "tone.wav",
            OutputFormat::Flac,
            ConversionOptions::default(),
            &mut output,
        )
        .unwrap();
        let mut reader =
            claxon::FlacReader::new(std::io::Cursor::new(output.into_inner())).unwrap();
        let samples: Vec<i32> = reader.samples().map(Result::unwrap).collect();
        assert_eq!(samples.len(), 16_000);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_wav_to_opus_from_a_file() {
        let directory = stream_temp_dir("wav-opus");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let mut output = std::io::Cursor::new(Vec::new());
        super::convert_audio_file_to_writer(
            &input,
            "tone.wav",
            OutputFormat::Opus,
            ConversionOptions::default(),
            &mut output,
        )
        .unwrap();
        let bytes = output.into_inner();
        assert!(bytes.starts_with(b"OggS"));

        let mut reader = opus_pure::OggOpusReader::new(std::io::Cursor::new(bytes)).unwrap();
        let head = reader.head().clone();
        let channels = head.channel_count as usize;
        let mut decoder = head.decoder(48_000).unwrap();
        let mut trim = opus_pure::Trim::new(&head, 48_000, channels).unwrap();
        let mut block = vec![0.0; opus_pure::MAX_PACKET_SAMPLES * channels];
        let mut frames = 0;
        for packet in reader.packets() {
            let packet = packet.unwrap();
            let decoded = decoder
                .decode(&packet.data, opus_pure::MAX_PACKET_SAMPLES, &mut block)
                .unwrap();
            frames += trim.keep(&packet, &block[..decoded * channels]).len() / channels;
        }
        assert_eq!(frames, 48_000);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_wav_to_aiff_and_au_from_a_file() {
        let directory = stream_temp_dir("wav-aiff-au");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        for target in [OutputFormat::Aiff, OutputFormat::Au] {
            let mut output = std::io::Cursor::new(Vec::new());
            super::convert_audio_file_to_writer(
                &input,
                "tone.wav",
                target,
                ConversionOptions::default(),
                &mut output,
            )
            .unwrap();
            let bytes = output.into_inner();
            if target == OutputFormat::Aiff {
                assert_eq!(&bytes[..4], b"FORM");
                assert_eq!(
                    u32::from_be_bytes(bytes[4..8].try_into().unwrap()) as usize + 8,
                    bytes.len()
                );
                assert_eq!(u32::from_be_bytes(bytes[22..26].try_into().unwrap()), 8_000);
                assert_eq!(
                    u32::from_be_bytes(bytes[42..46].try_into().unwrap()) as usize + 46,
                    bytes.len()
                );
            } else {
                assert_eq!(&bytes[..4], b".snd");
                assert_eq!(
                    u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize + 24,
                    bytes.len()
                );
            }
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_24_bit_flac_from_a_file() {
        let directory = stream_temp_dir("wav-flac-24");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let mut output = std::io::Cursor::new(Vec::new());
        super::convert_audio_file_to_writer(
            &input,
            "tone.wav",
            OutputFormat::Flac,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Bit24,
                ..Default::default()
            },
            &mut output,
        )
        .unwrap();
        let mut reader =
            claxon::FlacReader::new(std::io::Cursor::new(output.into_inner())).unwrap();
        assert_eq!(reader.streaminfo().bits_per_sample, 24);
        assert_eq!(reader.streaminfo().samples, Some(8_000));
        let samples: Vec<i32> = reader.samples().map(Result::unwrap).collect();
        assert_eq!(samples.len(), 16_000);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_wav_at_each_supported_depth_from_a_file() {
        let directory = stream_temp_dir("wav-depth-stream");
        let input = directory.join("quiet.wav");
        let source = vec![1_i32; 512];
        std::fs::write(&input, wav_24_bit_at(48_000, 1, &source)).unwrap();

        for (depth, expected_format) in [
            (AudioBitDepth::Bit16, 1),
            (AudioBitDepth::Bit24, 1),
            (AudioBitDepth::Float32, 3),
        ] {
            let mut output = std::io::Cursor::new(Vec::new());
            super::convert_audio_file_to_writer(
                &input,
                "quiet.wav",
                OutputFormat::Wav,
                ConversionOptions {
                    audio_bit_depth: depth,
                    ..Default::default()
                },
                &mut output,
            )
            .unwrap();
            let bytes = output.into_inner();
            let (format, channels, rate, bits, _, size) = wav_layout(&bytes);
            assert_eq!(
                (format, channels, rate, bits),
                (expected_format, 1, 48_000, depth.bits()),
                "unexpected streamed WAV header for {depth:?}"
            );
            assert_eq!(size, 512 * usize::from(depth.bits() / 8));
            match depth {
                AudioBitDepth::Bit16 => {
                    let parsed = parse_wav(&bytes);
                    let mut distinct = parsed.samples;
                    distinct.sort_unstable();
                    distinct.dedup();
                    assert!(
                        distinct.len() > 1,
                        "streamed 16-bit output was not dithered"
                    );
                }
                AudioBitDepth::Bit24 => {
                    assert!(
                        wav_24_bit_samples(&bytes).iter().all(|sample| *sample == 1),
                        "streamed 24-bit output should be quantized without dither"
                    );
                }
                AudioBitDepth::Float32 => {
                    let decoded = wav_float_samples(&bytes);
                    assert!(decoded
                        .iter()
                        .all(|sample| { (*sample - 1.0 / 8_388_608.0).abs() <= 1.0e-7 }));
                }
            }
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streaming_flac_rejects_float_depth_with_a_typed_error() {
        let directory = stream_temp_dir("wav-flac-float");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let mut output = std::io::Cursor::new(Vec::new());
        let error = super::convert_audio_file_to_writer(
            &input,
            "tone.wav",
            OutputFormat::Flac,
            ConversionOptions {
                audio_bit_depth: AudioBitDepth::Float32,
                ..Default::default()
            },
            &mut output,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::UnsupportedBitDepth {
                format: "FLAC",
                depth: "32-bit float"
            }
        ));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_wav_to_mp3_from_a_file() {
        let directory = stream_temp_dir("wav-mp3");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let mut output = std::io::Cursor::new(Vec::new());
        super::convert_audio_file_to_writer(
            &input,
            "tone.wav",
            OutputFormat::Mp3,
            ConversionOptions::default(),
            &mut output,
        )
        .unwrap();
        let bytes = output.into_inner();
        assert!(bytes.len() > 100);
        assert!(bytes.starts_with(b"ID3") || bytes[0] == 0xFF);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_resampled_wav_from_a_file() {
        let directory = stream_temp_dir("wav-resample");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_fixture()).unwrap();
        let output = stream_wav(
            &input,
            ConversionOptions {
                audio_sample_rate: Some(44_100),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&output);
        assert_eq!(parsed.sample_rate, 44_100);
        assert_eq!(parsed.samples.len(), 88_200);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_flac_and_mp3_input_files() {
        let directory = stream_temp_dir("encoded-input");
        let source = wav_fixture();
        for (name, format) in [
            ("tone.flac", OutputFormat::Flac),
            ("tone.mp3", OutputFormat::Mp3),
        ] {
            let encoded = super::convert_bytes_with_options(
                "tone.wav",
                &source,
                format,
                ConversionOptions::default(),
            )
            .unwrap()
            .remove(0)
            .bytes;
            let input = directory.join(name);
            std::fs::write(&input, encoded).unwrap();
            let output = stream_wav(&input, ConversionOptions::default());
            let parsed = parse_wav(&output);
            assert_eq!(parsed.sample_rate, 8_000);
            if name == "tone.flac" {
                assert_eq!(parsed.samples.len(), 16_000);
                assert_eq!(parsed.samples, parse_wav(&source).samples);
            } else {
                assert!(parsed.samples.len() >= 15_000);
                assert!(parsed.samples.len() <= 17_000);
            }
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_raw_and_matroska_ac3_tracks() {
        for (name, expected_channels) in [("sample-ac3.ac3", 6), ("sample-eac3.mkv", 6)] {
            let input = std::path::Path::new("testdata").join(name);
            let output = stream_wav(&input, ConversionOptions::default());
            let parsed = parse_wav(&output);
            assert_eq!(parsed.channels, expected_channels);
            assert_eq!(
                parsed.samples.len(),
                96_768 * usize::from(expected_channels)
            );
            assert_audible(&parsed.samples);
        }
    }

    #[test]
    fn streams_short_and_downsampled_audio() {
        let directory = stream_temp_dir("resample-edges");
        for (source_rate, frames, target_rate, expected_frames) in [
            (44_100, 44_100, 8_000, 8_000),
            (48_000, 48_000, 44_100, 44_100),
            (8_000, 1, 44_100, 6),
            (8_000, 2, 48_000, 12),
        ] {
            let input = directory.join(format!("tone-{source_rate}-{frames}.wav"));
            std::fs::write(&input, wav_fixture_at(source_rate, frames)).unwrap();
            let output = stream_wav(
                &input,
                ConversionOptions {
                    audio_sample_rate: Some(target_rate),
                    ..Default::default()
                },
            );
            let parsed = parse_wav(&output);
            assert_eq!(parsed.sample_rate, target_rate);
            assert_eq!(parsed.samples.len(), expected_frames * 2);
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_a_selected_opus_track_from_a_file() {
        let output = stream_wav(
            &fixture_path("sample-two-opus-tracks.webm"),
            ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&output);
        assert_eq!(parsed.samples.len(), 48_480);
        assert_audible(&parsed.samples);

        let resampled = stream_wav(
            &fixture_path("sample-two-opus-tracks.webm"),
            ConversionOptions {
                audio_track: Some(1),
                audio_sample_rate: Some(44_100),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&resampled);
        let expected_frames = (48_480.0_f64 * 44_100.0 / 48_000.0).round() as usize;
        assert_eq!(parsed.samples.len(), expected_frames);
        assert_audible(&parsed.samples);
    }

    #[test]
    fn streams_mp4_gapless_audio_from_a_file() {
        let output = stream_wav(
            &fixture_path("sample-aac.m4a"),
            ConversionOptions::default(),
        );
        let parsed = parse_wav(&output);
        assert_eq!(parsed.samples.len(), 88_200);
        assert_audible(&parsed.samples);

        let resampled = stream_wav(
            &fixture_path("sample-aac.m4a"),
            ConversionOptions {
                audio_sample_rate: Some(48_000),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&resampled);
        let expected_frames = (88_200.0_f64 * 48_000.0 / 44_100.0).round() as usize;
        assert_eq!(
            parsed.samples.len(),
            expected_frames * usize::from(parsed.channels)
        );
        assert_audible(&parsed.samples);
    }

    #[test]
    fn streams_ac3_to_stereo_without_clipping() {
        let output = stream_wav(
            &fixture_path("sample-ac3.mkv"),
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&output);
        assert_eq!(parsed.samples.len(), 96_768 * 2);
        assert_audible(&parsed.samples);
        let saturated = parsed
            .samples
            .iter()
            .filter(|sample| **sample == i16::MAX || **sample == i16::MIN)
            .count();
        assert!(
            saturated < parsed.samples.len() / 100,
            "the downmix hard-clipped too many samples"
        );
    }

    #[test]
    fn streams_full_scale_multichannel_without_clipping() {
        let directory = stream_temp_dir("loud-surround");
        let input = directory.join("loud.wav");
        std::fs::write(&input, wav_channels_at(48_000, 2_000, &[1.0; 6])).unwrap();
        let output = stream_wav(
            &input,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&output);
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.samples.len(), 2_000 * 2);
        assert!(
            !parsed.samples.contains(&i16::MIN),
            "a full-scale streaming sample wrapped"
        );
        let peak = parsed
            .samples
            .iter()
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(peak > 30_000, "the streaming downmix was too quiet");
        assert_audible(&parsed.samples);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn streams_multichannel_downmix_keeps_standard_level() {
        let directory = stream_temp_dir("stream-downmix-level");
        let input = directory.join("centre.wav");
        std::fs::write(
            &input,
            wav_channels_at(48_000, 480, &[0.0, 0.0, 0.5, 0.0, 0.0, 0.0]),
        )
        .unwrap();
        let output = stream_wav(
            &input,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&output);
        assert_eq!(parsed.channels, 2);
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.samples.len(), 480 * 2);
        let left = f32::from(parsed.samples[0]) / i16::MAX as f32;
        let right = f32::from(parsed.samples[1]) / i16::MAX as f32;
        assert!(
            (left - 0.3535).abs() < 0.002,
            "streamed centre left was {left}, expected 0.707 * 0.5"
        );
        assert!(
            (right - 0.3535).abs() < 0.002,
            "streamed centre right was {right}, expected 0.707 * 0.5"
        );

        std::fs::write(
            &input,
            wav_channels_at(48_000, 480, &[0.0, 0.0, 0.0, 0.0, 0.5, 0.0]),
        )
        .unwrap();
        let output = stream_wav(
            &input,
            ConversionOptions {
                audio_channels: Some(AudioChannels::Stereo),
                ..Default::default()
            },
        );
        let parsed = parse_wav(&output);
        let left = f32::from(parsed.samples[0]) / i16::MAX as f32;
        let right = f32::from(parsed.samples[1]) / i16::MAX as f32;
        assert!(
            (left - 0.3535).abs() < 0.002,
            "streamed surround left was {left}, expected 0.707 * 0.5"
        );
        assert!(
            right.abs() < 0.002,
            "a back-left signal leaked into the right channel: {right}"
        );
        std::fs::remove_dir_all(&directory).unwrap();
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
        let error = super::convert_audio_file_to_writer(
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
        let error = super::convert_audio_file_to_writer(
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
        super::convert_audio_file_to_writer(
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
        let aac_info = super::mp4_gapless_info(super::mp4_find(&aac, b"moov").unwrap(), 0)
            .expect("the AAC edit list is present");
        assert_eq!(aac_info.skip, 1_024, "AAC priming was not read");
        assert_eq!(aac_info.limit, Some(88_200));
        assert!(aac_info.limit_is_presentation);

        let m4a = fixture("sample.m4a");
        let m4a_info = super::mp4_gapless_info(super::mp4_find(&m4a, b"moov").unwrap(), 0)
            .expect("the audio track duration is present");
        assert_eq!(m4a_info.skip, 0);
        assert_eq!(m4a_info.limit, Some(135_168));
        assert!(!m4a_info.limit_is_presentation);

        let opus = fixture("sample-vp9-opus.mp4");
        let opus_info = super::mp4_gapless_info(super::mp4_find(&opus, b"moov").unwrap(), 1)
            .expect("the Opus edit list is present");
        assert_eq!(opus_info.skip, 336);
        assert_eq!(opus_info.limit, Some(96_048));
        assert!(opus_info.limit_is_presentation);

        let ac3 = fixture("sample-ac3.mp4");
        let ac3_info = super::mp4_gapless_info(super::mp4_find(&ac3, b"moov").unwrap(), 0)
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

    fn rms(samples: &[f32]) -> f64 {
        (samples
            .iter()
            .map(|sample| f64::from(*sample) * f64::from(*sample))
            .sum::<f64>()
            / samples.len() as f64)
            .sqrt()
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
        super::convert_audio_file_to_writer(
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
    fn streams_mp3_at_the_requested_bitrate() {
        let directory = stream_temp_dir("mp3-bitrate");
        let input = directory.join("tone.wav");
        std::fs::write(&input, wav_tone_at(48_000, 48_000, 1, 1_000.0, 0.5)).unwrap();
        let encode = |bitrate| {
            let mut output = Cursor::new(Vec::new());
            super::convert_audio_file_to_writer(
                &input,
                "tone.wav",
                OutputFormat::Mp3,
                ConversionOptions {
                    mp3_bitrate: bitrate,
                    ..Default::default()
                },
                &mut output,
            )
            .unwrap();
            output.into_inner()
        };
        let low = encode(Mp3Bitrate::Kbps128);
        let high = encode(Mp3Bitrate::Kbps320);
        assert!(
            high.len() > low.len() * 2,
            "streaming 320 kbps produced {} bytes but 128 kbps produced {}",
            high.len(),
            low.len()
        );
        assert_mp3_tone(&low, 48_000, 1, 48_000, Mp3Bitrate::Kbps128);
        assert_mp3_tone(&high, 48_000, 1, 48_000, Mp3Bitrate::Kbps320);

        std::fs::write(&input, wav_tone_at(44_100, 44_100, 2, 1_000.0, 0.5)).unwrap();
        let stereo = encode(Mp3Bitrate::Kbps256);
        assert_mp3_tone(&stereo, 44_100, 2, 44_100, Mp3Bitrate::Kbps256);
        std::fs::remove_dir_all(&directory).unwrap();
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

    fn tiff_with_orientations(orientations: &[u16]) -> Vec<u8> {
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

    fn tiff_with_primary_and_thumbnail_orientations(primary: u16, thumbnail: u16) -> Vec<u8> {
        fn push_ifd(tiff: &mut Vec<u8>, orientation: u16, next_ifd: u32) {
            tiff.extend_from_slice(&1_u16.to_le_bytes());
            tiff.extend_from_slice(&0x0112_u16.to_le_bytes());
            tiff.extend_from_slice(&3_u16.to_le_bytes());
            tiff.extend_from_slice(&1_u32.to_le_bytes());
            tiff.extend_from_slice(&orientation.to_le_bytes());
            tiff.extend_from_slice(&[0, 0]);
            tiff.extend_from_slice(&next_ifd.to_le_bytes());
        }

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II*\0");
        tiff.extend_from_slice(&8_u32.to_le_bytes());
        let thumbnail_ifd = 8 + 2 + 12 + 4;
        push_ifd(&mut tiff, primary, thumbnail_ifd);
        push_ifd(&mut tiff, thumbnail, 0);
        tiff
    }

    fn jpeg_with_exif_orientations(image: &image::DynamicImage, orientations: &[u16]) -> Vec<u8> {
        jpeg_with_exif_tiff(image, &tiff_with_orientations(orientations))
    }

    fn jpeg_with_exif_tiff(image: &image::DynamicImage, tiff: &[u8]) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        image
            .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut cursor,
                100,
            ))
            .unwrap();
        let jpeg = cursor.into_inner();
        let mut result = jpeg[..2].to_vec();
        let mut segment = b"Exif\0\0".to_vec();
        segment.extend_from_slice(tiff);
        result.extend_from_slice(&[0xFF, 0xE1]);
        result.extend_from_slice(&((segment.len() + 2) as u16).to_be_bytes());
        result.extend_from_slice(&segment);
        result.extend_from_slice(&jpeg[2..]);
        result
    }

    fn assert_rgb_near(actual: [u8; 3], expected: [u8; 3], tolerance: u8, label: &str) {
        for (channel, (actual, expected)) in actual.iter().zip(expected).enumerate() {
            assert!(
                actual.abs_diff(expected) <= tolerance,
                "{label}: channel {channel} got {actual}, expected {expected}"
            );
        }
    }

    fn expected_composite(source: [u8; 3], alpha: u8, background: [u8; 3]) -> [u8; 3] {
        let alpha = u16::from(alpha);
        let inverse = 255 - alpha;
        std::array::from_fn(|channel| {
            ((u16::from(source[channel]) * alpha + u16::from(background[channel]) * inverse + 127)
                / 255) as u8
        })
    }

    #[test]
    fn preserves_jpeg_exif_without_double_rotating() {
        let source = image::RgbImage::from_fn(8, 8, |x, _| {
            if x < 4 {
                image::Rgb([255, 0, 0])
            } else {
                image::Rgb([0, 0, 255])
            }
        });
        let input = jpeg_with_exif_orientations(&image::DynamicImage::ImageRgb8(source), &[6]);

        let rotated = convert_bytes("photo.jpg", &input, OutputFormat::Jpeg).unwrap();
        assert_eq!(exif_orientation(&rotated[0].bytes), Some(1));
        assert_eq!(
            image::load_from_memory(&rotated[0].bytes).unwrap().width(),
            8
        );

        let unrotated = convert_bytes_with_options(
            "photo.jpg",
            &input,
            OutputFormat::Jpeg,
            ConversionOptions {
                image_apply_exif_orientation: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(exif_orientation(&unrotated[0].bytes), Some(6));
    }

    #[test]
    fn exif_orientation_rotates_corners_and_ignores_duplicate_or_invalid_tags() {
        let mut source = image::RgbImage::new(16, 16);
        for (x, y, pixel) in source.enumerate_pixels_mut() {
            *pixel = match (x < 8, y < 8) {
                (true, true) => image::Rgb([255, 0, 0]),
                (false, true) => image::Rgb([0, 255, 0]),
                (true, false) => image::Rgb([0, 0, 255]),
                (false, false) => image::Rgb([255, 255, 0]),
            };
        }
        let source = image::DynamicImage::ImageRgb8(source);

        let oriented = jpeg_with_exif_orientations(&source, &[6]);
        assert_eq!(super::exif_orientation(&oriented), Some(6));
        let converted = convert_bytes("photo.jpg", &oriented, OutputFormat::Png).unwrap();
        let decoded = image::load_from_memory(&converted[0].bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (16, 16));
        let pixels = decoded.to_rgb8();
        assert_rgb_near(pixels.get_pixel(4, 4).0, [0, 0, 255], 35, "top-left");
        assert_rgb_near(pixels.get_pixel(12, 4).0, [255, 0, 0], 35, "top-right");
        assert_rgb_near(pixels.get_pixel(4, 12).0, [255, 255, 0], 35, "bottom-left");
        assert_rgb_near(pixels.get_pixel(12, 12).0, [0, 255, 0], 35, "bottom-right");

        let plain = jpeg_with_exif_orientations(&source, &[]);
        assert_eq!(super::exif_orientation(&plain), None);
        let plain = image::load_from_memory(
            &convert_bytes("plain.jpg", &plain, OutputFormat::Png).unwrap()[0].bytes,
        )
        .unwrap()
        .to_rgb8();
        assert_rgb_near(plain.get_pixel(4, 4).0, [255, 0, 0], 35, "plain top-left");

        let invalid = jpeg_with_exif_orientations(&source, &[9]);
        assert_eq!(super::exif_orientation(&invalid), Some(9));
        let invalid = image::load_from_memory(
            &convert_bytes("invalid.jpg", &invalid, OutputFormat::Png).unwrap()[0].bytes,
        )
        .unwrap()
        .to_rgb8();
        assert_rgb_near(
            invalid.get_pixel(4, 4).0,
            [255, 0, 0],
            35,
            "invalid top-left",
        );

        let duplicate = jpeg_with_exif_orientations(&source, &[6, 6]);
        assert_eq!(super::exif_orientation(&duplicate), None);
        let duplicate = image::load_from_memory(
            &convert_bytes("duplicate.jpg", &duplicate, OutputFormat::Png).unwrap()[0].bytes,
        )
        .unwrap()
        .to_rgb8();
        assert_rgb_near(
            duplicate.get_pixel(4, 4).0,
            [255, 0, 0],
            35,
            "duplicate top-left",
        );

        let disabled = convert_bytes_with_options(
            "photo.jpg",
            &oriented,
            OutputFormat::Png,
            ConversionOptions {
                image_apply_exif_orientation: false,
                ..Default::default()
            },
        )
        .unwrap();
        let disabled = image::load_from_memory(&disabled[0].bytes).unwrap();
        assert_eq!((disabled.width(), disabled.height()), (16, 16));
        assert_rgb_near(
            disabled.to_rgb8().get_pixel(4, 4).0,
            [255, 0, 0],
            35,
            "disabled top-left",
        );
    }

    #[test]
    fn exif_orientation_ignores_thumbnail_ifd_orientation() {
        let mut source = image::RgbImage::new(2, 1);
        source.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        source.put_pixel(1, 0, image::Rgb([0, 0, 255]));
        let source = image::DynamicImage::ImageRgb8(source);
        let jpeg =
            jpeg_with_exif_tiff(&source, &tiff_with_primary_and_thumbnail_orientations(6, 3));

        assert_eq!(super::exif_orientation(&jpeg), Some(6));
        let converted = convert_bytes("photo.jpg", &jpeg, OutputFormat::Png).unwrap();
        let decoded = image::load_from_memory(&converted[0].bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1, 2));
        let pixels = decoded.to_rgb8();
        let top = pixels.get_pixel(0, 0).0;
        let bottom = pixels.get_pixel(0, 1).0;
        assert!(top[0] > 200 && top[2] < 60, "top pixel is {top:?}");
        assert!(
            bottom[2] > 200 && bottom[0] < 60,
            "bottom pixel is {bottom:?}"
        );
    }

    #[test]
    fn applies_exif_orientation_when_loading_images() {
        let mut image = image::RgbImage::new(2, 1);
        image.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        image.put_pixel(1, 0, image::Rgb([0, 0, 255]));
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .unwrap();

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II*\0");
        tiff.extend_from_slice(&8_u32.to_le_bytes());
        tiff.extend_from_slice(&1_u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112_u16.to_le_bytes());
        tiff.extend_from_slice(&3_u16.to_le_bytes());
        tiff.extend_from_slice(&1_u32.to_le_bytes());
        tiff.extend_from_slice(&6_u16.to_le_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&0_u32.to_le_bytes());
        let mut segment = b"Exif\0\0".to_vec();
        segment.extend_from_slice(&tiff);
        let mut oriented = jpeg[..2].to_vec();
        oriented.extend_from_slice(&[0xFF, 0xE1]);
        oriented.extend_from_slice(&((segment.len() + 2) as u16).to_be_bytes());
        oriented.extend_from_slice(&segment);
        oriented.extend_from_slice(&jpeg[2..]);

        assert_eq!(super::exif_orientation(&oriented), Some(6));
        let converted = convert_bytes("photo.jpg", &oriented, OutputFormat::Png).unwrap();
        let decoded = image::load_from_memory(&converted[0].bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1, 2));
        let pixels = decoded.to_rgb8();
        let top = pixels.get_pixel(0, 0).0;
        let bottom = pixels.get_pixel(0, 1).0;
        assert!(top[0] > 200 && top[2] < 60, "top pixel is {top:?}");
        assert!(
            bottom[2] > 200 && bottom[0] < 60,
            "bottom pixel is {bottom:?}"
        );

        let unrotated = convert_bytes_with_options(
            "photo.jpg",
            &oriented,
            OutputFormat::Png,
            ConversionOptions {
                image_apply_exif_orientation: false,
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = image::load_from_memory(&unrotated[0].bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 1));
    }

    #[test]
    fn composites_transparency_onto_the_configured_background() {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            1,
            1,
            image::Rgba([255, 0, 0, 0]),
        ))
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();

        let bmp = convert_bytes_with_options(
            "clear.png",
            &png,
            OutputFormat::Bmp,
            ConversionOptions {
                image_background: [0, 0, 255],
                ..Default::default()
            },
        )
        .unwrap();
        let decoded = image::load_from_memory(&bmp[0].bytes).unwrap().to_rgb8();
        assert_eq!(decoded.get_pixel(0, 0).0, [0, 0, 255]);

        let default = convert_bytes("clear.png", &png, OutputFormat::Bmp).unwrap();
        let decoded = image::load_from_memory(&default[0].bytes)
            .unwrap()
            .to_rgb8();
        assert_eq!(decoded.get_pixel(0, 0).0, [255, 255, 255]);
    }

    #[test]
    fn composites_transparency_into_jpeg_and_bmp_with_two_backgrounds() {
        let transparent = image::Rgba([255, 0, 0, 0]);
        let semi_transparent = image::Rgba([200, 100, 50, 128]);
        let opaque = image::Rgba([10, 200, 30, 255]);
        let mut rgba = image::RgbaImage::new(24, 8);
        for (x, _, pixel) in rgba.enumerate_pixels_mut() {
            *pixel = match x {
                0..=7 => transparent,
                8..=15 => semi_transparent,
                _ => opaque,
            };
        }
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        for background in [[0, 0, 255], [12, 34, 56]] {
            let expected = [
                expected_composite([255, 0, 0], 0, background),
                expected_composite([200, 100, 50], 128, background),
                expected_composite([10, 200, 30], 255, background),
            ];
            for (target, format, tolerance) in [
                (OutputFormat::Jpeg, image::ImageFormat::Jpeg, 25),
                (OutputFormat::Bmp, image::ImageFormat::Bmp, 0),
            ] {
                let converted = convert_bytes_with_options(
                    "clear.png",
                    &png,
                    target,
                    ConversionOptions {
                        image_background: background,
                        jpeg_quality: Some(100),
                        ..Default::default()
                    },
                )
                .unwrap();
                let decoded = image::load_from_memory_with_format(&converted[0].bytes, format)
                    .unwrap()
                    .to_rgb8();
                assert_eq!((decoded.width(), decoded.height()), (24, 8));
                for (x, expected, label) in [
                    (4, expected[0], "transparent area"),
                    (12, expected[1], "semi-transparent area"),
                    (20, expected[2], "opaque area"),
                ] {
                    assert_rgb_near(decoded.get_pixel(x, 4).0, expected, tolerance, label);
                }
            }
        }
    }

    #[test]
    fn alpha_preserving_targets_ignore_the_configured_background() {
        let pixels = [
            image::Rgba([255, 0, 0, 0]),
            image::Rgba([200, 100, 50, 128]),
            image::Rgba([10, 200, 30, 255]),
        ];
        let mut rgba = image::RgbaImage::new(3, 1);
        for (x, pixel) in pixels.iter().enumerate() {
            rgba.put_pixel(x as u32, 0, *pixel);
        }
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        for (target, format) in [
            (OutputFormat::Png, image::ImageFormat::Png),
            (OutputFormat::Webp, image::ImageFormat::WebP),
        ] {
            let converted = convert_bytes_with_options(
                "alpha.png",
                &png,
                target,
                ConversionOptions {
                    image_background: [1, 2, 3],
                    ..Default::default()
                },
            )
            .unwrap();
            let decoded = image::load_from_memory_with_format(&converted[0].bytes, format).unwrap();
            assert!(
                decoded.color().has_alpha(),
                "{target:?} output lost its alpha channel"
            );
            let decoded = decoded.to_rgba8();
            assert_eq!((decoded.width(), decoded.height()), (3, 1));
            for (x, expected) in pixels.iter().enumerate() {
                assert_eq!(*decoded.get_pixel(x as u32, 0), *expected);
            }
        }
    }

    #[test]
    fn animated_gif_converts_to_a_single_frame() {
        use image::codecs::gif::{GifEncoder, Repeat};

        let mut gif = Vec::new();
        let mut encoder = GifEncoder::new(&mut gif);
        encoder.set_repeat(Repeat::Infinite).unwrap();
        encoder
            .encode_frames([
                image::Frame::new(image::RgbaImage::from_pixel(
                    2,
                    2,
                    image::Rgba([255, 0, 0, 255]),
                )),
                image::Frame::new(image::RgbaImage::from_pixel(
                    2,
                    2,
                    image::Rgba([0, 0, 255, 255]),
                )),
            ])
            .unwrap();
        drop(encoder);

        let converted = convert_bytes("animated.gif", &gif, OutputFormat::Png).unwrap();
        let decoded = image::load_from_memory(&converted[0].bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 2));
        let pixel = decoded.to_rgb8().get_pixel(0, 0).0;
        assert!(
            pixel[0] > 200 && pixel[2] < 60,
            "the first GIF frame was not retained: {pixel:?}"
        );
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
