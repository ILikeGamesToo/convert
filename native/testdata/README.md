# Native test fixtures

These files are used only by the `native` conversion tests to verify OGG, Opus, and M4A decoding, audio extraction from MP4 and WebM videos, a VP9 + Opus WebM → MP4 → WebM remux round trip, H.264/AAC fixtures for MP4 remux and typed rejection of incompatible WebM stream copies, and font container conversions for TrueType (glyf) and OpenType (CFF) fonts.

- `piano.ogg` — Ogg Vorbis, CC0 1.0. Source: [`themankindproject/audiofp`](https://github.com/themankindproject/audiofp/blob/main/tests/assets/piano.ogg); its [`CREDITS.md`](https://github.com/themankindproject/audiofp/blob/main/tests/assets/CREDITS.md) marks it CC0 and generated for that project.
- `sample.m4a` — AAC in an audio-only M4A, 22 KB. Remuxed from the audio track of WPT's [`media/A4.mp4`](https://github.com/web-platform-tests/wpt/blob/master/media/A4.mp4) with the video track removed and the `esds` SLConfigDescriptor set to the MP4 predefined value. WPT is licensed under the [3-Clause BSD License](https://github.com/web-platform-tests/wpt/blob/master/LICENSE.md).
- `sample-video.mp4` — H.264 video with an AAC audio track, 53 KB. Copied from WPT's [`media/A4.mp4`](https://github.com/web-platform-tests/wpt/blob/master/media/A4.mp4). WPT is licensed under the [3-Clause BSD License](https://github.com/web-platform-tests/wpt/blob/master/LICENSE.md).
- `sample-odd-h264.mp4` — 321×241 H.264 High 4:4:4 Predictive (`yuv444p`) with one 48 kHz mono AAC track, 1 s. Generated with FFmpeg 9.1 (libx264 and the native AAC encoder) from FFmpeg's own `color` and `sine` sources, so it contains no third-party media. This is the odd-dimension MP4 remux fixture; tests must not need libx264 because the file is committed:

  ```sh
  ffmpeg -y -f lavfi -i color=c=black:s=321x241:r=30:d=1 \
    -f lavfi -i sine=frequency=440:sample_rate=48000:duration=1 \
    -c:v libx264 -pix_fmt yuv444p -profile:v high444 -crf 18 \
    -c:a aac -b:a 96k -ac 1 -shortest sample-odd-h264.mp4
  ```
- `sample-even-h264.mp4` — 320×240 H.264 High (`yuv420p`) with one 48 kHz mono AAC track, 1 s. The even-dimension counterpart, generated with the same FFmpeg 9.1 sources and encoder settings as the odd fixture; used for the H.264→WebM rejection test:

  ```sh
  ffmpeg -y -f lavfi -i color=c=black:s=320x240:r=30:d=1 \
    -f lavfi -i sine=frequency=440:sample_rate=48000:duration=1 \
    -c:v libx264 -pix_fmt yuv420p -profile:v high -crf 18 \
    -c:a aac -b:a 96k -ac 1 -shortest sample-even-h264.mp4
  ```
- `sample-multitrack-h264.mp4` — two copied H.264 video tracks plus the AAC track from `sample-even-h264.mp4`. Used to prove that an MP4 with more than one video track is rejected instead of being stream-copied:

  ```sh
  ffmpeg -y -i sample-even-h264.mp4 -map 0:v:0 -map 0:v:0 -map 0:a:0 \
    -c copy sample-multitrack-h264.mp4
  ```
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
- `sample-two-opus-tracks.webm` — two 48 kHz mono Opus tracks (1 s and 1.01 s) whose final blocks carry different Matroska DiscardPadding values (13.5 ms and 3.5 ms). Generated with FFmpeg 9.1 (libopus) from its own `sine` source:

  ```sh
  ffmpeg -f lavfi -i sine=frequency=440:duration=1 \
    -f lavfi -i sine=frequency=880:duration=1.01 \
    -map 0:a -map 1:a -c:a libopus -b:a 96k -ar 48000 -ac 1 \
    sample-two-opus-tracks.webm
  ```
- `sample-aac.m4a` — 2 s of 440 Hz mono AAC-LC at 44.1 kHz with an MP4 edit list (1024-sample priming, 88,200-sample duration). Used to verify that the edit list trims AAC priming and padding. Generated with FFmpeg 9.1's native AAC encoder:

  ```sh
  ffmpeg -f lavfi -i sine=frequency=440:sample_rate=44100:duration=2 \
    -c:a aac -b:a 128k sample-aac.m4a
  ```
- `sample-aac-51.m4a` — 2 s of 440 Hz AAC-LC in 5.1 at 48 kHz. The track is listed with six channels; Symphonia's AAC decoder only handles up to stereo, so this fixture also verifies the clear multichannel-AAC error:

  ```sh
  ffmpeg -f lavfi -i sine=frequency=440:sample_rate=48000:duration=2 \
    -af "pan=5.1|FL=c0|FR=c0|FC=c0|LFE=c0|BL=c0|BR=c0" \
    -c:a aac -b:a 384k sample-aac-51.m4a
  ```
- `sample-ac3.ac3`, `sample-ac3.mkv`, and `sample-ac3.mp4` — the same 2 s of 440 Hz AC-3 in 5.1 at 48 kHz, as an elementary stream, in Matroska, and in MP4. The three containers exercise the raw syncframe walker and the packet paths:

  ```sh
  ffmpeg -f lavfi -i sine=frequency=440:sample_rate=48000:duration=2 \
    -af "pan=5.1|FL=c0|FR=c0|FC=c0|LFE=c0|BL=c0|BR=c0" \
    -c:a ac3 -b:a 384k sample-ac3.ac3
  ffmpeg -f lavfi -i sine=frequency=440:sample_rate=48000:duration=2 \
    -af "pan=5.1|FL=c0|FR=c0|FC=c0|LFE=c0|BL=c0|BR=c0" \
    -c:a ac3 -b:a 384k sample-ac3.mkv
  ffmpeg -f lavfi -i sine=frequency=440:sample_rate=48000:duration=2 \
    -af "pan=5.1|FL=c0|FR=c0|FC=c0|LFE=c0|BL=c0|BR=c0" \
    -c:a ac3 -b:a 384k sample-ac3.mp4
  ```
- `sample-eac3.mkv` — 2 s of 440 Hz E-AC-3 in 5.1 at 48 kHz:

  ```sh
  ffmpeg -f lavfi -i sine=frequency=440:sample_rate=48000:duration=2 \
    -af "pan=5.1|FL=c0|FR=c0|FC=c0|LFE=c0|BL=c0|BR=c0" \
    -c:a eac3 -b:a 384k sample-eac3.mkv
  ```
- `sample-ac3-center.ac3` — 2 s of 440 Hz AC-3 in 5.1 with audio only in the centre channel. A stereo downmix that keeps just the first two channels would be silent, so this fixture proves the standard centre/surround coefficients are applied:

  ```sh
  ffmpeg -f lavfi -i sine=frequency=440:sample_rate=48000:duration=2 \
    -af "pan=5.1|FC=c0" -c:a ac3 -b:a 192k sample-ac3-center.ac3
  ```
- `abel-regular.ttf` — Abel Regular, 35 KB, TrueType (`glyf`) outlines. Used as the main font fixture: every container conversion is round-tripped and the result is rendered back with `resvg`, comparing ink against the source font. Copied from Google Fonts' [`ofl/abel/Abel-Regular.ttf`](https://github.com/google/fonts/blob/main/ofl/abel/Abel-Regular.ttf), which is licensed under the [SIL Open Font License 1.1](https://github.com/google/fonts/blob/main/ofl/abel/OFL.txt):
- `source-code-pro-regular.otf` — Source Code Pro Regular, 128 KB, CFF (`OTTO`) outlines. The CFF counterpart of the fixture above, so both sfnt flavours are exercised. Copied from Adobe's [`adobe-fonts/source-code-pro`](https://github.com/adobe-fonts/source-code-pro/blob/release/OTF/SourceCodePro-Regular.otf), licensed under the [SIL Open Font License 1.1](https://github.com/adobe-fonts/source-code-pro/blob/release/LICENSE.md):
- `abel-regular.woff2` — 9 KB, Abel's latin subset exactly as Google Fonts serves it, including the WOFF2 `glyf`/`loca` transform. This is the only fixture whose glyph data has been pre-compressed by someone else's encoder, so it proves the decoder reverses the transform rather than only reading files this crate wrote. Downloaded from the `src` URL in the Google Fonts CSS response for `family=Abel`; same SIL Open Font License 1.1 as the TTF. It is a subset, so its cmap covers U+0000-00FF, not the full character set of `abel-regular.ttf`.

  ```sh
  curl -L -o abel-regular.ttf https://raw.githubusercontent.com/google/fonts/main/ofl/abel/Abel-Regular.ttf
  curl -L -o source-code-pro-regular.otf https://raw.githubusercontent.com/adobe-fonts/source-code-pro/release/OTF/SourceCodePro-Regular.otf
  curl -L -o abel-regular.woff2 https://fonts.gstatic.com/s/abel/v18/MwQ5bhbm2POE2V9BPQ.woff2
  ```
