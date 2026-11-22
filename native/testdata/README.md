# Native test fixtures

These files are used only by the `native` conversion tests to verify OGG, Opus, and M4A decoding plus audio extraction from MP4 and WebM videos.

- `piano.ogg` — Ogg Vorbis, CC0 1.0. Source: [`themankindproject/audiofp`](https://github.com/themankindproject/audiofp/blob/main/tests/assets/piano.ogg); its [`CREDITS.md`](https://github.com/themankindproject/audiofp/blob/main/tests/assets/CREDITS.md) marks it CC0 and generated for that project.
- `sample.m4a` — AAC in an audio-only M4A, 22 KB. Remuxed from the audio track of WPT's [`media/A4.mp4`](https://github.com/web-platform-tests/wpt/blob/master/media/A4.mp4) with the video track removed and the `esds` SLConfigDescriptor set to the MP4 predefined value. WPT is licensed under the [3-Clause BSD License](https://github.com/web-platform-tests/wpt/blob/master/LICENSE.md).
- `sample-video.mp4` — H.264 video with an AAC audio track, 53 KB. Copied from WPT's [`media/A4.mp4`](https://github.com/web-platform-tests/wpt/blob/master/media/A4.mp4). WPT is licensed under the [3-Clause BSD License](https://github.com/web-platform-tests/wpt/blob/master/LICENSE.md).
- `sample-opus.webm` — VP9 video with an Opus audio track, 99 KB. Copied from Chromium's [`media/test/data/bear-vp9-opus.webm`](https://github.com/chromium/chromium/blob/main/media/test/data/bear-vp9-opus.webm). Chromium is licensed under the [3-Clause BSD License](https://chromium.googlesource.com/chromium/src/+/main/LICENSE).
- `sample-opus.mp4` — AV1 video with an Opus audio track, 49 KB. Copied from Chromium's [`media/test/data/bear-av1-opus.mp4`](https://github.com/chromium/chromium/blob/main/media/test/data/bear-av1-opus.mp4). Chromium is licensed under the [3-Clause BSD License](https://chromium.googlesource.com/chromium/src/+/main/LICENSE).
