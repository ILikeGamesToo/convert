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
| Raster images | PNG, JPEG, WebP, GIF, BMP, TIFF, ICO, PNM, TGA, QOI, HDR, Farbfeld, DDS, SVG, AVIF, HEIC/HEIF | PNG, JPEG, WebP, GIF, BMP, TIFF, ICO, PBM/PGM/PPM/PAM, TGA, QOI, HDR, Farbfeld, PDF, SVG trace, MP4 (from GIF) | SVG input is rasterized with a size/pixel limit, and every raster input can also be traced to a new SVG with its own limits; see [Vector tracing](#vector-tracing). DDS is input-only. AVIF and HEIC are decoded in-process by pure-Rust crates, with no external tool and no C library to install. EXIF and ICC profiles are carried between the targets that store them; see [Metadata](#metadata). A GIF is also an animation, so it fills an MP4 container: its frames are re-encoded to H.264 with the source size and frame timing kept. |
| Text and data | TXT, CSV, JSON, YAML, TOML | PDF, text encodings, CSV, JSON, YAML, TOML | CSV/JSON/YAML/TOML conversions use the same tabular rules as the web handlers. |
| Office documents | DOCX, XLSX/XLS, PPTX, ODT, ODS | Text and the text encodings, CSV, JSON | Content is extracted, never rendered. A document becomes Markdown-shaped text with its headings, lists and tables in reading order, a presentation becomes its slides' text in presentation order, and a workbook becomes one CSV per sheet or one JSON document. The container's own parts decide which format a file is, before its extension does. See [Office documents as data](#office-documents-as-data). |
| PDF | PDF | PNG, JPEG, WebP, GIF, BMP, TIFF, ICO, PNM, TGA, QOI, HDR, Farbfeld, SVG pages, text encodings | Text extraction uses `pdf-extract`. Pages rasterise in-process through `hayro` or vectorise through `hayro-svg`, one file per page; see [PDF rasterisation](#pdf-rasterisation) and [PDF page SVGs](#pdf-page-svgs) for the scale, the naming and the limits. |
| Audio | WAV, FLAC, MP3, OGG/Opus, M4A/AAC, AIFF, AC-3/E-AC-3, video containers | WAV, FLAC, AIFF, AU, Opus, MP3, MP4 (MP3 and M4A as they are) | Multichannel layouts are downmixed explicitly; video inputs are used for their audio track only. Tags are carried between MP3, FLAC and Opus; see [Audio tags](#audio-tags). MP3 and M4A also fill an MP4 container by stream copy, so the audio is carried bit for bit and never re-encoded; a cover picture travels as the MP4's attached picture. |
| Archives | ZIP, TAR, TAR.GZ/TGZ, GZIP, RAR 4/5, 7z | ZIP, CBZ, TAR, TAR.GZ, GZIP, extracted files | Extraction rejects absolute paths, `..`, drive prefixes, duplicates, oversized entries and excessive entry counts. RAR and 7z are read-only and decoded in-process by pure-Rust crates, so nothing external is installed. An archive input re-packs into the target container instead of being nested, which is what turns a CBR or CB7 into a readable CBZ. Encrypted archives are refused with a typed error, and a decode is bounded by a 120 s deadline and a 512 MiB dictionary cap. |
| Video | MP4, MOV, M4V, WebM, MKV, AVI, WMV, MPEG | MP4, WebM, animated GIF, PNG/JPEG/WebP thumbnails, audio outputs | A pair whose codecs the target container already takes is stream-copied with `ffmpeg -c copy`; anything else is re-encoded (H.264/AAC for MP4, VP9/Opus for WebM, a palette GIF at 12 fps). AVI/WMV/MPEG always encode because only ffmpeg reads their codecs. The copy decision is per stream, so an AV1+PCM MP4 copies its video and encodes only the audio. Frame/thumbnail extraction uses `ffmpeg` and then the normal image encoder; the GIF target produces the whole clip, not one frame. A GIF source becomes an MP4 and an MP3 or M4A source becomes an audio-only MP4 through the same route table, which is why those pairs need no second input kind. Requires `ffmpeg` on PATH, `CONVERT2IT_FFMPEG`/`FFMPEG`, or the CLI `--ffmpeg` override. Frame-accurate editing, quality selection and hardware encoders are not implemented. |
| Ebooks | EPUB 2, EPUB 3 | EPUB, HTML, Markdown, text encodings | The container identifies the format, not the extension: a ZIP whose first entry is an uncompressed `mimetype` holding `application/epub+zip` is an EPUB. The spine gives the reading order and the navigation document or NCX gives the chapter titles. DRM is refused with a typed error and never attempted. MOBI and AZW3 are recognised by their PalmDOC header and refused by name, because no maintained crate reads them; see [Ebooks](#ebooks). |
| Fonts | TTF, OTF, WOFF, WOFF2, TTC/OTC | TTF, OTF, WOFF, WOFF2, SVG | Every font table is copied verbatim and only the container is rewritten, so outlines, advance widths, units-per-em, the cmap and the name table survive a conversion. TTF and OTF output relabel the container and keep the source outline format, so they differ only in file name; there is no `glyf`↔`CFF ` outline conversion (see Known gaps). A `.ttc`/`.otc` collection is converted from its first face. WOFF2 input reverses the `glyf`/`loca` transform; WOFF2 output uses the null transform the format allows. SVG output is the web handler's font specimen (one `<glyph>` per mapped codepoint), not a rasterisable text document. |

## Web-only handlers not yet ported

MOBI/AZW3, Typst/Pandoc, 3D models,
SQLite, LZH, the comic packer that builds a CBZ/CBT/CBR/CB7 from loose images, game
formats, and MIDI/module music. Those handlers depend on
JavaScript/WASM libraries or
external tools that do not have a verified native equivalent in this crate.
The ebook family is no longer on that list for EPUB: the native app reads EPUB 2
and 3 and writes EPUB from text, Markdown and HTML, and refuses MOBI/AZW3 by
name. See [Ebooks](#ebooks).

Office documents used to be in that list and are not any more, but the native
handler is narrower than the web app's two paths. The web app converts *between*
office formats — docx ↔ odt ↔ html ↔ markdown and the rest of `pandoc`, which is
a WASM build of a document converter — and `pptxRenderer` draws slides. The
native handler reads the same containers and extracts their content into text
and data only: it writes no docx, xlsx, pptx or odt, and it renders nothing. A
docx → odt, docx → pdf or pptx → png request is the web app's path and stays
there.

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
are the whole of the split; see [The three SVG outputs](#the-three-svg-outputs)
for how the font specimen, the page and the trace are told apart.

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

## The three SVG outputs

Three targets write a `.svg` file and each means something different, so each
has its own name, label and CLI spelling. A saved setting stores the target
rather than the extension, so nothing that already shipped changes meaning.

| Target | GUI label | CLI | Input | What the document is |
| --- | --- | --- | --- | --- |
| `OutputFormat::Svg` | SVG font | `svg` | TTF, OTF, WOFF, WOFF2, TTC/OTC | A font specimen: one `<glyph>` per mapped codepoint, not a picture of text. |
| `OutputFormat::SvgPage` | SVG page | `svg-page` | PDF | One document per page, carrying the page's own geometry with text as glyph outlines. |
| `OutputFormat::SvgTrace` | SVG trace | `svg-trace` | Every raster the app decodes, SVG input excepted | Paths traced from the pixels: colour regions fitted with curves. |

The GUI's format list shows each label with a description under it, so the three
never appear as an unexplained `.SVG`, and only one of them is ever offered for
a given input. `--list-formats` prints the CLI spelling rather than the shared
extension, so a script reading the registry can tell them apart too. `svg` alone
still selects the font specimen, which is what it has always selected; a raster
image asked for `svg` gets a typed error naming the target rather than a guess.

## Vector tracing

Tracing turns a decoded raster into a new document: the pixels are grouped into
colour layers, each layer's boundary is fitted with curves, and the paths are
written back to front. `vtracer` 0.6.5 (MIT OR Apache-2.0, pure Rust) does the
grouping and the fitting. It is handed the pixels `load_image` already decoded,
so no new image decoder was written and every raster input — PNG, JPEG, WebP,
GIF, BMP, TIFF, ICO, PNM, TGA, QOI, HDR, Farbfeld, DDS, AVIF, HEIC — traces.
SVG input is excluded: it is already vector, and tracing it would rasterise it
first.

One fixed configuration is used for every trace, because a preset menu hands a
quality decision to someone who cannot evaluate it. It is `vtracer`'s default
shape: 6-bit colour precision, a layer difference of 16, a 4-pixel speckle
filter, spline paths with a 60-degree corner threshold, and two decimal places
on the coordinates. Each choice was measured rather than assumed, on the two
committed fixtures:

- **Colour precision 6, layer difference 16.** Raising them to 8 bits and 48
  levels moves the mean round-trip error by under 0.5/255 while multiplying the
  path count (64 to 98 paths at 320x240, 256 to 1112 at 1.9 MP) and roughly
  doubling the time. Path count is what turns a trace into a blob.
- **Spline over polygon.** Polygon output is 3.7 times smaller — 32 KiB against
  119 KiB for the same 64 paths on the photo fixture — and the error at native
  size cannot tell the two apart. A trace exists to be scaled, though: rendered
  16 times up against the logo's circle, the spline boundary sits 0.15 px (RMS,
  worst 0.27) from the true edge while the polygon chords it at 0.33 px (worst
  0.52) and always to the inside, which is 8 px of visible faceting at that
  scale.
- **A 4-pixel speckle filter.** Without it, the grain in a photograph becomes
  thousands of one-pixel paths.

Measured through the conversion path on the release build: the five-colour logo
comes back as 5 paths and 1,501 bytes with a mean error of 0.64/255 and 99% of
pixels exact; the synthetic photograph comes back as 64 paths and 120 KiB with a
mean error of 5.06/255 and a 99th percentile of 19. The tests assert those
bounds and the path counts, and they render the trace back through `resvg`
instead of checking that a file was written.

Tracing cost grows faster than the pixel count: 0.07 s at 320x240, 7.8 s at
1.9 MP, 26 s and 400 MB at 4 MP, 59 s and 850 MB at 8 MP. The limit is therefore
4096 pixels per side and 4,000,000 pixels in total, both checked before a pixel
is clustered, and anything larger returns `TraceTooLarge` instead of grinding. A
12-megapixel phone photo is refused for that reason and has to be resized first,
which is a deliberate trade rather than a missing path.

A fully transparent pixel carries no colour, and encoders commonly store black
there, so the tracer writes white into those pixels before clustering while
keeping the alpha. A transparent background large enough for the tracer to key
out stays transparent; one it does not key out becomes white rather than a black
shape.

## Office documents as data

A docx, xlsx, pptx, odt or ods is a ZIP package of XML parts, and a legacy xls
is a BIFF8 workbook inside an OLE2 compound file. Nothing here is rendered:
there is no layout engine in this app, so an office document becomes its
content. That is the whole of the feature, and it is deliberately not a
smaller version of docx → PDF — that pair is 110,000 conversions a month on the
web app and would need a layout engine this app does not have.

**Which format a file is comes from the container, not the name.** A package is
identified by its parts — `word/document.xml`, `xl/workbook.xml`,
`ppt/presentation.xml`, or an OpenDocument `mimetype` entry naming text or
spreadsheet — and the extension is consulted only when the container says
nothing. A `.docx` that is really an OpenDocument text is read as one, a `.bin`
that is a docx is read as a docx, and a ZIP with none of those parts is left to
the archive path. The format picker's first-kilobyte sniff sees the same part
names, so a renamed file is classified before any conversion starts.

| Input | Becomes |
| --- | --- |
| DOCX | Text with `#` headings, `-` list items indented by level, and pipe tables in reading order. A table's first row becomes the markdown header, which is what every docx → markdown converter does. |
| ODT | The same text shape, from `content.xml`: `text:h` headings by `text:outline-level`, `text:list-item` bullets, and `table:table` tables. |
| PPTX | `# Slide N` for each slide in presentation order — the order is the one `ppt/presentation.xml` names through its relationships, so a deck whose slides were reordered still comes out in the order a reader sees. Blank slides are skipped. |
| XLSX, XLS, ODS | One CSV per sheet named `<stem>_<sheet>.csv`, or one JSON document, or one text file with `## <sheet>` sections of tab-separated rows. |

For a document or a presentation the JSON target holds the extracted text as a
JSON string, exactly the value a `.txt` input gives, and the CSV target holds
the one-column `text` table a `.txt` input gives. For a workbook the JSON shape
is `{"sheets": [{"name": …, "rows": [[…]]}]}` with cells typed where the format
has a type — numbers as numbers, booleans as booleans, an empty cell as `null`,
a date as an ISO 8601 string — and the CSV shape is the sheet's own values as
text.

**Dates.** Excel stores a date, a time and a duration as one serial number plus a
number format, and the format is not on the cell as calamine hands it over. A
serial with a whole-day part is written as `YYYY-MM-DD`, with the time appended
as `Thh:mm:ss` once it is not midnight; a bare fraction of a day is a time of
day; a cell whose format is a duration is written as hours, minutes and seconds,
with hours unbounded so `[hh]:mm` survives. A cell formatted as a *date* whose
value is a bare fraction is indistinguishable from a time of day without reading
its number format, and is written as a time.

**Limits**, matching the archive reader: a package may hold at most 10,000
entries, a part is read through a 256 MiB cap, and an OpenDocument table is
capped at 4096 columns with repeats of empty cells and rows collapsed rather than
materialised — LibreOffice pads every sheet out to a million empty rows.

**What is not preserved, and why:**

- Numbering schemes. List items keep their level and become `-` bullets; whether
  the source was `1.` or `a.` lives in `word/numbering.xml`, which this slice
  does not read.
- Text boxes, images and drawings. `w:txbxContent`, `w:drawing` and ODF
  `draw:frame` are skipped: their text is not in the paragraph flow, and an
  image cannot become text.
- Footnotes, endnotes and comments. They are separate parts
  (`word/footnotes.xml`, `office:annotation`), not body content.
- Character formatting (bold, italic, colour) and paragraph alignment. The text
  targets carry no attributes to put it in.
- Merged table cells are kept rectangular: a `w:gridSpan` of two writes the cell
  and an empty cell beside it, and a vertically merged continuation is empty.
- Headings deeper than six levels are written as level six, because that is as
  far as Markdown goes.

**What is deliberately not built:** writing an office format, converting between
office formats, rendering a page to an image or a PDF, and mixed or multi-input
output. Each of those needs either a layout engine or an office writer, and the
one-format-in-one-format-out rule means a docx can become text but not a docx
plus a PDF.

## Ebooks

EPUB 2 and EPUB 3 are read in-process by `rbook` 0.7.13 (Apache-2.0, pure Rust);
nothing is shelled out and nothing is installed. An EPUB is a ZIP, so the
container is what identifies it: a file is an EPUB when its first entry is
`mimetype`, stored uncompressed, holding `application/epub+zip`. A `.epub` that
does not say so is a corrupt container, and a ZIP named something else that does
say so is an EPUB.

Reading walks the spine in its own order, so the output follows the book rather
than the central directory; each content document's chapter title is the
navigation document's or NCX's label for it, falling back to the document's own
first heading and then to `Chapter N`. The XHTML is parsed with `html5ever`, the
browser-grade parser, and rendered three ways: plain text (with `br` as a line
break and runs collapsed the way a rendered paragraph collapses them), CommonMark
(headings, emphasis, links, lists, quotes, code and tables), and one HTML file
with each content document in its own `<section class="epub-chapter">`. A
chapter title is written above the text only when the book has more than one
chapter and that chapter carries no heading of its own, so a one-chapter document
does not gain an invented line.

EPUB output is built from text, Markdown and HTML and **validated before it is
returned**: the writer re-reads what it wrote and checks that the mimetype entry
is first and stored, that the package carries a navigation document or an NCX,
and that every spine entry resolves to a manifest item. Markdown is parsed as
CommonMark (tables, footnotes, strikethrough and task lists included) and HTML is
read as it is; both split into chapters at their top-level `h1`, and a document
without one becomes a single chapter named after the file. Text becomes one
chapter of paragraphs, and text → EPUB → text round-trips exactly.

**What is not carried:** images, stylesheets, fonts and media. One conversion is
one input format to one output format, and a container's own formats are its
output space, so an EPUB becomes text, Markdown or a document, never a resource
directory — and an HTML input cannot bring the images that sit beside it on disk
because the conversion is handed bytes, not a folder. An EPUB's images are
inlined as data URLs in the HTML output, where they can be resolved inside the
container; they are dropped from the text and Markdown outputs, and an HTML
input's images are dropped from EPUB output.

**DRM is refused, not attempted.** An EPUB returns `EbookDrm` when
`META-INF/rights.xml` or `META-INF/sinf.xml` exists, or when
`META-INF/encryption.xml` declares a content cipher; the IDPF and Adobe font
obfuscation algorithms are *not* DRM (they hide only embedded fonts) and pass
through. A MOBI or AZW3 is refused from its PalmDOC header's encryption type.
Nothing is decrypted, ever, and no password is ever asked for.

**Limits:** an EPUB container holds at most 10,000 entries and may declare at
most 512 MiB of uncompressed content; both are read from the ZIP central
directory before anything is decompressed. An empty spine, or a spine whose
documents carry no text, returns `EbookEmpty`; a container that is not a readable
EPUB returns `EbookCorrupt`.

**MOBI and AZW3 are recognised and refused.** They are PalmDOC containers and
their detection reports them as what they are, but the conversion returns
`EbookFormatDeferred`, which names the format as recognised and unconverted. The
only crate for the family is `mobi` 0.8.0 (MIT, last published 2022-12-11),
which decompresses PalmDOC and HUFF/CDIC and nothing else: it has no INDX, FDST
or KF8 skeleton handling, so an AZW3 would come out as unassembled fragments,
and the web handler's `foliate-mobi` is a JavaScript library rather than a
crate. A half-assembled book is worse than a refusal, so the reader is deferred
to a slice that can be verified against real files.

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
  The stub tests still cover empty and invalid remux and encode files. A pair
  is stream-copied only when every stream's codec fits the target: MP4→WebM
  accepts VP8/VP9/AV1 video with Opus/Vorbis audio, and WebM→MP4 accepts
  H.264/HEVC/VP9/AV1 video with AAC/Opus/MP3/AC-3/E-AC-3 audio. Every other
  pair is re-encoded instead of rejected, which is what makes MP4→WebM,
  AVI→MP4 and MP4→GIF work on an ordinary file. Without `ffmpeg` the
  conversion still fails with `FfmpegNotFound`: there is nothing to encode
  with. A build without libx264 encodes MP4 with the built-in MPEG-4 Part 2
  encoder instead, at a quality cost. GIF→MP4 is an H.264 encode of frames
  that were already reduced to a 256-colour palette, so it is lossy in the
  same way the web app's own GIF→MP4 is; the source size and the per-frame
  delays are kept. A single-frame GIF becomes a one-frame MP4 about 0.1 s
  long, which is what its frames are, not a slideshow padded to a length
  nobody asked for. MP3→MP4 keeps MP3 audio in the MP4 container: the pair is
  legal and the copy is bit-exact, but a player that only accepts AAC in MP4
  will not play it, and the app offers no quality setting to ask for that
  re-encode. See `native/VIDEO_PLAN.md` for the
  settled decisions and `cargo run --bin convert2it -- --list-video` for the
  route table.
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
- **Office documents:** extracting content is done, but *rendering* one is not,
  and will not be without a layout engine — a docx → PDF or docx → PNG request is
  the biggest office pair on the web app and is a different project. Legacy
  binary `.doc` and `.ppt`, and the OpenDocument presentation and graphics
  formats (`.odp`, `.odg`), are not inputs at all: the same reader for each is a
  document writer's worth of work, and none of them is in the top office pairs.
- **Packaging/auto-update:** the release workflow builds binaries, but installer
  signing and an updater are infrastructure work that needs certificates and a
  release channel.
