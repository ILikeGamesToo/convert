# Fuzzing

The native converter parses untrusted files, including the hand-written
Matroska `DiscardPadding` and MP4 edit-list walkers. These targets check that a
malformed file returns an error instead of panicking or looping forever.

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run convert_bytes
cargo +nightly fuzz run containers
```

`convert_bytes` covers the audio, image, PDF, and data conversion paths.
`containers` probes every accepted input extension through both the audio and
image decoders.
