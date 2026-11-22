#![no_main]

use convert_to_it_native::{audio_tracks, convert_bytes, OutputFormat};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for extension in [
        "webm", "mkv", "mp4", "m4a", "aac", "ogg", "opus", "flac", "wav", "mp3", "ac3", "eac3",
        "aiff", "au", "png", "jpg", "gif", "webp", "tiff", "svg", "pdf",
    ] {
        let name = format!("input.{extension}");
        let _ = audio_tracks(data, extension);
        let _ = convert_bytes(&name, data, OutputFormat::Wav);
        let _ = convert_bytes(&name, data, OutputFormat::Png);
    }
});
