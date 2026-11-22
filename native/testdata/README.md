# Native test fixtures

These files are used only by the `native` conversion tests to verify OGG, Opus, and M4A decoding plus audio extraction from MP4 and WebM videos, including a VP9 + Opus WebM → MP4 → WebM remux round trip.

- `piano.ogg` — Ogg Vorbis, CC0 1.0. Source: [`themankindproject/audiofp`](https://github.com/themankindproject/audiofp/blob/main/tests/assets/piano.ogg); its [`CREDITS.md`](https://github.com/themankindproject/audiofp/blob/main/tests/assets/CREDITS.md) marks it CC0 and generated for that project.
- `sample.m4a` — AAC in an audio-only M4A, 22 KB. Remuxed from the audio track of WPT's [`media/A4.mp4`](https://github.com/web-platform-tests/wpt/blob/master/media/A4.mp4) with the video track removed and the `esds` SLConfigDescriptor set to the MP4 predefined value. WPT is licensed under the [3-Clause BSD License](https://github.com/web-platform-tests/wpt/blob/master/LICENSE.md).
- `sample-video.mp4` — H.264 video with an AAC audio track, 53 KB. Copied from WPT's [`media/A4.mp4`](https://github.com/web-platform-tests/wpt/blob/master/media/A4.mp4). WPT is licensed under the [3-Clause BSD License](https://github.com/web-platform-tests/wpt/blob/master/LICENSE.md).
- `sample-opus.webm` — VP9 video with an Opus audio track, 99 KB. Copied from Chromium's [`media/test/data/bear-vp9-opus.webm`](https://github.com/chromium/chromium/blob/main/media/test/data/bear-vp9-opus.webm). Chromium is licensed under the [3-Clause BSD License](https://chromium.googlesource.com/chromium/src/+/main/LICENSE).
- `sample-opus.mp4` — AV1 video with an Opus audio track, 49 KB. Copied from Chromium's [`media/test/data/bear-av1-opus.mp4`](https://github.com/chromium/chromium/blob/main/media/test/data/bear-av1-opus.mp4). Chromium is licensed under the [3-Clause BSD License](https://chromium.googlesource.com/chromium/src/+/main/LICENSE).
- `sample-vp9-opus.webm`, `sample-vp9-opus.mp4`, and `sample-vp9-opus-roundtrip.webm` — 2 s of generated 320×240 VP9 video and a 440 Hz Opus sine tone, remuxed WebM → MP4 → WebM without re-encoding. Generated with FFmpeg 9.1 (libvpx 1.17.0, libopus) from FFmpeg's own `testsrc2` and `sine` sources, so they contain no third-party media:

  ```sh
  ffmpeg -f lavfi -i testsrc2=size=320x240:rate=30:duration=2 \
    -f lavfi -i sine=frequency=440:sample_rate=48000:duration=2 \
    -c:v libvpx-vp9 -b:v 300k -pix_fmt yuv420p \
    -c:a libopus -b:a 96k -shortest sample-vp9-opus.webm
  ffmpeg -i sample-vp9-opus.webm -c copy sample-vp9-opus.mp4
  ffmpeg -i sample-vp9-opus.mp4 -c copy sample-vp9-opus-roundtrip.webm
  ```
