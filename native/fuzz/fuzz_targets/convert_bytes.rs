#![no_main]

use convert_to_it_native::{audio_tracks, convert_bytes, OutputFormat};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for (name, target) in [
        ("input.webm", OutputFormat::Wav),
        ("input.mp4", OutputFormat::Flac),
        ("input.png", OutputFormat::Png),
        ("input.jpg", OutputFormat::Pdf),
        ("input.json", OutputFormat::Json),
    ] {
        let _ = convert_bytes(name, data, target);
    }
    let _ = audio_tracks(data, "mkv");
    let _ = audio_tracks(data, "ac3");
});
