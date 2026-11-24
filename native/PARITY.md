# Native / web parity

The native app is intentionally a focused subset of the browser app. This file
records the current boundary and the registry that keeps it honest.

Run this from `native/` to print the machine-readable native registry:

```sh
cargo run --bin convert2it -- --list-formats
```

The same data is available from the library as `native_format_registry()`.
`InputFormat::SELECTABLE` is the source of truth for the input side, and
`OutputFormat::supports_input` is the source of truth for the output side.

## Native coverage

| Area | Inputs | Native outputs | Notes |
| --- | --- | --- | --- |
| Raster images | PNG, JPEG, WebP, GIF, BMP, TIFF, ICO, PNM, TGA, QOI, HDR, Farbfeld, DDS, SVG, AVIF, HEIC/HEIF | PNG, JPEG, WebP, GIF, BMP, TIFF, ICO, PBM/PGM/PPM/PAM, TGA, QOI, HDR, Farbfeld, PDF | SVG is rasterized with a size/pixel limit. DDS is input-only. AVIF and HEIC are decoded in-process by pure-Rust crates, with no external tool and no C library to install. |
| Text and data | TXT, CSV, JSON, YAML, TOML | PDF, text encodings, CSV, JSON, YAML, TOML | CSV/JSON/YAML/TOML conversions use the same tabular rules as the web handlers. |
| PDF | PDF | text encodings | Text extraction uses `pdf-extract`. PDF page rasterization is not implemented. |
| Audio | WAV, FLAC, MP3, OGG/Opus, M4A/AAC, AIFF, AC-3/E-AC-3, video containers | WAV, FLAC, AIFF, AU, Opus, MP3 | Multichannel layouts are downmixed explicitly; video inputs are used for their audio track only. MP3-to-MP3 keeps its ID3v2 tag; FLAC-to-FLAC keeps Vorbis comments and pictures. |
| Archives | ZIP, TAR, TAR.GZ/TGZ, GZIP | ZIP, TAR, TAR.GZ, GZIP, extracted files | Extraction rejects absolute paths, `..`, drive prefixes, duplicates, oversized entries and excessive entry counts. |
| Video | MP4, WebM, MKV | MP4, WebM, PNG/JPEG/WebP thumbnails, audio outputs | MP4↔WebM remux uses `ffmpeg -c copy` and copies all video/audio streams. Incompatible codec/container pairs return a typed error; nothing is transcoded as a fallback. Frame/thumbnail extraction uses `ffmpeg` and then the normal image encoder. Requires `ffmpeg` on PATH, `CONVERT2IT_FFMPEG`/`FFMPEG`, or the CLI `--ffmpeg` override. General transcoding and frame-accurate editing are not implemented. |

| Fonts | TTF, OTF, WOFF, WOFF2, TTC/OTC | TTF, OTF, WOFF, WOFF2, SVG | Every font table is copied verbatim and only the container is rewritten, so outlines, advance widths, units-per-em, the cmap and the name table survive a conversion. TTF and OTF output relabel the container and keep the source outline format, so they differ only in file name; there is no `glyf`↔`CFF ` outline conversion (see Known gaps). A `.ttc`/`.otc` collection is converted from its first face. WOFF2 input reverses the `glyf`/`loca` transform; WOFF2 output uses the null transform the format allows. SVG output is the web handler's font specimen (one `<glyph>` per mapped codepoint), not a rasterisable text document. |

## Web-only handlers not yet ported

The web app (`src/handlers/index.ts`) also has dedicated handlers for vector
tracing, EPUB/AZW3/MOBI, office documents, Typst/Pandoc, 3D models,
SQLite, 7z/RAR/LZH, comic archives, game formats, MIDI/module music, and
PDF rasterization. Those handlers depend on JavaScript/WASM libraries or
external tools that do not have a verified native equivalent in this crate.

## AVIF and HEIC

Both are decoded in-process by pure-Rust crates: there is no external tool, no C
library and nothing for a user to install. Detection is by the ISO BMFF `ftyp`
brand, so both are also selectable in the format list and reach every image
target and PDF like any other image.

**AVIF** — the container, item derivations and colour conversion come from
`gamut-avif`; the AV1 codestream from `rav1d`, the Rust port of `dav1d`, built
with `default-features = false` because its `asm` feature assembles vendored x86
assembly with NASM, which would be a build-time tool outside Rust. 8-bit content
decodes to RGBA8 and 10/12-bit content to RGBA16, which the outputs that can
carry it (PNG, TIFF, Farbfeld) keep. Alpha auxiliary items, `grid` tile assembly,
the `clap`/`irot`/`imir` transformative properties and the AVIF `Exif` item
(including orientation) are handled. What is not: an AVIF whose first `colr` is
an ICC profile instead of CICP `nclx` colour is refused with a typed error
rather than decoded with the wrong colours, because CICP colour is what the
decoder converts; chroma is upsampled nearest-neighbour for 4:2:0/4:2:2
sources; an image *sequence* (`avis`) contributes its primary still only.

**HEIC/HEIF** — `heifer` decodes the container and the HEVC codestream, HEVC
included, in pure Rust and with no `unsafe`. 8-bit content decodes to RGBA8 and
10/12-bit content to RGBA16. The `clap`/`irot`/`imir` container transforms, grid
tiles, alpha auxiliary items and the EXIF orientation tag are applied, and
samples are colour-managed to sRGB from an embedded ICC profile when there is one
(phone photos are usually Display P3). What is not: an HDR file whose transfer is
PQ or HLG is refused with a typed error rather than flattened (`heifer` has a
separate gain-map API that this app does not drive); an image *sequence*
contributes its primary image only; and an EXIF orientation is ignored for a file
that also carries an `irot`/`imir` transform, because the decoder has already
applied that one and rotating again would turn the image twice.

**Why HEIC ships, and what that accepts** — the blocker used to be read as two
problems: an LGPL/C decoder, and HEVC patent licensing. The first is gone:
`heifer` is MIT OR Apache-2.0, has no `unsafe` and no C, and decodes real
iPhone-style photos correctly. The second is real and was **accepted knowingly by
the project owner**: HEVC decoding carries patent licensing that a static binary
redistributing a decoder walks into, and a permissive copyright licence from the
decoder author grants no patent rights. That decision is recorded here
deliberately. Do not re-open it as though it were an oversight, and do not add a
second HEVC implementation without the same decision.

## Known blockers

- **Video:** remux and thumbnails require an external `ffmpeg`; none is
  bundled. The test suite exercises the subprocess path with a stub ffmpeg.
  The real-ffmpeg tests print an explicit skip message and return without
  failing when no binary is available. In the verification environment, Gyan
  FFmpeg 9.1 was installed with
  `winget install --id Gyan.FFmpeg --exact --silent --accept-package-agreements
  --accept-source-agreements --disable-interactivity`. MP4→WebM→MP4 remux and
  PNG/JPEG/WebP thumbnails were exercised through the library, the GUI worker
  and the CLI; outputs were checked with `ffprobe`/image decoding. A real
  thumbnail request past the end of the one-second clip (`-ss 10.000`, exact
  app argv) made ffmpeg 9.1 exit 0 with zero bytes on stdout and empty stderr;
  `ffprobe` rejects that zero-byte file as invalid data. The thumbnail guard
  checks `output.stdout.is_empty()` before decoding, so the library returned
  `FfmpegEmptyOutput`, the GUI worker surfaced `ffmpeg produced no output`,
  and the CLI exited non-zero with `convert2it: ffmpeg produced no output`.
  The stub tests still cover empty and invalid remux files. Remux is
  codec-copy only: MP4→WebM accepts VP8/VP9/AV1 video with Opus/Vorbis audio,
  and WebM→MP4 accepts H.264/HEVC/VP9/AV1 video with AAC/Opus/MP3/AC-3/E-AC-3
  audio. Incompatible combinations return a typed error; nothing is
  transcoded as a fallback. General transcoding is not implemented. See
  `native/VIDEO_PLAN.md` for the settled decisions and
  `cargo run --bin convert2it -- --list-video` for the route table.
- - **HEIC/HEIF input:** blocked on a licensing decision rather than on missing
  code. Pure-Rust, permissively licensed HEIC decoders now exist
  (`heifer` MIT/Apache-2.0, `zune-heic` MIT/Apache-2.0/Zlib, plus `gamut-heic`
  MIT/Apache-2.0 for the container), so the old LGPL `libde265` blocker no
  longer applies — but HEVC decoding carries patent licensing that a static
  binary redistributing a decoder walks into, so adding one is the project
  owner's call and not something to slip in. A HEIC file is therefore still
  reported as an unsupported conversion.
- **WOFF2 encoding:** output keeps the null `glyf`/`loca` transform, so it is
  valid and decodes everywhere, but it does not get the format's glyph
  pre-compression: the 35,220-byte Abel TTF becomes a 13,824-byte WOFF2 and a
  15,836-byte WOFF. Adding the encoder side of the glyph transform is the
  obvious next slice; nothing else depends on it.
- **Font flavour conversion:** `TTF` and `OTF` output relabel the container and
  keep the source outline format — no `glyf`↔`CFF ` conversion. A TTF converted
  to `.otf` still carries TrueType outlines, and an OTF converted to `.ttf`
  still carries CFF outlines. Both files are valid OpenType and render
  identically; the extension names the container, not the outline flavour. This
  is intended, and the web handler's OTF output differs here: it re-serialises
  through `opentype.js`, which rewrites the outlines. Copying the `glyf`/`CFF `
  table instead preserves every outline exactly, where converting loses
  quadratic-curve fidelity in the cubic-to-quadratic direction and TrueType
  hinting in both. Implementing the real conversion is a curve-fitting job
  (quad-to-cubic is exact, cubic-to-quadratic is not) plus CFF charstring
  emission, not a container change.
- **Metadata:** JPEG-to-JPEG preserves APP1 EXIF segments (and resets the
  orientation tag after pixels are rotated); MP3-to-MP3 preserves the ID3v2
  tag; FLAC-to-FLAC preserves Vorbis comments and pictures. EXIF in other image
  targets and tags in other audio formats are still dropped.
- **Packaging/auto-update:** the release workflow builds binaries, but installer
  signing and an updater are infrastructure work that needs certificates and a
  release channel.
- **PDF images:** PDF text extraction is available; PDF→image requires a PDF
  renderer (MuPDF/Poppler-class dependency), which is not bundled.
