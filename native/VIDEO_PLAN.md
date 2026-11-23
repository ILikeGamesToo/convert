# Native video plan

This document records the settled design for the native video slice. It is the
source of truth for later work; the implementation and tests follow it.

## Scope

The first slice covers automatic container conversion and frame/thumbnail
extraction:

- MP4/WebM/MKV input to MP4 or WebM.
- Stream-copy remux only. Incompatible codec/container pairs return a typed,
  user-facing error; the app never re-encodes as a fallback.
- PNG/JPEG/WebP thumbnails from video.
- Existing output validation behavior.

There is no full transcoding, frame-accurate editing, trimming, scaling,
cropping, subtitle handling or general codec conversion in this slice.

## Settled decisions

### One route enum, no user-facing mode

`VideoRoute` is the internal route enum. `VideoRouteInfo.route` uses the same
enum, and the CLI's `--list-video` prints it. The current registry advertises
only `Remux`; `Transcode` is reserved for a future slice and is never selected
by the conversion entry points.

The route is selected automatically. Nothing user-facing gains a mode:

- no mode field on `ConversionOptions`;
- no GUI "Handling" control;
- no CLI flag for choosing a mode.

`ConversionOptions` stays unchanged and deserialization-compatible with
settings already saved by users. A user never chooses remux versus transcode,
so there are no invalid or duplicate mode combinations to validate.

### Routing rules

1. If `validate_remux` accepts the source/container pair, remux it with
   `ffmpeg -c copy`.
2. Otherwise return the typed unsupported-codec error from `validate_remux`.

Current remux allowances:

- MP4 target: H.264/HEVC/VP9/AV1 video with AAC/Opus/MP3/AC-3/E-AC-3 audio.
- WebM target: VP8/VP9/AV1 video with Opus/Vorbis audio.

The error names the stream, codec and target container and tells the user to
use a compatible source file. The app does not weaken validation or fall back
to re-encoding.

### Stream preservation

The remux command maps all video and audio streams:

```text
-map 0:v? -map 0:a? -c copy
```

Video and audio codecs, dimensions and track data are preserved. No encoder is
selected, and `-c copy` applies to every mapped stream.

### Errors and output validation

- Missing ffmpeg remains `FfmpegNotFound`.
- A failed remux surfaces ffmpeg's status and stderr (`FfmpegFailed`).
- Empty or invalid output is rejected (`FfmpegEmptyOutput` or
  `FfmpegInvalidOutput`).
- Incompatible codec/container pairs return `UnsupportedVideoCodec`.
- Cancellation between files returns `ConvertError::Cancelled` and removes
  temporary files; a single synchronous remux is not interrupted mid-process.

### Progress and cancellation

The current slice is synchronous. `VideoControl`, `VideoProgress` and
`convert_bytes_with_video_control` remain public API hooks for a future
transcode path, but remux and thumbnail conversions ignore them. The GUI
progress modal advances per file; it does not show subprocess progress.

## Fixtures and tests

Committed fixtures live in `native/testdata/`; their exact generation commands
and provenance are in `native/testdata/README.md`.

- `sample-vp9-opus.webm`, `sample-vp9-opus.mp4` and
  `sample-vp9-opus-roundtrip.webm`: VP9/Opus media for stream-copy round trips.
- `sample-odd-h264.mp4`: 321x241 H.264/AAC, used for MP4 remux and H.264/WebM
  rejection.
- `sample-even-h264.mp4`: 320x240 H.264/AAC, used for MP4 remux and
  H.264/WebM rejection.
- `sample-multitrack-h264.mp4`: two H.264 video tracks, used for typed
  rejection.

Coverage:

- VP9/Opus MP4→WebM and WebM→MP4 remux using `-c copy`;
- a real-ffmpeg MP4→WebM→MP4 round trip with matching VP9/Opus streams and
  dimensions;
- PNG/JPEG/WebP thumbnails at 0 and 500 ms, with a scaled width and a decoded
  image check;
- H.264/AAC MP4→WebM and multi-track MP4→WebM typed errors, with no ffmpeg
  invocation;
- H.264/AAC MP4→MP4 remux;
- empty and invalid ffmpeg output errors;
- CLI and GUI end-to-end remux/thumbnail tests;
- `--list-video` listing only stream-copy routes.

Real-FFmpeg tests skip with an explicit message when ffmpeg is unavailable.

## Deferred

- Full transcoding, including H.264/AAC→VP9/Opus and user-selectable quality.
- Progress and cancellation inside a long ffmpeg operation.
- Bundling ffmpeg and the associated licensing work.
- Frame-accurate trimming/editing.
- AVIF/HEIC input and metadata preservation.
