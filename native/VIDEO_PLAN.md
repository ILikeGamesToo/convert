# Native video plan

This document records the settled design for the native video slice. It is the
source of truth for later work; the implementation and tests follow it.

## Scope

The slice covers automatic container conversion, animation and
frame/thumbnail extraction:

- MP4/MOV/M4V/WebM/MKV/AVI/WMV/MPEG input to MP4, WebM or an animated GIF.
- Stream-copy remux when the target container already takes every source
  codec, and ffmpeg re-encoding of the streams that it does not.
- PNG/JPEG/WebP thumbnails from video.
- Animated GIF input to MP4, and MP3/M4A input to an audio-only MP4, without a
  second input kind.
- Existing output validation behavior.

There is no frame-accurate editing, trimming, scaling or cropping control,
subtitle handling, hardware encoder selection, or user-facing quality control.

## Settled decisions

### One route enum, no user-facing mode

`VideoRoute` is the internal route enum. `VideoRouteInfo.route` uses the same
enum, and the CLI's `--list-video` prints it. `VideoRouteInfo.fallback` names
the route taken when the source codecs are outside the route's copy lists.

The route is selected automatically. Nothing user-facing gains a mode:

- no mode field on `ConversionOptions`;
- no GUI "Handling" control;
- no CLI flag for choosing a mode.

`ConversionOptions` stays unchanged and deserialization-compatible with
settings already saved by users. The encoder settings are fixed, documented
below, and not user-selectable yet.

### Routing rules

1. A GIF target always encodes: a GIF holds no frame of a video codec.
2. A container whose codecs the native parser reads (MP4/MOV/M4V, MKV, WebM) is
   stream-copied when every stream's codec is on the target's copy list.
3. Every other case encodes. That includes a parsed container with a codec
   outside the list, a codec the parser does not recognise, and every
   AVI/WMV/MPEG file, whose codecs only ffmpeg reads.

Current copy allowances:

- MP4 target: H.264/HEVC/VP9/AV1 video with AAC/Opus/MP3/AC-3/E-AC-3 audio.
- WebM target: VP8/VP9/AV1 video with Opus/Vorbis audio.

The copy decision is made per stream. An MP4 with AV1 video and PCM audio copies
the video and encodes only the audio, so a source is never re-encoded further
than the target requires.

### Encoder settings

MP4 encodes H.264 with libx264 at `-preset medium -crf 23` and AAC at 128 kbps.
A build without libx264 (an LGPL-only ffmpeg) is retried once with the built-in
MPEG-4 Part 2 encoder at `-q:v 3`; every other failure is reported as it is.
WebM encodes VP9 with libvpx at `-crf 32 -b:v 0` and Opus at 128 kbps.

Both video encoders are given `-pix_fmt yuv420p`, which no player rejects, and
`scale=trunc(iw/2)*2:trunc(ih/2)*2`, because yuv420p cannot hold an odd width or
height and a 321x241 source would otherwise fail outright.

### Animated GIF

A GIF from a video is an animation, not a thumbnail: the image targets keep
producing a still frame at the configured time, and the GIF target produces the
whole clip.

The encode is two passes. `palettegen=stats_mode=diff` derives a palette from
the frames the GIF will hold, and `paletteuse=dither=sierra2_4a` maps each frame
onto it. A single-pass GIF falls back to ffmpeg's default 32-colour web palette,
which is what makes a naive video-to-GIF conversion look banded.

Defaults, and why:

- 12 frames a second. Motion still reads, and dropping to two fifths of a 30 fps
  source is the largest size saving after the palette.
- 480 px wide at most, never upscaled. A GIF is palette data with no inter-frame
  compression worth the name, so a 1080p source is megabytes a second.
- Infinite looping (`-loop 0`), which is what the format is used for.

### Non-video sources into MP4

A GIF and an MP3 or an M4A are not video containers, but the MP4 target holds
them, so they are routed through the same conversion instead of gaining a
decoder or a second kind. `video_container_source(target, extension)` is the one
place that says which sources a container target accepts; the registry, the
CLI, the converter and the route table all read it.

- **GIF → MP4** encodes: the container holds no frame of an image, so the
  images kind's frames are encoded to H.264 with the same settings as any other
  MP4 encode, at the source size, with the GIF's own per-frame delays. A GIF is
  read here and nowhere else in the video path; there is no GIF parser.
- **MP3/M4A → MP4** copies: `-map 0:a -map 0:v:0? -c copy
  -disposition:v:0 attached_pic`. The audio stream is carried bit for bit, and a
  cover picture stays album art instead of becoming a one-frame video track. A
  source with no picture exercises the optional map and nothing is added.
- Only MP4 is offered. A WebM target would need Opus or Vorbis audio and VP9
  frames, which is a re-encode of both, not a container job.

### Stream preservation

A remux command maps all video and audio streams with `-map 0:v? -map 0:a?` and
applies `-c copy`. Video and audio codecs, dimensions and track data are
preserved; no encoder is selected.

An encode maps the same streams, so multiple audio tracks survive (each is
encoded), and a source with no audio track produces output with none.

### Errors and output validation

- Missing ffmpeg remains `FfmpegNotFound`, including for a pair that would have
  to be encoded: without the binary there is no conversion.
- A failed remux or encode surfaces ffmpeg's status and stderr
  (`FfmpegFailed`).
- Empty or invalid output is rejected (`FfmpegEmptyOutput` or
  `FfmpegInvalidOutput`). A GIF is validated by its `GIF87a`/`GIF89a` header.
- `UnsupportedVideoCodec` now covers the one case a re-encode cannot fix: a
  parsed source with no video stream at all, for a target that needs one.
  `codec` is `"none"` there.

### Progress and cancellation

`VideoControl` is used by the encode path. The command carries
`-progress pipe:1 -nostats`, and each `out_time_us` ffmpeg writes becomes a
`VideoProgress`.

`total` stays `None`, so `VideoProgress::fraction()` returns `None`. Filling it
would mean parsing the `Duration:` line, which ffmpeg only prints at `info`
verbosity; running the encode at that verbosity buries the error line of a failed
encode under stream and format banner lines. Reporting the encoded time without a
fraction is the honest trade.

Cancellation polls the flag every 200 ms, kills the child, waits for it, and
returns `ConvertError::Cancelled`. A remux and a thumbnail remain single fast
invocations and ignore the hooks.

## Fixtures and tests

Committed fixtures live in `native/testdata/`; their exact generation commands
and provenance are in `native/testdata/README.md`.

- `sample-vp9-opus.webm`, `sample-vp9-opus.mp4` and
  `sample-vp9-opus-roundtrip.webm`: VP9/Opus media for stream-copy round trips.
- `sample-odd-h264.mp4`: 321x241 H.264/AAC, used for MP4 remux (the odd
  dimensions are why an encode needs the scale filter).
- `sample-even-h264.mp4`: 320x240 H.264/AAC, used for the H.264→WebM encode and
  the H.264→GIF animation.
- `sample-multitrack-h264.mp4`: two H.264 video tracks, used for the
  multi-track H.264→WebM encode.
- The AVI fixture the real-ffmpeg encode test needs is generated by the test
  itself (MPEG-4 Part 2 video with PCM audio, both built into ffmpeg), so no
  binary is committed for it.

Coverage:

- VP9/Opus MP4→WebM and WebM→MP4 remux using `-c copy`;
- a real-ffmpeg MP4→WebM→MP4 round trip: the VP9/Opus codecs survive, which is
  what proves the copy path ran instead of an encoder;
- an H.264/AAC MP4→WebM conversion that encodes VP9 and Opus instead of
  returning an error;
- an AVI→MP4 conversion, checked with `ffprobe` for H.264 (or the MPEG-4 Part 2
  fallback), AAC, 320x240 and both stream kinds;
- an AVI→GIF conversion that decodes to exactly 12 frames at 320x240, with
  different images in frames 0 and 6 and `nb_frames=12` from `ffprobe`;
- the GIF argument pipeline: `palettegen=stats_mode=diff`, `fps=12`, the width
  cap, `paletteuse=dither=sierra2_4a` and `-loop 0`;
- a missing-libx264 MP4 failure that retries with MPEG-4 Part 2;
- progress callbacks and a cancel that kills a sleeping stub ffmpeg;
- PNG/JPEG/WebP thumbnails at 0 and 500 ms, with a scaled width and a decoded
  image check;
- an incompatible pair with no ffmpeg returning `FfmpegNotFound`;
- empty and invalid ffmpeg output errors;
- CLI and GUI end-to-end remux/thumbnail tests;
- a GIF→MP4 conversion checked with `ffprobe` for H.264 at the source size and
  about one frame per source frame, and a real animation whose duration is
  kept;
- MP3 and M4A into MP4 with the audio stream hashed before and after the
  conversion, so a re-encode or a dropped stream cannot pass, plus the attached
  picture asserted where the source has one and absent where it does not;
- the stub argument pipeline for both new routes: libx264 for a GIF and
  `-c copy` with the attached-picture map for audio;
- `--list-video` listing the copy lists, the encode routes, each row's
  fallback, and the three new source rows.

Real-FFmpeg tests skip with an explicit message when ffmpeg is unavailable.

## Deferred

- User-selectable quality, codec choice and hardware encoders.
- A progress fraction: it needs the input duration, which this path does not
  probe.
- Bundling ffmpeg and the associated licensing work.
- Frame-accurate trimming/editing and scaling/cropping controls.
- Animated GIF from an animated GIF, and animated WebP.
- Audio into WebM, and FLAC/WAV/AIFF/Opus into MP4: each needs a re-encode to
  the container's codec, and a quality control that does not exist yet.
- Animated GIF from an animated GIF with a transparency-preserving path (APNG
  or a second palette pass keyed on the source's alpha), and animated WebP.
