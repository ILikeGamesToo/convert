# Native test fixtures

These files are used only by the `native` conversion tests to verify OGG, Opus, and M4A decoding, audio extraction from MP4 and WebM videos, a VP9 + Opus WebM → MP4 → WebM remux round trip, H.264/AAC fixtures for MP4 remux and typed rejection of incompatible WebM stream copies, and font container conversions for TrueType (glyf) and OpenType (CFF) fonts, plus AVIF and HEIC decoding, and PDF page rasterisation of a
document Chrome wrote.
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
- `sample-avif-quad.avif` — a 64×64 AVIF still image: four solid 32×32 quadrants (red, green, blue, white), encoded losslessly at 8-bit 4:4:4. This is the known-content fixture: the test decodes it and compares each quadrant against the source colour within a tolerance of 3 per channel. FFmpeg's `color` source writes those quadrants as 253/0/0, 0/254/0, 0/0/254 and 255/255/255, so the expected values are those and not pure 255. Generated with FFmpeg 9.1 (libaom-av1) from FFmpeg's own sources, so it contains no third-party media:

  ```sh
  ffmpeg -y -f lavfi -i color=c=0xFF0000:s=32x32:d=1 \
    -f lavfi -i color=c=0x00FF00:s=32x32:d=1 \
    -f lavfi -i color=c=0x0000FF:s=32x32:d=1 \
    -f lavfi -i color=c=0xFFFFFF:s=32x32:d=1 \
    -filter_complex "[0][1]hstack[t];[2][3]hstack[b];[t][b]vstack" \
    -frames:v 1 -update 1 quad.png
  ffmpeg -y -i quad.png -c:v libaom-av1 -still-picture 1 \
    -aom-params lossless=1 -pix_fmt yuv444p -f avif sample-avif-quad.avif
  ```
- `sample-avif-quad-10bit.avif` — the same four quadrants coded at 10-bit 4:4:4, 368 bytes. It exercises the high-bit-depth decode path: the decoder produces a 16-bit image, and the test compares the quadrants against the same expected colours within the same tolerance:

  ```sh
  ffmpeg -y -i quad.png -c:v libaom-av1 -still-picture 1 \
    -aom-params lossless=1 -pix_fmt yuv444p10le -f avif sample-avif-quad-10bit.avif
  ```
- `sample-avif-alpha-irot.avif` — a 256×256 image of black letters on a translucent white background with an `irot` quarter turn and an alpha auxiliary image item carrying the translucency, 10 KB. Decodes to 256×512 with partial alpha, so it covers the `auxC` alpha merge and the transformative properties. Copied from libavif's [`tests/data/abc_color_irot_alpha_irot.avif`](https://github.com/AOMediaCodec/libavif/blob/main/tests/data/abc_color_irot_alpha_irot.avif), whose [`tests/data/README.md`](https://github.com/AOMediaCodec/libavif/blob/main/tests/data/README.md) licenses it "same as libavif", the [2-Clause BSD License](https://github.com/AOMediaCodec/libavif/blob/main/LICENSE).
- `sample-avif-grid.avif` — an 80×80 image stored as a 2×2 `grid` of 40×40 tiles with a non-grid alpha item, 2.4 KB. Covers grid derivation and tile assembly. Copied from libavif's [`tests/data/color_grid_alpha_nogrid.avif`](https://github.com/AOMediaCodec/libavif/blob/main/tests/data/color_grid_alpha_nogrid.avif), licensed "same as libavif" (2-Clause BSD).
- `sample-avif-sequence.avif` — a 150×150 still plus an image sequence track (the `avis` brand), 1.2 KB. The native decoder presents the still and ignores the track, which is what this fixture pins. Copied from libavif's [`tests/data/colors-animated-8bpc.avif`](https://github.com/AOMediaCodec/libavif/blob/main/tests/data/colors-animated-8bpc.avif), licensed "same as libavif" (2-Clause BSD).
- `sample-heic-photo.heic` — the reference real-world HEIC: a 1280×854 HEVC photo with no alpha and no EXIF orientation, 701 KB. The test decodes it and compares three sampled pixels against FFmpeg 9.1's decode of the same file (tolerance 8 per channel, which is colour-conversion rounding), and asserts every pixel is opaque because the file has no alpha channel. Copied from libheif's [`examples/example.heic`](https://github.com/strukturag/libheif/blob/master/examples/example.heic); that directory carries its own [`examples/COPYING`](https://github.com/strukturag/libheif/blob/master/examples/COPYING), the MIT licence, which is why this file is not under libheif's LGPL.
- `sample-heic-alpha-orientation.heic` — a 64×32 HEIC: the left half opaque red, the right half green at alpha 128, with EXIF orientation 6, 738 bytes. Decoding it must produce a 32×64 image with the red half on top and the green half (alpha 128) at the bottom, which is what covers the alpha auxiliary item and the EXIF rotation in one fixture. Generated from that known content with `heifer` 0.5.1's own encoder (`heifer` and `heifer-hevc-enc` are MIT OR Apache-2.0), then the two bytes of the orientation tag were set to 6, because the encoder writes orientation 1 for pixels that are already upright. There is no FFmpeg route to a HEIC file: FFmpeg 9.1 has no HEIF muxer and its AVIF muxer refuses an HEVC codestream:

  ```sh
  # heifer's encoder writes the container and the HEVC codestream; the source is
  # built in memory as described above, then the orientation SHORT is set to 6.
  # The committed bytes are what the tests read, so no encoder is needed to run them.
  ```
- `sample-chrome.pdf` — a one-page A4 PDF written by Chrome 158's own PDF
  backend from `pdf-source.html`, 55 KB. The page carries Abel text in two
  sizes, an inline SVG with five coloured shapes, and a bordered table, so the
  rasteriser is exercised on a document that no code in this crate produced: the
  app's own text writer embeds no font and draws no vector art. Abel is embedded
  as a subset (`FontFile2`, CID-keyed TrueType), and Source Code Pro, the
  monospace line, arrives as a `Type3` font whose glyphs are content streams,
  so the fixture covers both a font program and outlined glyphs. The test renders
  it at one pixel per point and asserts the 594 x 841 page size (A4 is
  594.96 x 841.92 points, floored as the web handler floors it), an opaque alpha
  channel on every pixel, ink on more than 5% of the page, at least 100 pixels of
  the accent red `#b3261e` from the source, and more than 50 distinct colours,
  which is what anti-aliased glyph and vector edges produce and a page of filled
  boxes would not. Printed from the committed HTML with:

  ```sh
  chrome --headless=new --disable-gpu --no-sandbox --no-pdf-header-footer \
    --virtual-time-budget=10000 --user-data-dir=/tmp/chrome-print \
    --print-to-pdf=sample-chrome.pdf pdf-source.html
  ```

  Re-printing reproduces the page but not the bytes, because Chrome stamps a
  creation date and a document ID into the file. The text and the vector art are
  this repository's own, the embedded subset is the SIL Open Font License 1.1
  Abel already in this directory, and the writer is Chrome's, so the file carries
  no third-party content of its own.
- `pdf-source.html` — the page `sample-chrome.pdf` is printed from, kept so the
  fixture can be regenerated. It loads `abel-regular.ttf` and
  `source-code-pro-regular.otf` from this directory through `@font-face`.
