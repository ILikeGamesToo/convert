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
    // The registry rows stay three tab-separated fields, so tooling that reads
    // them keeps working; the metadata note is a separate paragraph.
    for line in stdout.lines() {
        if line.starts_with("Metadata:")
            || line.starts_with("Video:")
            || line.starts_with("Office:")
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
