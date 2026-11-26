use std::process::Command;

#[test]
fn list_video_reports_copy_lists_and_encode_routes() {
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
        stdout.starts_with("source\ttarget\troute\tvideo\taudio\tfallback\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains("mp4\twebm\tremux\tvp8,vp9,av1\topus,vorbis\ttranscode"),
        "{stdout}"
    );
    assert!(
        stdout.contains("webm\tmp4\tremux\th264,hevc,vp9,av1\taac,opus,mp3,ac3,eac3\ttranscode"),
        "{stdout}"
    );
    // Only ffmpeg reads AVI, so there is no codec list to copy and nothing to
    // fall back from.
    assert!(stdout.contains("avi\tmp4\ttranscode\t\t\tnone"), "{stdout}");
    assert!(stdout.contains("mp4\tgif\ttranscode\t\t\tnone"), "{stdout}");
    // Non-video sources: a GIF is encoded, an audio-only stream is copied.
    assert!(stdout.contains("gif\tmp4\ttranscode\t\t\tnone"), "{stdout}");
    assert!(
        stdout.contains("mp3\tmp4\tremux\t\taac,opus,mp3,ac3,eac3\tnone"),
        "{stdout}"
    );
    assert!(
        stdout.contains("m4a\tmp4\tremux\t\taac,opus,mp3,ac3,eac3\tnone"),
        "{stdout}"
    );
    for row in stdout.lines().filter(|line| line.contains('\t')) {
        let columns: Vec<&str> = row.split('\t').collect();
        assert_eq!(columns.len(), 6, "wrong column count in {row:?}");
        if columns[2] == "transcode" {
            assert!(
                columns[3].is_empty() && columns[4].is_empty(),
                "a transcode row must not advertise a copy list: {row:?}"
            );
        }
    }
    assert!(stdout.contains("any other pair is re-encoded"), "{stdout}");
    assert!(stdout.contains("Routing is automatic"), "{stdout}");
}
