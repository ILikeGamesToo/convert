use std::process::Command;

#[test]
fn list_formats_reports_the_metadata_carried_by_each_target() {
    let output = Command::new(env!("CARGO_BIN_EXE_convert2it"))
        .arg("--list-formats")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "--list-formats failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("images keep EXIF and ICC profiles into jpg, png, webp and tiff"),
        "{stdout}"
    );
    assert!(
        stdout.contains("audio keeps title, artist, album, track"),
        "{stdout}"
    );
    assert!(stdout.contains("EPUB ebook\tepub\t"), "{stdout}");
    assert!(stdout.contains("HTML document\thtml\t"), "{stdout}");
    assert!(stdout.contains("Markdown document\tmd\t"), "{stdout}");
    // The GIF and audio rows reach the MP4 container and nothing else new.
    for row in ["GIF image\tgif\t", "MP3 audio\tmp3\t", "M4A audio\tm4a\t"] {
        let line = stdout
            .lines()
            .find(|line| line.starts_with(row))
            .unwrap_or_else(|| panic!("{row} is missing from\n{stdout}"));
        let outputs: Vec<&str> = line.split('\t').nth(2).unwrap().split(',').collect();
        assert!(outputs.contains(&"mp4"), "{line}");
        assert!(!outputs.contains(&"webm"), "{line}");
    }
    // All three SVG targets write `.svg`, so the registry prints the `--format`
    // spelling that selects one of them.
    assert!(stdout.contains("svg-trace"), "{stdout}");
    assert!(stdout.contains("svg-page"), "{stdout}");
    // The registry rows stay three tab-separated fields, so tooling that reads
    // them keeps working; the metadata note is a separate paragraph.
    for line in stdout.lines() {
        if line.starts_with("Metadata:")
            || line.starts_with("Video:")
            || line.starts_with("Office:")
            || line.starts_with("SVG:")
            || line.is_empty()
        {
            continue;
        }
        assert_eq!(
            line.split('\t').count(),
            3,
            "a registry row changed shape: {line}"
        );
    }
}
