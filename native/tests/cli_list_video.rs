use std::process::Command;

#[test]
fn list_video_reports_only_stream_copy_routes() {
    let output = Command::new(env!("CARGO_BIN_EXE_convert2it"))
        .arg("--list-video")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "--list-video failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("mp4\twebm\tremux\tvp8,vp9,av1\topus,vorbis"),
        "{stdout}"
    );
    assert!(
        stdout.contains("webm\tmp4\tremux\th264,hevc,vp9,av1\taac,opus,mp3,ac3,eac3"),
        "{stdout}"
    );
    assert!(!stdout.contains("\ttranscode\t"), "{stdout}");
    assert!(
        stdout.contains("incompatible codec/container pairs are rejected"),
        "{stdout}"
    );
    assert!(stdout.contains("Routing is automatic"), "{stdout}");
}
