// SPDX-License-Identifier: GPL-2.0-or-later

use std::path::{Path, PathBuf};
use std::process::Command;

use convert_to_it_native::VideoToolConfig;

fn real_ffmpeg() -> Option<PathBuf> {
    let path = VideoToolConfig::default().resolve()?;
    let output = Command::new(&path).arg("-version").output().ok()?;
    let version = String::from_utf8_lossy(&output.stdout);
    (output.status.success() && version.contains("ffmpeg version")).then_some(path)
}

/// A test that cannot run has to say so where the log will show it. libtest
/// captures the `print!` macros for a test that passes, so an `eprintln!`
/// notice never leaves the harness and the skip reads as `ok`. Writing to the
/// handle directly is not captured. Setting `CONVERT2IT_REQUIRE_FFMPEG=1` turns
/// the skip into a failure, so a job that is supposed to have ffmpeg cannot go
/// green without running these.
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
    let output = Command::new(ffmpeg)
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

#[test]
fn real_ffmpeg_cli_reports_zero_byte_thumbnail_output() {
    let Some(ffmpeg) = ffmpeg_for_real_test("CLI integration test") else {
        return;
    };
    let directory =
        std::env::temp_dir().join(format!("convert2it-cli-empty-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let input = directory.join("synthetic.mp4");
    generate_real_video(&ffmpeg, &input);
    let output_directory = directory.join("out");
    std::fs::create_dir_all(&output_directory).unwrap();

    // Seeking past the end of the one-second clip makes ffmpeg exit 0 with
    // zero bytes on stdout. The app must turn that into an error, not success.
    let output = Command::new(env!("CARGO_BIN_EXE_convert2it"))
        .args([
            "--format",
            "png",
            "--ffmpeg",
            ffmpeg.to_str().unwrap(),
            "--video-frame-ms",
            "10000",
            "--thumbnail-width",
            "160",
            "--output",
            output_directory.to_str().unwrap(),
            input.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "CLI unexpectedly succeeded with empty ffmpeg output"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ffmpeg produced no output"),
        "CLI stderr did not explain the empty output: {stderr}"
    );
    assert!(
        std::fs::read_dir(&output_directory)
            .unwrap()
            .next()
            .is_none(),
        "CLI wrote an output file despite the empty ffmpeg output"
    );

    std::fs::remove_dir_all(&directory).unwrap();
}
