// SPDX-License-Identifier: GPL-2.0-or-later

//! Font conversion: sfnt (TTF/OTF) repackaging, the WOFF and WOFF2 containers,
//! and the SVG specimen the web handler emits.
//!
//! The web handler parses a font and re-emits it, which keeps the outline
//! flavour it read: a TrueType font stays `glyf`-based and a CFF font stays
//! `CFF `-based. This module works one level lower and copies every font table
//! verbatim while it rewrites the container, so glyph outlines, advance widths,
//! units-per-em, the cmap and the name table survive a conversion untouched.
//! `TTF` and `OTF` name the same sfnt container with different outline
//! flavours, so converting between them renames the container instead of
//! re-drawing the outlines, exactly like the web handler.
//!
//! WOFF2 output uses the null `glyf`/`loca` transform the container format
//! defines: the tables are Brotli-compressed as they are rather than run
//! through the WOFF2 glyph transform, which is a size optimisation that costs
//! nothing at decode time. WOFF2 input still goes through a full decoder that
//! reverses the transform, so real-world web fonts convert correctly.
//!
//! A font collection (`.ttc`/`.otc`) is accepted and converted from its first
//! face; the collection itself is not preserved.

use crate::error::ConvertError;
use crate::format::OutputFormat;
use std::collections::BTreeMap;
use std::io::Write;

const SFNT_TRUETYPE: u32 = 0x0001_0000;
const SFNT_TRUE: u32 = u32::from_be_bytes(*b"true");
const SFNT_CFF: u32 = u32::from_be_bytes(*b"OTTO");
const COLLECTION_TAG: u32 = u32::from_be_bytes(*b"ttcf");
const WOFF_TAG: u32 = u32::from_be_bytes(*b"wOFF");
const WOFF2_TAG: u32 = u32::from_be_bytes(*b"wOF2");

/// The checksum every well-formed sfnt file sums to.
const SFNT_CHECKSUM: u32 = 0xB1B0_AFBA;

/// Table tags WOFF2 can name with six flag bits instead of four tag bytes, in
/// the order the specification lists them.
const KNOWN_TAGS: [&[u8; 4]; 63] = [
    b"cmap", b"head", b"hhea", b"hmtx", b"maxp", b"name", b"OS/2", b"post", b"cvt ", b"fpgm",
    b"glyf", b"loca", b"prep", b"CFF ", b"VORG", b"EBDT", b"EBLC", b"gasp", b"hdmx", b"kern",
    b"LTSH", b"PCLT", b"VDMX", b"vhea", b"vmtx", b"BASE", b"GDEF", b"GPOS", b"GSUB", b"EBSC",
    b"JSTF", b"MATH", b"CBDT", b"CBLC", b"COLR", b"CPAL", b"SVG ", b"sbix", b"acnt", b"avar",
    b"bdat", b"bloc", b"bsln", b"cvar", b"fdsc", b"feat", b"fmtx", b"fvar", b"gvar", b"hsty",
    b"just", b"lcar", b"mort", b"morx", b"opbd", b"prop", b"trak", b"Zapf", b"Silf", b"Glat",
    b"Gloc", b"Feat", b"Sill",
];

/// Which font container a byte stream is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FontContainer {
    /// A standalone `glyf`-based TrueType font.
    TrueType,
    /// A standalone CFF-based OpenType font.
    Cff,
    /// A font collection; only its first face is converted.
    Collection,
    Woff,
    Woff2,
}

/// Identifies a font container by its leading tag.
pub(crate) fn sniff(input: &[u8]) -> Option<FontContainer> {
    let tag = u32::from_be_bytes(input.get(..4)?.try_into().ok()?);
    Some(match tag {
        SFNT_TRUETYPE | SFNT_TRUE => FontContainer::TrueType,
        SFNT_CFF => FontContainer::Cff,
        COLLECTION_TAG => FontContainer::Collection,
        WOFF_TAG => FontContainer::Woff,
        WOFF2_TAG => FontContainer::Woff2,
        _ => return None,
    })
}

/// Converts a font input to the requested font container or SVG specimen.
pub(crate) fn convert_font(input: &[u8], target: OutputFormat) -> Result<Vec<u8>, ConvertError> {
    let font = load(input)?;
    match target {
        OutputFormat::Ttf | OutputFormat::Otf => Ok(write_sfnt(&font)),
        OutputFormat::Woff => encode_woff(&font),
        OutputFormat::Woff2 => encode_woff2(&font),
        OutputFormat::Svg => Ok(svg_specimen(&font)?.into_bytes()),
        _ => Err(ConvertError::Font(format!(
            "{} is not a font output",
            target.label()
        ))),
    }
}

#[derive(Clone, Debug)]
struct FontTable {
    tag: [u8; 4],
    data: Vec<u8>,
}

/// Every table of one sfnt face, sorted by tag.
#[derive(Clone, Debug)]
struct Sfnt {
    version: u32,
    tables: Vec<FontTable>,
}

/// Reads a font container into sfnt tables, decompressing WOFF and WOFF2.
fn load(input: &[u8]) -> Result<Sfnt, ConvertError> {
    match sniff(input) {
        Some(FontContainer::TrueType | FontContainer::Cff | FontContainer::Collection) => {
            parse_sfnt(input, 0)
        }
        Some(FontContainer::Woff) => {
            let sfnt = wuff::decompress_woff1(input).map_err(|error| {
                ConvertError::Font(format!("could not decompress the WOFF font: {error}"))
            })?;
            parse_sfnt(&sfnt, 0)
        }
        Some(FontContainer::Woff2) => {
            let sfnt = wuff::decompress_woff2(input).map_err(|error| {
                ConvertError::Font(format!("could not decompress the WOFF2 font: {error}"))
            })?;
            parse_sfnt(&sfnt, 0)
        }
        None => Err(font_error("the input is not a font file")),
    }
}

fn parse_sfnt(input: &[u8], index: usize) -> Result<Sfnt, ConvertError> {
    let office = match sniff(input) {
        Some(FontContainer::Collection) => collection_face(input, index)?,
        Some(_) => 0,
        None => return Err(font_error("the input is not an sfnt font")),
    };
    let version =
        be_u32(input, office).ok_or_else(|| font_error("the font header is truncated"))?;
    if !matches!(version, SFNT_TRUETYPE | SFNT_TRUE | SFNT_CFF) {
        return Err(font_error("the font has an unknown sfnt version"));
    }

    let count = usize::from(
        be_u16(input, office + 4).ok_or_else(|| font_error("the font header is truncated"))?,
    );
    let mut tables = Vec::with_capacity(count);
    for entry in 0..count {
        let record = office + 12 + entry * 16;
        let tag: [u8; 4] = input
            .get(record..record + 4)
            .ok_or_else(|| font_error("the font table directory is truncated"))?
            .try_into()
            .map_err(|_| font_error("the font table directory is truncated"))?;
        let offset = usize::try_from(
            be_u32(input, record + 8)
                .ok_or_else(|| font_error("the font table directory is truncated"))?,
        )
        .map_err(|_| font_error("a font table offset does not fit in memory"))?;
        let length = usize::try_from(
            be_u32(input, record + 12)
                .ok_or_else(|| font_error("the font table directory is truncated"))?,
        )
        .map_err(|_| font_error("a font table length does not fit in memory"))?;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| font_error("a font table points outside the file"))?;
        let data = input
            .get(offset..end)
            .ok_or_else(|| font_error("a font table points outside the file"))?;
        tables.push(FontTable {
            tag,
            data: data.to_vec(),
        });
    }

    tables.sort_by_key(|table| table.tag);
    if tables.windows(2).any(|pair| pair[0].tag == pair[1].tag) {
        return Err(font_error("the font has duplicate table tags"));
    }
    Ok(Sfnt { version, tables })
}

/// Returns the offset table of one face inside a font collection.
fn collection_face(input: &[u8], index: usize) -> Result<usize, ConvertError> {
    let count =
        be_u32(input, 8).ok_or_else(|| font_error("the font collection header is truncated"))?;
    if index >= count as usize {
        return Err(font_error(format!(
            "the font collection has {count} face(s) and no face {index}"
        )));
    }
    let offset = be_u32(input, 12 + index * 4)
        .ok_or_else(|| font_error("the font collection header is truncated"))?;
    usize::try_from(offset).map_err(|_| font_error("a font face offset does not fit in memory"))
}

fn write_sfnt(font: &Sfnt) -> Vec<u8> {
    let count = font.tables.len();
    let mut out = Vec::with_capacity(sfnt_size(font));
    out.extend_from_slice(&font.version.to_be_bytes());
    out.extend_from_slice(&(count as u16).to_be_bytes());
    let (search_range, entry_selector, range_shift) = search_fields(count);
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&range_shift.to_be_bytes());

    let mut offset = 12 + count * 16;
    let mut head_offset = None;
    for table in &font.tables {
        out.extend_from_slice(&table.tag);
        out.extend_from_slice(&table_checksum(table).to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(table.data.len() as u32).to_be_bytes());
        if &table.tag == b"head" {
            head_offset = Some(offset);
        }
        offset += align(table.data.len());
    }

    for table in &font.tables {
        out.extend_from_slice(&table.data);
        let padded = align(out.len());
        out.resize(padded, 0);
    }

    if let Some(head) = head_offset {
        if head + 12 <= out.len() {
            let adjustment = SFNT_CHECKSUM.wrapping_sub(checksum(&out));
            out[head + 8..head + 12].copy_from_slice(&adjustment.to_be_bytes());
        }
    }
    out
}

fn encode_woff(font: &Sfnt) -> Result<Vec<u8>, ConvertError> {
    let count = font.tables.len();
    let mut directory = Vec::with_capacity(count * 20);
    let mut payload = Vec::new();
    let mut offset = 44 + count * 20;
    for table in &font.tables {
        let compressed = deflate(&table.data)?;
        let stored = if compressed.len() < table.data.len() {
            compressed
        } else {
            table.data.clone()
        };
        directory.extend_from_slice(&table.tag);
        directory.extend_from_slice(&(offset as u32).to_be_bytes());
        directory.extend_from_slice(&(stored.len() as u32).to_be_bytes());
        directory.extend_from_slice(&(table.data.len() as u32).to_be_bytes());
        directory.extend_from_slice(&table_checksum(table).to_be_bytes());
        offset += align(stored.len());
        payload.extend_from_slice(&stored);
        let padded = align(payload.len());
        payload.resize(padded, 0);
    }

    let mut out = Vec::with_capacity(offset);
    out.extend_from_slice(b"wOFF");
    out.extend_from_slice(&font.version.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&(count as u16).to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&(sfnt_size(font) as u32).to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&[0u8; 20]);
    out.extend_from_slice(&directory);
    out.extend_from_slice(&payload);
    let length = out.len() as u32;
    out[8..12].copy_from_slice(&length.to_be_bytes());
    Ok(out)
}

fn encode_woff2(font: &Sfnt) -> Result<Vec<u8>, ConvertError> {
    let count = font.tables.len();
    let mut directory = Vec::new();
    let mut tables = Vec::new();
    for table in &font.tables {
        let known = KNOWN_TAGS.iter().position(|tag| **tag == table.tag);
        let transformed = &table.tag == b"glyf" || &table.tag == b"loca";
        directory.push(known.unwrap_or(63) as u8 | if transformed { 0xC0 } else { 0 });
        if known.is_none() {
            directory.extend_from_slice(&table.tag);
        }
        push_base128(&mut directory, table.data.len() as u32);
        tables.extend_from_slice(&table.data);
    }

    let compressed = brotli(&tables)?;
    let mut out = Vec::with_capacity(48 + directory.len() + compressed.len());
    out.extend_from_slice(b"wOF2");
    out.extend_from_slice(&font.version.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out.extend_from_slice(&(count as u16).to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&(sfnt_size(font) as u32).to_be_bytes());
    out.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes());
    out.extend_from_slice(&[0u8; 20]);
    out.extend_from_slice(&directory);
    out.extend_from_slice(&compressed);
    let padded = align(out.len());
    out.resize(padded, 0);
    let length = out.len() as u32;
    out[8..12].copy_from_slice(&length.to_be_bytes());
    Ok(out)
}

fn brotli(data: &[u8]) -> Result<Vec<u8>, ConvertError> {
    let params = brotli::enc::BrotliEncoderParams {
        quality: 11,
        mode: brotli::enc::backward_references::BrotliEncoderMode::BROTLI_MODE_FONT,
        size_hint: data.len(),
        ..brotli::enc::BrotliEncoderParams::default()
    };
    let mut reader = data;
    let mut out = Vec::new();
    brotli::BrotliCompress(&mut reader, &mut out, &params)?;
    Ok(out)
}

fn deflate(data: &[u8]) -> Result<Vec<u8>, ConvertError> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

/// Writes a WOFF2 `UIntBase128` value in its shortest form.
fn push_base128(out: &mut Vec<u8>, value: u32) {
    let mut bytes = [0u8; 5];
    let mut index = 4;
    bytes[index] = (value & 0x7F) as u8;
    let mut rest = value >> 7;
    while rest != 0 {
        index -= 1;
        bytes[index] = (rest & 0x7F) as u8 | 0x80;
        rest >>= 7;
    }
    out.extend_from_slice(&bytes[index..]);
}

fn align(value: usize) -> usize {
    (value + 3) & !3
}

fn search_fields(count: usize) -> (u16, u16, u16) {
    if count == 0 {
        return (0, 0, 0);
    }
    let mut power = 1usize;
    let mut selector = 0u16;
    while power * 2 <= count {
        power *= 2;
        selector += 1;
    }
    let search_range = (power * 16) as u16;
    let range_shift = (count * 16) as u16 - search_range;
    (search_range, selector, range_shift)
}

fn sfnt_size(font: &Sfnt) -> usize {
    let tables: usize = font
        .tables
        .iter()
        .map(|table| align(table.data.len()))
        .sum();
    12 + font.tables.len() * 16 + tables
}

fn checksum(data: &[u8]) -> u32 {
    data.chunks(4).fold(0u32, |sum, chunk| {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum.wrapping_add(u32::from_be_bytes(word))
    })
}

/// A table's checksum, computed the way the directory stores it: the `head`
/// table is summed with its check sum adjustment cleared.
fn table_checksum(table: &FontTable) -> u32 {
    if &table.tag == b"head" && table.data.len() >= 12 {
        let mut data = table.data.clone();
        data[8..12].fill(0);
        checksum(&data)
    } else {
        checksum(&table.data)
    }
}

fn be_u16(input: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        input.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn be_u32(input: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        input.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn font_error(message: impl Into<String>) -> ConvertError {
    ConvertError::Font(message.into())
}

const NAME_ID_FAMILY: u16 = 1;
const NAME_ID_FULL: u16 = 4;

/// Builds the SVG specimen the web handler emits: an SVG font with one
/// `<glyph>` per mapped codepoint, plus the specimen lines that need a text
/// stack to render. Codepoints XML cannot represent are left out so the
/// document stays well formed.
fn svg_specimen(font: &Sfnt) -> Result<String, ConvertError> {
    let sfnt = write_sfnt(font);
    let face = ttf_parser::Face::parse(&sfnt, 0)
        .map_err(|error| font_error(format!("could not read the font: {error}")))?;
    let units_per_em = face.units_per_em();
    let family = face_name(&face, NAME_ID_FAMILY).unwrap_or_else(|| "ConvertedFont".to_owned());
    let full_name = face_name(&face, NAME_ID_FULL).unwrap_or_else(|| family.clone());

    let mut glyphs = String::new();
    let mut alphabet_width = 0u32;
    for (codepoint, glyph) in mapped_glyphs(&face) {
        let mut path = SvgPath::default();
        let _ = face.outline_glyph(glyph, &mut path);
        let advance = face.glyph_hor_advance(glyph).unwrap_or(units_per_em);
        let character = char::from_u32(codepoint).unwrap_or_default();
        glyphs.push_str(&format!(
            "      <glyph unicode=\"{}\" glyph-name=\"{}\" horiz-adv-x=\"{advance}\" d=\"{}\"/>\n",
            escape(&character.to_string()),
            escape(face.glyph_name(glyph).unwrap_or_default()),
            escape(path.d.trim_end())
        ));
        if matches!(codepoint, 32 | 65..=90 | 97..=122) {
            alphabet_width += u32::from(advance);
        }
    }

    let width = f64::from(alphabet_width) / f64::from(units_per_em) * 2.0 + 4.0;
    let mut svg = String::new();
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width:.4}em\" height=\"10em\">\n"
    ));
    svg.push_str("  <defs>\n");
    svg.push_str(&format!(
        "    <font id=\"{}\" horiz-adv-x=\"{units_per_em}\">\n",
        escape(&full_name)
    ));
    svg.push_str(&format!(
        "      <font-face\n          font-family=\"{}\"\n          style-name=\"Regular\"\n          \
         units-per-em=\"{units_per_em}\"\n          ascent=\"{}\"\n          descent=\"{}\"\n      />\n",
        escape(&family),
        face.ascender(),
        face.descender(),
    ));
    svg.push_str(&glyphs);
    svg.push_str("    </font>\n  </defs>\n\n");
    svg.push_str(&format!(
        "  <style>\n    text {{\n      font-family: \"{}\";\n      font-size: 2em;\n      \
         fill: black;\n    }}\n  </style>\n\n",
        escape(&family)
    ));
    svg.push_str(SPECIMEN_TEXT);
    svg.push_str("</svg>\n");
    Ok(svg)
}

/// The three sample lines the web handler draws with the generated font.
const SPECIMEN_TEXT: &str = r#"  <text x="1em" y="1em">
    The quick brown fox jumps over the lazy dog.
  </text>
  <text x="1em" y="3em">
    ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz
  </text>
  <text x="1em" y="4.5em">
    0123456789 !@#$%^&amp;*() -=_+[]{};&apos;:&quot;,./&lt;&gt;?\|\`~
  </text>
"#;

/// Every codepoint the font maps, in ascending order, with the glyph it
/// selects. A codepoint mapped by more than one cmap subtable keeps the first
/// mapping found.
fn mapped_glyphs(face: &ttf_parser::Face<'_>) -> BTreeMap<u32, ttf_parser::GlyphId> {
    let mut codepoints = BTreeMap::new();
    if let Some(cmap) = face.tables().cmap {
        for subtable in cmap.subtables {
            subtable.codepoints(|codepoint| {
                if !is_xml_character(codepoint) {
                    return;
                }
                if let Some(character) = char::from_u32(codepoint) {
                    if let Some(glyph) = face.glyph_index(character) {
                        codepoints.entry(codepoint).or_insert(glyph);
                    }
                }
            });
        }
    }
    codepoints
}

fn face_name(face: &ttf_parser::Face<'_>, name_id: u16) -> Option<String> {
    face.names()
        .into_iter()
        .filter(|name| name.name_id == name_id)
        .find_map(|name| name.to_string())
        .filter(|name| !name.is_empty())
}

/// Collects one glyph outline as SVG path data with the Y axis flipped, since
/// SVG and sfnt use different coordinate systems.
#[derive(Default)]
struct SvgPath {
    d: String,
}

impl SvgPath {
    fn point(&mut self, x: f32, y: f32) {
        self.d.push_str(&format_coordinate(x));
        self.d.push(' ');
        self.d.push_str(&format_coordinate(-y));
        self.d.push(' ');
    }
}

impl ttf_parser::OutlineBuilder for SvgPath {
    fn move_to(&mut self, x: f32, y: f32) {
        self.d.push('M');
        self.point(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.d.push('L');
        self.point(x, y);
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.d.push('Q');
        self.point(x1, y1);
        self.point(x, y);
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.d.push('C');
        self.point(x1, y1);
        self.point(x2, y2);
        self.point(x, y);
    }

    fn close(&mut self) {
        self.d.push('Z');
    }
}

fn format_coordinate(value: f32) -> String {
    let rounded = (value * 100_000.0).round() / 100_000.0;
    if rounded == 0.0 {
        return "0".to_owned();
    }
    format!("{rounded}")
}

fn is_xml_character(codepoint: u32) -> bool {
    matches!(
        codepoint,
        0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x1_0000..=0x10_FFFF
    )
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            '\n' => escaped.push_str("&#x0A;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::convert_bytes;
    use crate::input_format::InputFormat;
    use crate::test_support::{otf_font_fixture, ttf_font_fixture, woff2_font_fixture};
    use std::path::PathBuf;
    use std::sync::Arc;

    const CONTAINER_OUTPUTS: [OutputFormat; 4] = [
        OutputFormat::Ttf,
        OutputFormat::Otf,
        OutputFormat::Woff,
        OutputFormat::Woff2,
    ];

    fn tag_name(tag: &[u8; 4]) -> String {
        String::from_utf8_lossy(tag).into_owned()
    }

    /// Renders text with the font loaded from `font` bytes, so a conversion
    /// that loads but draws nothing fails the test.
    fn render_text(font: &[u8], family: &str) -> image::RgbaImage {
        let mut database = resvg::usvg::fontdb::Database::new();
        database.load_font_data(font.to_vec());
        let options = resvg::usvg::Options {
            fontdb: Arc::new(database),
            ..resvg::usvg::Options::default()
        };
        let svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"420\" height=\"64\">\
             <rect width=\"420\" height=\"64\" fill=\"white\"/>\
             <text x=\"4\" y=\"48\" font-family=\"{family}\" font-size=\"40\" fill=\"black\">\
             Hamburgefonstiv</text></svg>"
        );
        let tree = resvg::usvg::Tree::from_str(&svg, &options).unwrap();
        let size = tree.size().to_int_size();
        let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height()).unwrap();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::default(),
            &mut pixmap.as_mut(),
        );
        image::RgbaImage::from_raw(size.width(), size.height(), pixmap.take()).unwrap()
    }

    fn ink_pixels(image: &image::RgbaImage) -> usize {
        image
            .pixels()
            .filter(|pixel| pixel.0[..3].iter().all(|channel| *channel < 128))
            .count()
    }

    #[test]
    fn identifies_every_font_container() {
        assert_eq!(sniff(&ttf_font_fixture()), Some(FontContainer::TrueType));
        assert_eq!(sniff(&otf_font_fixture()), Some(FontContainer::Cff));
        assert_eq!(sniff(&woff2_font_fixture()), Some(FontContainer::Woff2));
        let woff = convert_font(&ttf_font_fixture(), OutputFormat::Woff).unwrap();
        assert_eq!(sniff(&woff), Some(FontContainer::Woff));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(sniff(b"no"), None);
    }

    #[test]
    fn every_table_survives_every_container() {
        for source in [ttf_font_fixture(), otf_font_fixture(), woff2_font_fixture()] {
            let before = load(&source).unwrap();
            for target in CONTAINER_OUTPUTS {
                let converted = convert_font(&source, target).unwrap();
                let after = load(&converted).unwrap();
                assert_eq!(before.version, after.version, "{target:?}");
                assert_eq!(before.tables.len(), after.tables.len(), "{target:?}");
                for (original, converted) in before.tables.iter().zip(&after.tables) {
                    assert_eq!(original.tag, converted.tag, "{target:?}");
                    if &original.tag == b"head" {
                        assert_eq!(original.data.len(), converted.data.len());
                        assert_eq!(&original.data[..8], &converted.data[..8]);
                        assert_eq!(&original.data[12..], &converted.data[12..]);
                    } else {
                        assert_eq!(
                            original.data,
                            converted.data,
                            "{} changed as {target:?}",
                            tag_name(&original.tag)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn converted_fonts_still_render_glyphs() {
        for (source, family) in [
            (ttf_font_fixture(), "Abel"),
            (otf_font_fixture(), "Source Code Pro"),
        ] {
            let control = ink_pixels(&render_text(&source, family));
            assert!(
                control > 200,
                "the source {family} font does not render, so the test proves nothing"
            );
            for target in CONTAINER_OUTPUTS {
                let converted = convert_font(&source, target).unwrap();
                let sfnt = write_sfnt(&load(&converted).unwrap());
                assert_eq!(
                    ink_pixels(&render_text(&sfnt, family)),
                    control,
                    "{family} as {target:?} renders differently from its source"
                );
            }
        }
    }

    #[test]
    fn web_fonts_decode_and_render() {
        let converted = convert_font(&woff2_font_fixture(), OutputFormat::Ttf).unwrap();
        assert!(ink_pixels(&render_text(&converted, "Abel")) > 200);
    }

    fn glyph_path_data(svg: &str, character: &str) -> String {
        let needle = format!("unicode=\"{character}\"");
        let start = svg
            .find(&needle)
            .unwrap_or_else(|| panic!("the specimen has no glyph for {character}"));
        let rest = &svg[start..];
        let open = rest.find("d=\"").expect("the glyph has no path data") + 3;
        let close = rest[open..]
            .find('"')
            .expect("the glyph path is unterminated")
            + open;
        rest[open..close].to_owned()
    }

    #[test]
    fn svg_specimen_lists_every_mapped_codepoint() {
        let source = ttf_font_fixture();
        let svg = String::from_utf8(convert_font(&source, OutputFormat::Svg).unwrap()).unwrap();
        let face = ttf_parser::Face::parse(&source, 0).unwrap();
        assert_eq!(svg.matches("<glyph ").count(), mapped_glyphs(&face).len());
        assert!(svg.contains("font-family=\"Abel\""));
        assert!(svg.contains(&format!("units-per-em=\"{}\"", face.units_per_em())));

        let path = glyph_path_data(&svg, "A");
        assert!(!path.is_empty(), "the 'A' glyph has no path data");
        let specimen = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"64\" height=\"64\">\
             <rect width=\"64\" height=\"64\" fill=\"white\"/>\
             <path transform=\"translate(6 58) scale(0.05)\" d=\"{path}\" fill=\"black\"/></svg>"
        );
        let image = crate::images::rasterize_svg(specimen.as_bytes())
            .unwrap()
            .to_rgba8();
        assert!(
            ink_pixels(&image) > 20,
            "the 'A' path data renders no ink, so the outline is not usable"
        );
    }

    /// Wraps a single-face sfnt in a TrueType collection header, shifting the
    /// table offsets to their absolute position in the collection.
    fn build_collection(sfnt: &[u8]) -> Vec<u8> {
        let count = usize::from(be_u16(sfnt, 4).unwrap());
        let mut collection = Vec::new();
        collection.extend_from_slice(b"ttcf");
        collection.extend_from_slice(&0x0001_0000u32.to_be_bytes());
        collection.extend_from_slice(&1u32.to_be_bytes());
        collection.extend_from_slice(&16u32.to_be_bytes());
        let body = collection.len();
        collection.extend_from_slice(sfnt);
        for index in 0..count {
            let record = body + 12 + index * 16;
            let offset = be_u32(&collection, record + 8).unwrap() + body as u32;
            collection[record + 8..record + 12].copy_from_slice(&offset.to_be_bytes());
        }
        collection
    }

    #[test]
    fn font_collections_convert_their_first_face() {
        let source = ttf_font_fixture();
        let collection = build_collection(&source);
        assert_eq!(sniff(&collection), Some(FontContainer::Collection));

        let converted = convert_font(&collection, OutputFormat::Woff2).unwrap();
        assert_eq!(sniff(&converted), Some(FontContainer::Woff2));
        let standalone = load(&source).unwrap();
        let taken = load(&converted).unwrap();
        assert_eq!(standalone.tables.len(), taken.tables.len());
        for (original, converted) in standalone.tables.iter().zip(&taken.tables) {
            assert_eq!(original.tag, converted.tag);
            if &original.tag != b"head" {
                assert_eq!(original.data, converted.data, "{}", tag_name(&original.tag));
            }
        }
    }

    #[test]
    fn rejects_inputs_that_are_not_fonts() {
        assert!(matches!(
            convert_font(b"\x89PNG\r\n\x1a\n", OutputFormat::Ttf),
            Err(ConvertError::Font(_))
        ));
        assert!(matches!(
            convert_font(&ttf_font_fixture(), OutputFormat::Png),
            Err(ConvertError::Font(_))
        ));
        let mut truncated = ttf_font_fixture();
        truncated.truncate(truncated.len() / 2);
        assert!(matches!(
            convert_font(&truncated, OutputFormat::Woff),
            Err(ConvertError::Font(_))
        ));
    }

    #[test]
    fn converts_through_the_public_entry_point() {
        let converted =
            convert_bytes("Abel-Regular.ttf", &ttf_font_fixture(), OutputFormat::Woff2).unwrap();
        assert_eq!(converted[0].name, PathBuf::from("Abel-Regular.woff2"));
        assert_eq!(sniff(&converted[0].bytes), Some(FontContainer::Woff2));
    }

    #[test]
    fn registry_offers_every_font_output() {
        let registry = crate::format::native_format_registry();
        for input in [
            InputFormat::Ttf,
            InputFormat::Otf,
            InputFormat::Woff,
            InputFormat::Woff2,
        ] {
            let handler = registry
                .iter()
                .find(|handler| handler.input == input)
                .unwrap_or_else(|| panic!("{input:?} is not in the registry"));
            for target in [
                OutputFormat::Ttf,
                OutputFormat::Otf,
                OutputFormat::Woff,
                OutputFormat::Woff2,
                OutputFormat::Svg,
            ] {
                assert!(
                    handler.outputs.contains(&target),
                    "{input:?} does not offer {target:?}"
                );
            }
        }
    }
}
