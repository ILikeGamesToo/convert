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
| Raster images | PNG, JPEG, WebP, GIF, BMP, TIFF, ICO, PNM, TGA, QOI, HDR, Farbfeld, DDS, SVG, AVIF, HEIC/HEIF | PNG, JPEG, WebP, GIF, BMP, TIFF, ICO, PBM/PGM/PPM/PAM, TGA, QOI, HDR, Farbfeld, PDF | SVG is rasterized with a size/pixel limit. DDS is input-only. AVIF and HEIC are decoded in-process by pure-Rust crates, with no external tool and no C library to install. EXIF and ICC profiles are carried between the targets that store them; see [Metadata](#metadata). |
| Text and data | TXT, CSV, JSON, YAML, TOML | PDF, text encodings, CSV, JSON, YAML, TOML | CSV/JSON/YAML/TOML conversions use the same tabular rules as the web handlers. |
| PDF | PDF | PNG, JPEG, WebP, GIF, BMP, TIFF, ICO, PNM, TGA, QOI, HDR, Farbfeld, SVG pages, text encodings | Text extraction uses `pdf-extract`. Pages rasterise in-process through `hayro` or vectorise through `hayro-svg`, one file per page; see [PDF rasterisation](#pdf-rasterisation) and [PDF page SVGs](#pdf-page-svgs) for the scale, the naming and the limits. |
| Audio | WAV, FLAC, MP3, OGG/Opus, M4A/AAC, AIFF, AC-3/E-AC-3, video containers | WAV, FLAC, AIFF, AU, Opus, MP3 | Multichannel layouts are downmixed explicitly; video inputs are used for their audio track only. Tags are carried between MP3, FLAC and Opus; see [Audio tags](#audio-tags). |
| Archives | ZIP, TAR, TAR.GZ/TGZ, GZIP, RAR 4/5, 7z | ZIP, CBZ, TAR, TAR.GZ, GZIP, extracted files | Extraction rejects absolute paths, `..`, drive prefixes, duplicates, oversized entries and excessive entry counts. RAR and 7z are read-only and decoded in-process by pure-Rust crates, so nothing external is installed. An archive input re-packs into the target container instead of being nested, which is what turns a CBR or CB7 into a readable CBZ. Encrypted archives are refused with a typed error, and a decode is bounded by a 120 s deadline and a 512 MiB dictionary cap. |
| Video | MP4, WebM, MKV | MP4, WebM, PNG/JPEG/WebP thumbnails, audio outputs | MP4↔WebM remux uses `ffmpeg -c copy` and copies all video/audio streams. Incompatible codec/container pairs return a typed error; nothing is transcoded as a fallback. Frame/thumbnail extraction uses `ffmpeg` and then the normal image encoder. Requires `ffmpeg` on PATH, `CONVERT2IT_FFMPEG`/`FFMPEG`, or the CLI `--ffmpeg` override. General transcoding and frame-accurate editing are not implemented. |

| Fonts | TTF, OTF, WOFF, WOFF2, TTC/OTC | TTF, OTF, WOFF, WOFF2, SVG | Every font table is copied verbatim and only the container is rewritten, so outlines, advance widths, units-per-em, the cmap and the name table survive a conversion. TTF and OTF output relabel the container and keep the source outline format, so they differ only in file name; there is no `glyf`↔`CFF ` outline conversion (see Known gaps). A `.ttc`/`.otc` collection is converted from its first face. WOFF2 input reverses the `glyf`/`loca` transform; WOFF2 output uses the null transform the format allows. SVG output is the web handler's font specimen (one `<glyph>` per mapped codepoint), not a rasterisable text document. |

## Web-only handlers not yet ported

The web app (`src/handlers/index.ts`) also has dedicated handlers for vector
tracing, EPUB/AZW3/MOBI, office documents, Typst/Pandoc, 3D models,
SQLite, LZH, the comic packer that builds a CBZ/CBT/CBR/CB7 from loose images, game
formats, and MIDI/module music. Those handlers depend on
JavaScript/WASM libraries or
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

## PDF rasterisation

Pages are rasterised in-process by `hayro` 0.9 (Apache-2.0 OR MIT, pure Rust,
`#![forbid(unsafe_code)]`): no C library, no external binary, nothing to
install, and no network access. Its default features bundle permissively
licensed substitutes for the 14 standard fonts and the predefined CMaps, so a
document that embeds no font still draws real glyphs rather than boxes, and a
document that embeds a subset uses the embedded one. Rasterising the committed
Chrome fixture reproduces its source colours exactly and its text as
anti-aliased glyphs (`native/src/pdf.rs` asserts both).

The contract copies the web app's pdfjs handler, because PDF is the one input
where one file in does not mean one file out:

- **One output file per page**, named `<stem>_<page>.<ext>` with a zero-based
  page number, so a three-page `report.pdf` becomes `report_0.png`,
  `report_1.png` and `report_2.png`. The user keeps the pages they want.
- **One pixel per point** (72 dpi). pdfjs renders `getViewport({scale: 1})`,
  which is the page's own point size, and floors it to whole pixels; this module
  does the same. There is no size option to expose that scale, because the
  native app has no equivalent of the web app's width/height options.
- **Opaque white background**, matching pdfjs's `background:
  "rgb(255,255,255)"`, so a page whose content leans on transparency is backed
  by paper instead of coming out with a cut-out.

Each page is handed to the encoder as soon as it is drawn, so a long document
never exists in memory as a whole. Two limits are enforced per page instead of
per document: 8192 pixels on either edge, because the rasteriser addresses a
page with 16-bit coordinates, and 4096 x 4096 pixels of area, which is 64 MiB as
RGBA. A page over either limit returns `PdfPageTooLarge` naming its page number
and size rather than being rendered badly; A0 and a 2 m poster both fit.

What does not render: an encrypted document returns `PdfEncrypted`, because
`hayro` cannot decrypt; a document it cannot parse returns `Pdf` with the
reason, and a panic inside the renderer is caught at the page boundary and
reported the same way rather than taking the app down. The gaps `hayro`
documents for every viewer that is not Acrobat — knockout groups, CMYK blending,
3D content — come with it.

## PDF page SVGs

`hayro-svg` 0.9 (same project and licence, pure Rust) walks the same
interpreted page and writes a vector document instead of a bitmap. It ships as
its own target rather than as a second meaning for `svg`, because `svg` is
already the font specimen: `OutputFormat::Svg` stays labelled "SVG font" so an
existing font workflow resolves as it did, and `OutputFormat::SvgPage` is the
one the PDF row offers — "SVG page" under the GUI's Image category, `--format
svg-page` on the CLI, and `.svg` as the file extension for both. The two names
are the whole of the split, and `--list-formats` prints extensions, so both rows
end in `svg` while only `svg-page` selects a page from the CLI.

Everything about page handling is shared with rasterisation: one output file per
page named `<stem>_<page>.svg`, a zero-based page number, and each page written
as soon as it is converted. Nothing is sampled, so the 8192-pixel edge and
4096 x 4096 area limits do not apply. The document is sized in points — the
`viewBox` is the page's own size (`0 0 594.96 841.92` for A4) — and an opaque
white background matches the raster path. Shapes stay geometry and text becomes
glyph outlines (`<path>`/`<use>`), so the fixture's rules are `<rect>` elements,
there is no `<image>` fallback, and the text is not selectable in the SVG.
`hayro-svg`'s default `embed-fonts`/`embed-cmaps` features carry the same
permissive substitutes as the rasteriser, so a page that embeds no font still
outlines real glyphs. The committed fixture converts to a valid 388 KB SVG, and
`native/src/pdf.rs` asserts the viewBox, the vector fill and the absence of a
raster fallback.

## Metadata

Preserving is the default and there is no setting that turns it off: a
conversion that dropped a colour profile would change how the output looks on
another screen with no clue why. `--list-formats` prints the same boundary in
one line. Metadata is carried per format capability, not by rewriting it:

| Block | Read from | Written to | Notes |
| --- | --- | --- | --- |
| EXIF | JPEG (`APP1`), PNG (`eXIf`), WebP (`EXIF`), TIFF (IFD0, ExifIFD, GPS IFD), AVIF (`Exif` item), HEIC (`Exif` item) | JPEG (`APP1`), PNG (`eXIf`), WebP (`EXIF`), TIFF (IFD0 tags plus an Exif IFD) | The raw TIFF stream is copied, so tags this app does not model survive. |
| ICC profile | JPEG (`APP2`), PNG (`iCCP`), WebP (`ICCP`), TIFF (tag 34675) | JPEG (`APP2`), PNG (`iCCP`), WebP (`ICCP`), TIFF (tag 34675) | Byte for byte; concatenated profile segments are reassembled and split to the target's chunk size. |

**Orientation** is the one tag that is rewritten, because it would otherwise be
applied twice. When the app rotates the pixels — the default
`image_apply_exif_orientation`, or a HEIC/AVIF container `irot`/`imir` the
decoder applies regardless — the carried tag is set to 1. When the app does
not rotate (WebP input, whose orientation this app does not apply, or the
option turned off), the tag is carried unchanged and describes the unrotated
pixels exactly as it did in the source.

**What is dropped, and where:**

- GIF, BMP, ICO, PNM, TGA, QOI, HDR, Farbfeld and PDF have no EXIF or ICC
  container, so a conversion to them loses both.
- AVIF and HEIC ICC profiles are not carried: both decoders colour-manage to
  sRGB, so the source profile no longer describes the pixels and copying it
  would mis-tag them. A HEIC's EXIF is carried.
- A CMYK/YCCK JPEG's ICC profile is not carried, for the same reason: the
  decoder converts to RGB.
- A TIFF input is rebuilt from its descriptive IFD0 tags, its Exif IFD and its
  GPS IFD. MakerNote, the Interoperability IFD and any thumbnail IFD are not
  carried: their values are offsets into the original file and would point at
  the wrong bytes after relocation. A TIFF *output* carries the IFD0 tags, the
  orientation, the Exif IFD and the ICC profile; GPS is not written into TIFF
  because the tag writer has no multi-value RATIONAL form, so a TIFF target
  loses GPS while JPEG, PNG and WebP targets keep it.
- XMP, IPTC and MakerNotes are not handled for any target.

## Audio tags

| Block | Read from | Written to | Notes |
| --- | --- | --- | --- |
| ID3v2 | MP3 | MP3 | MP3 to MP3 copies the source's whole tag verbatim, so frames this app does not model (ReplayGain, chapters) survive; another input to MP3 builds a 2.3 tag with the fields below. |
| Vorbis comments | FLAC, Ogg Opus/Vorbis | FLAC, Opus | FLAC to FLAC copies the comment and picture blocks verbatim. |
| iTunes `ilst` | M4A/AAC | — | There is no M4A output, so these tags can only leave M4A for MP3, FLAC or Opus. |
| Cover art | ID3v2 `APIC`, FLAC `PICTURE`, iTunes `covr`, `METADATA_BLOCK_PICTURE` | MP3 `APIC`, FLAC `PICTURE`, Opus `METADATA_BLOCK_PICTURE` | One front cover; the picture's MIME type is carried, and sizes are left unknown (`0`) in the FLAC picture block. |
| Fields | title, artist, album, album artist, track number and total, genre, date, comment | same | |

WAV, AIFF and AU have no tag container this app writes, so a conversion to
them drops tags by design. Tags are not carried between different audio tracks
of one container: the file's tags are a file-level property and apply to
whichever track is decoded.

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
- **Packaging/auto-update:** the release workflow builds binaries, but installer
  signing and an updater are infrastructure work that needs certificates and a
  release channel.
