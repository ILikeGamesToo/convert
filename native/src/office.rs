// SPDX-License-Identifier: GPL-2.0-or-later

//! Office documents as data.
//!
//! A DOCX, XLSX, PPTX, ODT or ODS is a ZIP package of XML parts, and an XLS is
//! a legacy OLE2 workbook. This app has no layout engine, so an office document
//! is never rendered: it is read for its content. A word processor document
//! becomes Markdown-shaped text with its headings, lists and tables in reading
//! order, a presentation becomes its slides' text, and a workbook becomes rows
//! for the CSV, JSON and text targets.
//!
//! Every package is bounded the way the archive reader bounds an archive: the
//! entry count and each decompressed part have a cap, because an office
//! document is a zip bomb waiting to happen.

use crate::archive::{MAX_ARCHIVE_ENTRIES, MAX_ARCHIVE_ENTRY_BYTES};
use crate::convert::ConvertedFile;
use crate::data::text_to_csv;
use crate::error::ConvertError;
use crate::format::OutputFormat;
use crate::shared::{file_extension, output_name, unsupported};
use crate::text::encode_text;
use calamine::{Data, DataType, ExcelDateTime, Ods, Reader, Xls, Xlsx};
use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::name::QName;
use quick_xml::{Reader as XmlReader, XmlVersion};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

/// The OLE2 compound-file signature. A password-protected DOCX/XLSX/PPTX and a
/// legacy XLS are both this container; neither is a ZIP package.
pub(crate) const OLE2_SIGNATURE: [u8; 8] = [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];

/// The stream an encrypted OOXML package stores its payload in, as the UTF-16LE
/// bytes a compound-file directory holds.
const ENCRYPTED_PACKAGE: &[u8] = b"E\0n\0c\0r\0y\0p\0t\0e\0d\0P\0a\0c\0k\0a\0g\0e\0";

/// How many bytes of a compound file are scanned for that stream name.
const ENCRYPTED_SCAN_BYTES: usize = 1024 * 1024;

/// The widest table this module writes. An OpenDocument cell may claim up to
/// 16384 columns with a single `table:number-columns-repeated` attribute, and a
/// document table that wide is a padding artefact rather than content.
const MAX_TABLE_COLUMNS: usize = 4096;

/// How many times a `table:number-rows-repeated` or `table:number-columns-repeated`
/// attribute is honoured. LibreOffice writes a million-row repeat for the empty
/// remainder of a table, which must collapse rather than materialise.
const MAX_TABLE_REPEAT: usize = 1024;

/// How deep a container may nest before its content is skipped. Real documents
/// nest a handful of levels; a hostile one can nest `<text:list>` forever.
const MAX_XML_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OfficeKind {
    Docx,
    Xlsx,
    Xls,
    Pptx,
    Odt,
    Ods,
}

impl OfficeKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Docx => "DOCX",
            Self::Xlsx => "XLSX",
            Self::Xls => "XLS",
            Self::Pptx => "PPTX",
            Self::Odt => "ODT",
            Self::Ods => "ODS",
        }
    }
}

/// The office kind a bare extension names, without looking at the container.
pub(crate) fn kind_for_extension(extension: &str) -> Option<OfficeKind> {
    Some(match extension {
        "docx" => OfficeKind::Docx,
        "xlsx" => OfficeKind::Xlsx,
        "xls" => OfficeKind::Xls,
        "pptx" => OfficeKind::Pptx,
        "odt" => OfficeKind::Odt,
        "ods" => OfficeKind::Ods,
        _ => return None,
    })
}

/// Which office document `bytes` holds, judged by the container before the
/// extension. Returns `Ok(None)` when the file is not an office document at all,
/// which leaves the plain archive and text paths alone.
pub(crate) fn detect(input_name: &str, input: &[u8]) -> Result<Option<OfficeKind>, ConvertError> {
    let extension = file_extension(input_name);
    if input.starts_with(&OLE2_SIGNATURE) {
        return detect_compound(&extension, input);
    }
    if let Ok(mut archive) = open_package(input) {
        if let Some(kind) = ooxml_kind(&archive) {
            return Ok(Some(kind));
        }
        if let Some(kind) = odf_kind(&mut archive, &extension)? {
            return Ok(Some(kind));
        }
        if let Some(kind) = kind_for_extension(&extension) {
            return Err(ConvertError::OfficeCorrupt {
                format: kind.label(),
                message: "the package has none of the parts an office document needs".to_owned(),
            });
        }
        return Ok(None);
    }
    match kind_for_extension(&extension) {
        Some(kind) => Err(ConvertError::OfficeCorrupt {
            format: kind.label(),
            message: "the file is not a ZIP package".to_owned(),
        }),
        None => Ok(None),
    }
}

fn detect_compound(extension: &str, input: &[u8]) -> Result<Option<OfficeKind>, ConvertError> {
    // A legacy workbook is the one OLE2 container this app reads, and calamine
    // reports a password-protected one as `XlsError::Password`.
    if extension == "xls" {
        return Ok(Some(OfficeKind::Xls));
    }
    let Some(kind) = kind_for_extension(extension) else {
        return Ok(None);
    };
    // Word, Excel and PowerPoint save a password-protected package as an OLE2
    // container wrapping the zipped parts in an `EncryptedPackage` stream.
    if looks_encrypted(input) {
        return Err(ConvertError::OfficeEncrypted);
    }
    Err(ConvertError::OfficeCorrupt {
        format: kind.label(),
        message: "the file is an OLE2 container, not the ZIP package this format needs".to_owned(),
    })
}

fn looks_encrypted(input: &[u8]) -> bool {
    let scan = &input[..input.len().min(ENCRYPTED_SCAN_BYTES)];
    scan.windows(ENCRYPTED_PACKAGE.len())
        .any(|window| window == ENCRYPTED_PACKAGE)
}

fn ooxml_kind(archive: &zip::ZipArchive<Cursor<&[u8]>>) -> Option<OfficeKind> {
    // The part names are the format's own claim about what it is, and the
    // extension is only consulted when the package says nothing.
    if archive.index_for_name("word/document.xml").is_some() {
        return Some(OfficeKind::Docx);
    }
    if archive.index_for_name("xl/workbook.xml").is_some() {
        return Some(OfficeKind::Xlsx);
    }
    if archive.index_for_name("ppt/presentation.xml").is_some() {
        return Some(OfficeKind::Pptx);
    }
    None
}

fn odf_kind(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    extension: &str,
) -> Result<Option<OfficeKind>, ConvertError> {
    let mimetype = match read_part_limited(archive, "mimetype", "ODF", 4096)? {
        Some(bytes) => String::from_utf8_lossy(&bytes).trim().to_owned(),
        None => String::new(),
    };
    let kind = if mimetype.contains("vnd.oasis.opendocument.text") {
        Some(OfficeKind::Odt)
    } else if mimetype.contains("vnd.oasis.opendocument.spreadsheet") {
        Some(OfficeKind::Ods)
    } else if archive.index_for_name("content.xml").is_some() {
        // A package may carry no `mimetype` entry at all; the extension is the
        // only remaining clue, and a text document and a workbook differ by it.
        match extension {
            "odt" => Some(OfficeKind::Odt),
            "ods" => Some(OfficeKind::Ods),
            _ => None,
        }
    } else {
        None
    };
    if kind.is_some() && odf_is_encrypted(archive)? {
        return Err(ConvertError::OfficeEncrypted);
    }
    Ok(kind)
}

fn odf_is_encrypted(archive: &mut zip::ZipArchive<Cursor<&[u8]>>) -> Result<bool, ConvertError> {
    // OpenDocument encryption is recorded in the manifest rather than by a
    // container change, so the package still looks like a package.
    Ok(
        match read_part_limited(archive, "META-INF/manifest.xml", "ODF", 4 * 1024 * 1024)? {
            Some(manifest) => manifest
                .windows(b"encryption-data".len())
                .any(|window| window == b"encryption-data"),
            None => false,
        },
    )
}

/// The office kind a first-kilobyte prefix names. Part names and the ODF
/// `mimetype` value live in the local file headers at the front of a package,
/// so the format picker can tell an office document from an archive without
/// reading the whole file.
pub(crate) fn sniff_prefix(bytes: &[u8]) -> Option<OfficeKind> {
    if bytes.starts_with(&OLE2_SIGNATURE) {
        return None;
    }
    if !bytes.starts_with(b"PK\x03\x04") && !bytes.starts_with(b"PK\x05\x06") {
        return None;
    }
    for (marker, kind) in [
        (
            b"application/vnd.oasis.opendocument.text".as_slice(),
            OfficeKind::Odt,
        ),
        (
            b"application/vnd.oasis.opendocument.spreadsheet".as_slice(),
            OfficeKind::Ods,
        ),
        (b"word/document.xml".as_slice(), OfficeKind::Docx),
        (b"xl/workbook.xml".as_slice(), OfficeKind::Xlsx),
        (b"ppt/presentation.xml".as_slice(), OfficeKind::Pptx),
    ] {
        if bytes.windows(marker.len()).any(|window| window == marker) {
            return Some(kind);
        }
    }
    None
}

pub(crate) fn convert(
    input_name: &str,
    input: &[u8],
    kind: OfficeKind,
    target: OutputFormat,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    if !is_text_or_data_target(target) {
        return Err(unsupported(&file_extension(input_name), target));
    }
    match kind {
        OfficeKind::Docx => document_output(input_name, docx_text(input)?, target),
        OfficeKind::Odt => document_output(input_name, odt_text(input)?, target),
        OfficeKind::Pptx => document_output(input_name, pptx_text(input)?, target),
        OfficeKind::Xlsx | OfficeKind::Xls | OfficeKind::Ods => {
            workbook_output(input_name, &read_workbook(kind, input)?, target)
        }
    }
}

fn is_text_or_data_target(target: OutputFormat) -> bool {
    matches!(
        target,
        OutputFormat::Text
            | OutputFormat::TextUtf8NoBom
            | OutputFormat::TextUtf8Bom
            | OutputFormat::TextUtf16Le
            | OutputFormat::TextUtf16Be
            | OutputFormat::TextUtf32Le
            | OutputFormat::TextUtf32Be
            | OutputFormat::Csv
            | OutputFormat::Json
    )
}

// ---- Packages and parts ---------------------------------------------------

fn open_package(input: &[u8]) -> Result<zip::ZipArchive<Cursor<&[u8]>>, zip::result::ZipError> {
    zip::ZipArchive::new(Cursor::new(input))
}

fn package_of<'a>(
    input: &'a [u8],
    format: &'static str,
) -> Result<zip::ZipArchive<Cursor<&'a [u8]>>, ConvertError> {
    let archive = open_package(input).map_err(|error| ConvertError::OfficeCorrupt {
        format,
        message: error.to_string(),
    })?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(ConvertError::ArchiveTooManyEntries {
            count: archive.len(),
            max: MAX_ARCHIVE_ENTRIES,
        });
    }
    Ok(archive)
}

fn read_part(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    format: &'static str,
) -> Result<Option<Vec<u8>>, ConvertError> {
    read_part_limited(archive, name, format, MAX_ARCHIVE_ENTRY_BYTES)
}

fn read_part_limited(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    format: &'static str,
    limit: u64,
) -> Result<Option<Vec<u8>>, ConvertError> {
    let mut entry = match archive.by_name(name) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(error) => {
            return Err(ConvertError::OfficeCorrupt {
                format,
                message: format!("cannot read {name}: {error}"),
            })
        }
    };
    // A zip-level encrypted part (which an OpenDocument manifest normally
    // declares first) is a document this app cannot open, not a corrupt one.
    if entry.encrypted() {
        return Err(ConvertError::OfficeEncrypted);
    }
    let mut bytes = Vec::new();
    (&mut entry)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| ConvertError::OfficeCorrupt {
            format,
            message: format!("cannot read {name}: {error}"),
        })?;
    if bytes.len() as u64 > limit {
        return Err(ConvertError::OfficePartTooLarge {
            format,
            part: name.to_owned(),
            max: limit,
        });
    }
    Ok(Some(bytes))
}

fn read_required_part(
    archive: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    format: &'static str,
) -> Result<Vec<u8>, ConvertError> {
    read_part(archive, name, format)?.ok_or_else(|| ConvertError::OfficeCorrupt {
        format,
        message: format!("the package has no {name} part"),
    })
}

fn decode_part(bytes: &[u8], format: &'static str) -> Result<String, ConvertError> {
    // Every office XML part is UTF-8; a part that is not is a corrupt package
    // rather than something to decode lossily.
    let text = std::str::from_utf8(bytes).map_err(|error| ConvertError::OfficeCorrupt {
        format,
        message: format!("an XML part is not valid UTF-8: {error}"),
    })?;
    Ok(text.strip_prefix('\u{feff}').unwrap_or(text).to_owned())
}

fn xml_error(format: &'static str, error: quick_xml::Error) -> ConvertError {
    ConvertError::OfficeCorrupt {
        format,
        message: error.to_string(),
    }
}

fn local_name<'a>(name: &QName<'a>) -> &'a str {
    name.local_name().into_inner()
}

/// Whether `element` is the one named `local` and is a namespaced (`prefixed`)
/// attribute, which is how the relationship id on a slide reference differs
/// from the plain `id` beside it.
fn attribute(element: &BytesStart<'_>, local: &str) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|attribute| attribute.key.local_name().into_inner() == local)
        .and_then(|attribute| attribute.normalized_value(XmlVersion::Implicit1_0).ok())
        .map(|value| value.into_owned())
}

/// Consumes the rest of an element whose start tag was just read. Depth is
/// counted rather than names matched, so a nested element with the same name
/// cannot end the wrong subtree early.
fn skip_element(reader: &mut XmlReader<&[u8]>, format: &'static str) -> Result<(), ConvertError> {
    let mut depth = 1_usize;
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error(format, error))?
        {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
            }
            Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

fn push_reference(text: &mut String, reference: BytesRef<'_>) {
    match reference.resolve_char_ref() {
        Ok(Some(character)) => text.push(character),
        Ok(None) => {
            let name = reference.into_inner();
            let replacement = match name.as_ref() {
                "amp" => "&",
                "lt" => "<",
                "gt" => ">",
                "quot" => "\"",
                "apos" => "'",
                _ => return,
            };
            text.push_str(replacement);
        }
        Err(_) => {}
    }
}

// ---- Documents and presentations ------------------------------------------

#[derive(Debug, PartialEq)]
enum Block {
    Heading { level: u8, text: String },
    Paragraph(String),
    ListItem { level: u8, text: String },
    Table(Vec<Vec<String>>),
}

fn render_blocks(blocks: &[Block]) -> String {
    let mut text = String::new();
    let mut previous_list_item = false;
    for block in blocks {
        let rendered = match block {
            Block::Heading { level, text } => Some(format!(
                "{} {text}",
                "#".repeat(usize::from((*level).clamp(1, 6)))
            )),
            Block::Paragraph(paragraph) => Some(paragraph.clone()),
            Block::ListItem { level, text } => Some(format!(
                "{}- {text}",
                "  ".repeat(usize::from(*level).saturating_sub(1))
            )),
            Block::Table(rows) => render_table(rows),
        };
        let Some(rendered) = rendered else { continue };
        let list_item = matches!(block, Block::ListItem { .. });
        if !text.is_empty() {
            text.push('\n');
            // Consecutive list items are one tight list: a blank line between
            // them would end the list in Markdown.
            if !list_item || !previous_list_item {
                text.push('\n');
            }
        }
        text.push_str(&rendered);
        previous_list_item = list_item;
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

fn render_table(rows: &[Vec<String>]) -> Option<String> {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return None;
    }
    let mut lines = Vec::with_capacity(rows.len() + 1);
    for (index, row) in rows.iter().enumerate() {
        lines.push(table_row(row, width));
        if index == 0 {
            lines.push(format!("|{}", " --- |".repeat(width)));
        }
    }
    Some(lines.join("\n"))
}

fn table_row(row: &[String], width: usize) -> String {
    let mut line = String::from("|");
    for index in 0..width {
        line.push(' ');
        if let Some(cell) = row.get(index) {
            line.push_str(&cell.replace('|', "\\|").replace('\n', " "));
        }
        line.push_str(" |");
    }
    line
}

fn document_output(
    input_name: &str,
    text: String,
    target: OutputFormat,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    let bytes = match target {
        OutputFormat::Csv => text_to_csv(&text)?,
        // A document is text, so its JSON is that text as a JSON string, the
        // same value a `.txt` input gives.
        OutputFormat::Json => serde_json::to_vec_pretty(&Value::String(text))?,
        _ => encode_text(&text, target)?,
    };
    Ok(vec![ConvertedFile {
        name: output_name(input_name, target.extension()),
        bytes,
    }])
}

/// The text of every paragraph and run inside an element that carries text
/// directly, such as `<w:t>` or `<a:t>`.
fn element_text(
    reader: &mut XmlReader<&[u8]>,
    end: &str,
    format: &'static str,
) -> Result<String, ConvertError> {
    let mut text = String::new();
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error(format, error))?
        {
            Event::Eof => break,
            Event::End(element) if local_name(&element.name()) == end => break,
            Event::Text(element) => text.push_str(&element.xml10_content()),
            Event::CData(element) => text.push_str(&element.into_inner()),
            Event::GeneralRef(element) => push_reference(&mut text, element),
            _ => {}
        }
    }
    Ok(text)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ParagraphKind {
    Word,
    Slide,
}

struct Paragraph {
    text: String,
    heading: Option<u8>,
    list_level: Option<u8>,
}

fn docx_text(input: &[u8]) -> Result<String, ConvertError> {
    let mut archive = package_of(input, "DOCX")?;
    let part = read_required_part(&mut archive, "word/document.xml", "DOCX")?;
    let document = decode_part(&part, "DOCX")?;
    let mut reader = XmlReader::from_str(&document);
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("DOCX", error))?
        {
            Event::Start(element) if local_name(&element.name()) == "body" => break,
            Event::Eof => {
                return Err(ConvertError::OfficeCorrupt {
                    format: "DOCX",
                    message: "the document part has no body".to_owned(),
                })
            }
            _ => {}
        }
    }
    let mut blocks = Vec::new();
    docx_blocks(&mut reader, "body", 0, &mut blocks)?;
    let text = render_blocks(&blocks);
    if text.is_empty() {
        return Err(ConvertError::OfficeEmptyBody { format: "DOCX" });
    }
    Ok(text)
}

fn docx_blocks(
    reader: &mut XmlReader<&[u8]>,
    end: &str,
    depth: usize,
    blocks: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("DOCX", error))?
        {
            Event::Eof => return Ok(()),
            Event::End(element) if local_name(&element.name()) == end => return Ok(()),
            Event::Start(element) => {
                let name = local_name(&element.name()).to_owned();
                match name.as_str() {
                    "p" => {
                        let paragraph = paragraph(reader, "p", ParagraphKind::Word, "DOCX")?;
                        if let Some(block) = paragraph_block(&paragraph) {
                            blocks.push(block);
                        }
                    }
                    "tbl" => blocks.push(Block::Table(word_table(reader, "DOCX")?)),
                    // A tracked deletion wraps the content it removed, which is
                    // not part of the document a reader sees.
                    "del" | "moveFrom" => skip_element(reader, "DOCX")?,
                    // Content controls, tracked insertions and smart tags wrap
                    // ordinary blocks, so their content belongs at this level.
                    "sdt" | "sdtContent" | "ins" | "customXml" | "smartTag" => {
                        if depth < MAX_XML_DEPTH {
                            docx_blocks(reader, &name, depth + 1, blocks)?;
                        } else {
                            skip_element(reader, "DOCX")?;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn paragraph_block(paragraph: &Paragraph) -> Option<Block> {
    let text = paragraph.text.trim().to_owned();
    if text.is_empty() {
        return None;
    }
    if let Some(level) = paragraph.heading {
        return Some(Block::Heading { level, text });
    }
    match paragraph.list_level {
        Some(level) => Some(Block::ListItem { level, text }),
        None => Some(Block::Paragraph(text)),
    }
}

/// Reads one `<w:p>` or `<a:p>`, collecting its run text and the paragraph
/// properties each format stores in a `pPr` child.
fn paragraph(
    reader: &mut XmlReader<&[u8]>,
    end: &str,
    kind: ParagraphKind,
    format: &'static str,
) -> Result<Paragraph, ConvertError> {
    let mut paragraph = Paragraph {
        text: String::new(),
        heading: None,
        list_level: None,
    };
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error(format, error))?
        {
            Event::Eof => return Ok(paragraph),
            Event::End(element) if local_name(&element.name()) == end => return Ok(paragraph),
            Event::Start(element) => {
                let name = local_name(&element.name()).to_owned();
                match name.as_str() {
                    "pPr" => match kind {
                        ParagraphKind::Word => word_properties(reader, &mut paragraph)?,
                        ParagraphKind::Slide => slide_properties(reader, &mut paragraph)?,
                    },
                    "t" => {
                        let text = element_text(reader, "t", format)?;
                        paragraph.text.push_str(&text);
                    }
                    // Deleted text, field instructions, and the drawing subtrees
                    // (which carry shape names and text-box content of their own)
                    // are not the paragraph's own text.
                    "del" | "delText" | "instrText" | "drawing" | "pict" | "object"
                    | "txbxContent" | "footnoteReference" | "endnoteReference"
                    | "commentReference" => skip_element(reader, format)?,
                    _ => {}
                }
            }
            Event::Empty(element) => {
                let name = local_name(&element.name()).to_owned();
                match name.as_str() {
                    "tab" => paragraph.text.push('\t'),
                    "br" | "cr" | "line-break" => paragraph.text.push('\n'),
                    "noBreakHyphen" => paragraph.text.push('-'),
                    "delText" | "instrText" => {}
                    _ => {}
                }
            }
            Event::GeneralRef(element) => push_reference(&mut paragraph.text, element),
            _ => {}
        }
    }
}

fn word_properties(
    reader: &mut XmlReader<&[u8]>,
    paragraph: &mut Paragraph,
) -> Result<(), ConvertError> {
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("DOCX", error))?
        {
            Event::Eof => return Ok(()),
            Event::End(element) if local_name(&element.name()) == "pPr" => return Ok(()),
            Event::Start(element) => match local_name(&element.name()).to_owned().as_str() {
                "pStyle" => paragraph.heading = heading_level(&attribute(&element, "val")),
                "outlineLvl" => {
                    if paragraph.heading.is_none() {
                        paragraph.heading = outline_level(&element);
                    }
                }
                "numPr" => paragraph.list_level = Some(numbering_level(reader)?),
                _ => {}
            },
            Event::Empty(element) => match local_name(&element.name()).to_owned().as_str() {
                "pStyle" => paragraph.heading = heading_level(&attribute(&element, "val")),
                "outlineLvl" => {
                    if paragraph.heading.is_none() {
                        paragraph.heading = outline_level(&element);
                    }
                }
                // A `numPr` with no level child is a first-level list item.
                "numPr" => paragraph.list_level = Some(1),
                _ => {}
            },
            _ => {}
        }
    }
}

fn numbering_level(reader: &mut XmlReader<&[u8]>) -> Result<u8, ConvertError> {
    let mut level = 1_u8;
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("DOCX", error))?
        {
            Event::Eof => return Ok(level),
            Event::End(element) if local_name(&element.name()) == "numPr" => return Ok(level),
            Event::Start(element) | Event::Empty(element)
                if local_name(&element.name()) == "ilvl" =>
            {
                level = attribute(&element, "val")
                    .and_then(|value| value.parse::<u8>().ok())
                    .unwrap_or(0)
                    .saturating_add(1);
            }
            _ => {}
        }
    }
}

fn outline_level(element: &BytesStart<'_>) -> Option<u8> {
    attribute(element, "val")
        .and_then(|value| value.parse::<u8>().ok())
        .map(|level| level.saturating_add(1).min(9))
}

/// The heading level a paragraph style names. Word writes `Heading1`, some
/// templates write `Heading 1` or a bare level number, and the outline level in
/// the paragraph properties is the format's own fallback.
fn heading_level(style: &Option<String>) -> Option<u8> {
    let style = style.as_deref()?;
    let normalized: String = style
        .chars()
        .filter(|character| !matches!(character, ' ' | '_' | '-'))
        .collect::<String>()
        .to_ascii_lowercase();
    if let Some(digits) = normalized.strip_prefix("heading") {
        if let Ok(level) = digits.parse::<u8>() {
            if (1..=9).contains(&level) {
                return Some(level);
            }
        }
    }
    match normalized.parse::<u8>() {
        Ok(level) if (1..=9).contains(&level) => Some(level),
        _ => None,
    }
}

fn slide_properties(
    reader: &mut XmlReader<&[u8]>,
    paragraph: &mut Paragraph,
) -> Result<(), ConvertError> {
    let mut bullet = false;
    let mut level = 1_u8;
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("PPTX", error))?
        {
            Event::Eof => break,
            Event::End(element) if local_name(&element.name()) == "pPr" => break,
            Event::Start(element) | Event::Empty(element) => {
                match local_name(&element.name()).to_owned().as_str() {
                    "lvl" => {
                        level = attribute(&element, "val")
                            .and_then(|value| value.parse::<u8>().ok())
                            .unwrap_or(0)
                            .saturating_add(1);
                    }
                    "buChar" | "buAutoNum" => bullet = true,
                    "buNone" => bullet = false,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if bullet {
        paragraph.list_level = Some(level);
    }
    Ok(())
}

fn word_table(
    reader: &mut XmlReader<&[u8]>,
    format: &'static str,
) -> Result<Vec<Vec<String>>, ConvertError> {
    let mut rows = Vec::new();
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error(format, error))?
        {
            Event::Eof => return Ok(rows),
            Event::End(element) if local_name(&element.name()) == "tbl" => return Ok(rows),
            Event::Start(element) => match local_name(&element.name()).to_owned().as_str() {
                "tr" => rows.push(word_row(reader, format)?),
                "tblPr" | "tblGrid" => skip_element(reader, format)?,
                _ => {}
            },
            _ => {}
        }
    }
}

fn word_row(
    reader: &mut XmlReader<&[u8]>,
    format: &'static str,
) -> Result<Vec<String>, ConvertError> {
    let mut cells = Vec::new();
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error(format, error))?
        {
            Event::Eof => return Ok(cells),
            Event::End(element) if local_name(&element.name()) == "tr" => return Ok(cells),
            Event::Start(element) => match local_name(&element.name()).to_owned().as_str() {
                "tc" => {
                    let (text, span) = word_cell(reader, format)?;
                    cells.push(text);
                    for _ in 1..span {
                        cells.push(String::new());
                    }
                }
                "trPr" => skip_element(reader, format)?,
                _ => {}
            },
            _ => {}
        }
    }
}

fn word_cell(
    reader: &mut XmlReader<&[u8]>,
    format: &'static str,
) -> Result<(String, u32), ConvertError> {
    let mut paragraphs: Vec<String> = Vec::new();
    let mut span = 1_u32;
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error(format, error))?
        {
            Event::Eof => return Ok((paragraphs.join(" "), span)),
            Event::End(element) if local_name(&element.name()) == "tc" => {
                return Ok((paragraphs.join(" "), span))
            }
            Event::Start(element) => match local_name(&element.name()).to_owned().as_str() {
                "p" => {
                    let paragraph = paragraph(reader, "p", ParagraphKind::Word, format)?;
                    let text = paragraph.text.trim();
                    if !text.is_empty() {
                        paragraphs.push(text.to_owned());
                    }
                }
                "tcPr" => span = grid_span(reader)?,
                // A table inside a cell is flattened into that cell's text.
                "tbl" => {
                    for row in word_table(reader, format)? {
                        let text = row.join(" ");
                        if !text.trim().is_empty() {
                            paragraphs.push(text);
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn grid_span(reader: &mut XmlReader<&[u8]>) -> Result<u32, ConvertError> {
    let mut span = 1_u32;
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("DOCX", error))?
        {
            Event::Eof => return Ok(span),
            Event::End(element) if local_name(&element.name()) == "tcPr" => return Ok(span),
            Event::Start(element) | Event::Empty(element)
                if local_name(&element.name()) == "gridSpan" =>
            {
                span = attribute(&element, "val")
                    .and_then(|value| value.parse::<u32>().ok())
                    .unwrap_or(1)
                    .max(1);
            }
            _ => {}
        }
    }
}

fn pptx_text(input: &[u8]) -> Result<String, ConvertError> {
    let mut archive = package_of(input, "PPTX")?;
    let presentation = read_required_part(&mut archive, "ppt/presentation.xml", "PPTX")?;
    let relationships = read_part(&mut archive, "ppt/_rels/presentation.xml.rels", "PPTX")?;
    let relationships = match relationships {
        Some(bytes) => Some(decode_part(&bytes, "PPTX")?),
        None => None,
    };
    let presentation = decode_part(&presentation, "PPTX")?;
    let order = slide_order(&presentation, relationships.as_deref())?;
    let mut rendered = String::new();
    for (position, part) in order.iter().enumerate() {
        let Some(bytes) = read_part(&mut archive, part, "PPTX")? else {
            continue;
        };
        let blocks = slide_blocks(&decode_part(&bytes, "PPTX")?)?;
        if blocks.is_empty() {
            continue;
        }
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        rendered.push_str(&format!("# Slide {}\n\n", position + 1));
        rendered.push_str(&render_blocks(&blocks));
    }
    if rendered.is_empty() {
        return Err(ConvertError::OfficeEmptyBody { format: "PPTX" });
    }
    Ok(rendered)
}

/// The slide parts in presentation order. The order lives in
/// `ppt/presentation.xml` as relationship ids, not in the part names: a deck
/// whose slides were reordered still has `slide1.xml` first on disk.
fn slide_order(
    presentation: &str,
    relationships: Option<&str>,
) -> Result<Vec<String>, ConvertError> {
    let mut ids = Vec::new();
    let mut reader = XmlReader::from_str(presentation);
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("PPTX", error))?
        {
            Event::Eof => break,
            Event::Start(element) | Event::Empty(element)
                if local_name(&element.name()) == "sldId" =>
            {
                if let Some(id) = relationship_id(&element) {
                    ids.push(id);
                }
            }
            _ => {}
        }
    }
    let Some(relationships) = relationships else {
        return Ok(Vec::new());
    };
    let targets = relationship_targets(relationships)?;
    Ok(ids
        .into_iter()
        .filter_map(|id| targets.get(&id).cloned())
        .collect())
}

fn relationship_id(element: &BytesStart<'_>) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|attribute| {
            attribute.key.local_name().into_inner() == "id" && attribute.key.prefix().is_some()
        })
        .and_then(|attribute| attribute.normalized_value(XmlVersion::Implicit1_0).ok())
        .map(|value| value.into_owned())
}

fn relationship_targets(relationships: &str) -> Result<HashMap<String, String>, ConvertError> {
    let mut targets = HashMap::new();
    let mut reader = XmlReader::from_str(relationships);
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("PPTX", error))?
        {
            Event::Eof => return Ok(targets),
            Event::Start(element) | Event::Empty(element) => {
                if local_name(&element.name()) != "Relationship" {
                    continue;
                }
                let external = attribute(&element, "TargetMode")
                    .is_some_and(|mode| mode.eq_ignore_ascii_case("external"));
                if external {
                    continue;
                }
                let (Some(id), Some(target)) =
                    (attribute(&element, "Id"), attribute(&element, "Target"))
                else {
                    continue;
                };
                targets.insert(id, slide_part(&target));
            }
            _ => {}
        }
    }
}

fn slide_part(target: &str) -> String {
    let target = target.replace('\\', "/");
    let target = target.trim_start_matches('/');
    if target.starts_with("ppt/") {
        target.to_owned()
    } else {
        format!("ppt/{target}")
    }
}

fn slide_blocks(slide: &str) -> Result<Vec<Block>, ConvertError> {
    let mut blocks = Vec::new();
    let mut reader = XmlReader::from_str(slide);
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("PPTX", error))?
        {
            Event::Eof => return Ok(blocks),
            Event::Start(element) => match local_name(&element.name()).to_owned().as_str() {
                "p" => {
                    let paragraph = paragraph(&mut reader, "p", ParagraphKind::Slide, "PPTX")?;
                    if let Some(block) = paragraph_block(&paragraph) {
                        blocks.push(block);
                    }
                }
                "tbl" => blocks.push(Block::Table(word_table(&mut reader, "PPTX")?)),
                _ => {}
            },
            _ => {}
        }
    }
}

// ---- OpenDocument text ----------------------------------------------------

fn odt_text(input: &[u8]) -> Result<String, ConvertError> {
    let mut archive = package_of(input, "ODT")?;
    let part = read_required_part(&mut archive, "content.xml", "ODT")?;
    let content = decode_part(&part, "ODT")?;
    let mut reader = XmlReader::from_str(&content);
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("ODT", error))?
        {
            Event::Start(element) if local_name(&element.name()) == "body" => break,
            Event::Eof => {
                return Err(ConvertError::OfficeCorrupt {
                    format: "ODT",
                    message: "the content part has no office:body".to_owned(),
                })
            }
            _ => {}
        }
    }
    let mut blocks = Vec::new();
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("ODT", error))?
        {
            Event::Start(element) if local_name(&element.name()) == "text" => {
                odt_blocks(&mut reader, "text", None, 0, &mut blocks)?;
                break;
            }
            Event::End(element) if local_name(&element.name()) == "body" => break,
            Event::Eof => break,
            _ => {}
        }
    }
    let text = render_blocks(&blocks);
    if text.is_empty() {
        return Err(ConvertError::OfficeEmptyBody { format: "ODT" });
    }
    Ok(text)
}

fn odt_blocks(
    reader: &mut XmlReader<&[u8]>,
    end: &str,
    list_level: Option<u8>,
    depth: usize,
    blocks: &mut Vec<Block>,
) -> Result<(), ConvertError> {
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("ODT", error))?
        {
            Event::Eof => return Ok(()),
            Event::End(element) if local_name(&element.name()) == end => return Ok(()),
            Event::Start(element) => {
                let name = local_name(&element.name()).to_owned();
                match name.as_str() {
                    "h" => {
                        let level = attribute(&element, "outline-level")
                            .and_then(|value| value.parse::<u8>().ok())
                            .unwrap_or(1)
                            .clamp(1, 9);
                        let text = odt_inline(reader, "h")?.trim().to_owned();
                        if !text.is_empty() {
                            blocks.push(Block::Heading { level, text });
                        }
                    }
                    "p" => {
                        let text = odt_inline(reader, "p")?.trim().to_owned();
                        if !text.is_empty() {
                            blocks.push(match list_level {
                                Some(level) => Block::ListItem { level, text },
                                None => Block::Paragraph(text),
                            });
                        }
                    }
                    "list" => {
                        let level = list_level.unwrap_or(0).saturating_add(1).min(9);
                        if depth < MAX_XML_DEPTH {
                            odt_blocks(reader, &name, Some(level), depth + 1, blocks)?;
                        } else {
                            skip_element(reader, "ODT")?;
                        }
                    }
                    "list-item" | "section" | "list-header" => {
                        if depth < MAX_XML_DEPTH {
                            odt_blocks(reader, &name, list_level, depth + 1, blocks)?;
                        } else {
                            skip_element(reader, "ODT")?;
                        }
                    }
                    "table" => blocks.push(Block::Table(odt_table(reader)?)),
                    "tracked-changes" | "annotation" | "note" | "frame" | "sequence-decls"
                    | "user-field-decls" | "variable-decls" | "forms" => {
                        skip_element(reader, "ODT")?
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

fn odt_inline(reader: &mut XmlReader<&[u8]>, end: &str) -> Result<String, ConvertError> {
    let mut text = String::new();
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("ODT", error))?
        {
            Event::Eof => return Ok(text),
            Event::End(element) if local_name(&element.name()) == end => return Ok(text),
            Event::Start(element) => {
                let name = local_name(&element.name()).to_owned();
                match name.as_str() {
                    // `text:s` is a run of spaces: the attribute counts them.
                    "s" => text.push_str(&" ".repeat(space_count(&element))),
                    "tab" => text.push('\t'),
                    "line-break" => text.push('\n'),
                    "note" | "annotation" | "frame" | "ruby-text" => skip_element(reader, "ODT")?,
                    _ => {}
                }
            }
            Event::Empty(element) => {
                let name = local_name(&element.name()).to_owned();
                match name.as_str() {
                    "s" => text.push_str(&" ".repeat(space_count(&element))),
                    "tab" => text.push('\t'),
                    "line-break" => text.push('\n'),
                    _ => {}
                }
            }
            Event::Text(element) => text.push_str(&element.xml10_content()),
            Event::CData(element) => text.push_str(&element.into_inner()),
            Event::GeneralRef(element) => push_reference(&mut text, element),
            _ => {}
        }
    }
}

fn space_count(element: &BytesStart<'_>) -> usize {
    attribute(element, "c")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, MAX_TABLE_REPEAT)
}

fn odt_table(reader: &mut XmlReader<&[u8]>) -> Result<Vec<Vec<String>>, ConvertError> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("ODT", error))?
        {
            Event::Eof => break,
            Event::End(element) if local_name(&element.name()) == "table" => break,
            Event::Start(element) if local_name(&element.name()) == "table-row" => {
                let repeat = row_repeat(&element);
                let row = odt_row(reader)?;
                push_row(&mut rows, row, repeat);
            }
            _ => {}
        }
    }
    // The used range ends at the last row that holds anything, so the empty
    // remainder LibreOffice writes for the rest of the sheet does not become
    // trailing blank lines.
    while rows
        .last()
        .is_some_and(|row| row.iter().all(String::is_empty))
    {
        rows.pop();
    }
    Ok(rows)
}

fn row_repeat(element: &BytesStart<'_>) -> usize {
    attribute(element, "number-rows-repeated")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, MAX_TABLE_REPEAT)
}

fn push_row(rows: &mut Vec<Vec<String>>, row: Vec<String>, repeat: usize) {
    let empty = row.iter().all(String::is_empty);
    // A repeated empty row is padding, not content: one copy is enough to keep
    // the rows below it in place.
    for _ in 0..if empty { 1 } else { repeat } {
        rows.push(row.clone());
    }
}

fn odt_row(reader: &mut XmlReader<&[u8]>) -> Result<Vec<String>, ConvertError> {
    let mut cells: Vec<(usize, String)> = Vec::new();
    let mut column = 0_usize;
    let mut width = 0_usize;
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("ODT", error))?
        {
            Event::Eof => break,
            Event::End(element) if local_name(&element.name()) == "table-row" => break,
            Event::Start(element) => match local_name(&element.name()).to_owned().as_str() {
                "table-cell" => {
                    let repeat = cell_repeat(&element);
                    let text = odt_cell_text(reader)?.trim().to_owned();
                    place_cell(&mut cells, column, text, repeat, &mut width);
                    column = column.saturating_add(repeat);
                }
                "covered-table-cell" => {
                    column = column.saturating_add(cell_repeat(&element));
                    skip_element(reader, "ODT")?;
                }
                _ => {}
            },
            Event::Empty(element) => {
                let name = local_name(&element.name()).to_owned();
                match name.as_str() {
                    "table-cell" => {
                        let repeat = cell_repeat(&element);
                        place_cell(&mut cells, column, String::new(), repeat, &mut width);
                        column = column.saturating_add(repeat);
                    }
                    "covered-table-cell" => {
                        column = column.saturating_add(cell_repeat(&element));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let width = width.min(MAX_TABLE_COLUMNS);
    let mut row = vec![String::new(); width];
    for (index, text) in cells {
        if index < width {
            row[index] = text;
        }
    }
    Ok(row)
}

fn place_cell(
    cells: &mut Vec<(usize, String)>,
    column: usize,
    text: String,
    repeat: usize,
    width: &mut usize,
) {
    if text.is_empty() {
        return;
    }
    *width = (*width).max(column.saturating_add(repeat).min(MAX_TABLE_COLUMNS));
    cells.push((column, text));
}

fn cell_repeat(element: &BytesStart<'_>) -> usize {
    attribute(element, "number-columns-repeated")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, MAX_TABLE_REPEAT)
}

fn odt_cell_text(reader: &mut XmlReader<&[u8]>) -> Result<String, ConvertError> {
    let mut paragraphs: Vec<String> = Vec::new();
    loop {
        match reader
            .read_event()
            .map_err(|error| xml_error("ODT", error))?
        {
            Event::Eof => return Ok(paragraphs.join(" ")),
            Event::End(element) if local_name(&element.name()) == "table-cell" => {
                return Ok(paragraphs.join(" "))
            }
            Event::Start(element) => match local_name(&element.name()).to_owned().as_str() {
                "p" => {
                    let text = odt_inline(reader, "p")?.trim().to_owned();
                    if !text.is_empty() {
                        paragraphs.push(text);
                    }
                }
                "h" => {
                    let text = odt_inline(reader, "h")?.trim().to_owned();
                    if !text.is_empty() {
                        paragraphs.push(text);
                    }
                }
                // A table inside a cell is flattened into that cell's text.
                "table" => {
                    for row in odt_table(reader)? {
                        let text = row.join(" ");
                        if !text.trim().is_empty() {
                            paragraphs.push(text);
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

// ---- Workbooks ------------------------------------------------------------

struct Sheet {
    name: String,
    rows: Vec<Vec<Data>>,
}

fn read_workbook(kind: OfficeKind, input: &[u8]) -> Result<Vec<Sheet>, ConvertError> {
    let format = kind.label();
    match kind {
        OfficeKind::Xlsx => {
            let mut reader = Xlsx::new(Cursor::new(input)).map_err(|error| match error {
                calamine::XlsxError::Password => ConvertError::OfficeEncrypted,
                other => ConvertError::OfficeCorrupt {
                    format,
                    message: other.to_string(),
                },
            })?;
            read_sheets(&mut reader, format, |error| match error {
                calamine::XlsxError::Password => ConvertError::OfficeEncrypted,
                other => ConvertError::OfficeCorrupt {
                    format,
                    message: other.to_string(),
                },
            })
        }
        OfficeKind::Xls => {
            let mut reader = Xls::new(Cursor::new(input)).map_err(|error| match error {
                calamine::XlsError::Password => ConvertError::OfficeEncrypted,
                other => ConvertError::OfficeCorrupt {
                    format,
                    message: other.to_string(),
                },
            })?;
            read_sheets(&mut reader, format, |error| match error {
                calamine::XlsError::Password => ConvertError::OfficeEncrypted,
                other => ConvertError::OfficeCorrupt {
                    format,
                    message: other.to_string(),
                },
            })
        }
        OfficeKind::Ods => {
            let mut reader = Ods::new(Cursor::new(input)).map_err(|error| match error {
                calamine::OdsError::Password => ConvertError::OfficeEncrypted,
                other => ConvertError::OfficeCorrupt {
                    format,
                    message: other.to_string(),
                },
            })?;
            read_sheets(&mut reader, format, |error| match error {
                calamine::OdsError::Password => ConvertError::OfficeEncrypted,
                other => ConvertError::OfficeCorrupt {
                    format,
                    message: other.to_string(),
                },
            })
        }
        _ => unreachable!("only the workbook kinds reach the spreadsheet reader"),
    }
}

fn read_sheets<RS, E, F>(
    reader: &mut impl Reader<RS, Error = E>,
    format: &'static str,
    map: F,
) -> Result<Vec<Sheet>, ConvertError>
where
    RS: Read + std::io::Seek,
    F: Fn(E) -> ConvertError,
{
    let mut sheets = Vec::new();
    for name in reader.sheet_names() {
        let range = reader.worksheet_range(&name).map_err(&map)?;
        let rows: Vec<Vec<Data>> = range.rows().map(<[Data]>::to_vec).collect();
        if rows.iter().all(|row| row.iter().all(DataType::is_empty)) {
            continue;
        }
        sheets.push(Sheet { name, rows });
    }
    if sheets.is_empty() {
        return Err(ConvertError::OfficeEmptyBody { format });
    }
    Ok(sheets)
}

fn workbook_output(
    input_name: &str,
    sheets: &[Sheet],
    target: OutputFormat,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    match target {
        OutputFormat::Csv => {
            let mut used = HashSet::new();
            let mut files = Vec::with_capacity(sheets.len());
            for sheet in sheets {
                let mut writer = csv::Writer::from_writer(Vec::new());
                for row in &sheet.rows {
                    writer.write_record(row.iter().map(cell_text))?;
                }
                writer.flush()?;
                files.push(ConvertedFile {
                    name: sheet_output_name(input_name, &sheet.name, &mut used),
                    bytes: writer.into_inner().map_err(|error| error.into_error())?,
                });
            }
            Ok(files)
        }
        OutputFormat::Json => {
            let sheets = sheets
                .iter()
                .map(|sheet| {
                    serde_json::json!({
                        "name": sheet.name,
                        "rows": sheet
                            .rows
                            .iter()
                            .map(|row| Value::Array(row.iter().map(cell_json).collect()))
                            .collect::<Vec<_>>(),
                    })
                })
                .collect::<Vec<_>>();
            let value = serde_json::json!({ "sheets": sheets });
            Ok(vec![ConvertedFile {
                name: output_name(input_name, target.extension()),
                bytes: serde_json::to_vec_pretty(&value)?,
            }])
        }
        _ => {
            let mut text = String::new();
            for (index, sheet) in sheets.iter().enumerate() {
                if index > 0 {
                    text.push('\n');
                }
                text.push_str(&format!("## {}\n\n", sheet.name));
                for row in &sheet.rows {
                    let line = row.iter().map(cell_text).collect::<Vec<_>>().join("\t");
                    text.push_str(line.trim_end());
                    text.push('\n');
                }
            }
            Ok(vec![ConvertedFile {
                name: output_name(input_name, target.extension()),
                bytes: encode_text(&text, target)?,
            }])
        }
    }
}

/// `<stem>_<sheet>.<extension>`, the same shape the PDF page targets use: one
/// table per file, named after the sheet it came from.
fn sheet_output_name(input_name: &str, sheet: &str, used: &mut HashSet<String>) -> PathBuf {
    let stem = Path::new(input_name)
        .file_stem()
        .filter(|stem| !stem.is_empty())
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "converted".to_owned());
    let base = file_component(sheet);
    let mut candidate = format!("{stem}_{base}");
    let mut index = 2;
    while !used.insert(candidate.to_lowercase()) {
        candidate = format!("{stem}_{base}_{index}");
        index += 1;
    }
    PathBuf::from(format!("{candidate}.{}", OutputFormat::Csv.extension()))
}

fn file_component(name: &str) -> String {
    let mut component: String = name
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    while component.ends_with('_') {
        component.pop();
    }
    let component = component.trim_start_matches('_');
    if component.is_empty() {
        "sheet".to_owned()
    } else {
        component.to_owned()
    }
}

fn cell_text(cell: &Data) -> String {
    match cell {
        Data::Int(value) => value.to_string(),
        Data::Float(value) => float_text(*value),
        Data::String(value) => value.clone(),
        Data::Bool(value) => {
            if *value {
                "TRUE".to_owned()
            } else {
                "FALSE".to_owned()
            }
        }
        Data::DateTime(value) => date_time_text(value),
        Data::DateTimeIso(value) | Data::DurationIso(value) => value.clone(),
        Data::Error(error) => error.to_string(),
        Data::Empty => String::new(),
    }
}

fn cell_json(cell: &Data) -> Value {
    match cell {
        Data::Int(value) => Value::Number((*value).into()),
        Data::Float(value) => number_json(*value),
        Data::Bool(value) => Value::Bool(*value),
        Data::Empty => Value::Null,
        _ => Value::String(cell_text(cell)),
    }
}

fn number_json(value: f64) -> Value {
    if !value.is_finite() {
        return Value::Null;
    }
    if value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 {
        return Value::Number((value as i64).into());
    }
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn float_text(value: f64) -> String {
    // Rust's `f64` Display is the shortest representation that round-trips, so
    // a stored `0.1` stays `0.1` instead of drifting in the sixteenth digit.
    format!("{value}")
}

/// A stored date, time or duration as ISO 8601. Excel keeps all three as a
/// serial number and a number format, and the format is not on the cell here, so
/// a bare fraction of a day is read as a time of day and a duration is written
/// as hours, minutes and seconds.
fn date_time_text(value: &ExcelDateTime) -> String {
    let raw = value.as_f64();
    if value.is_duration() {
        return duration_text(raw);
    }
    if (0.0..1.0).contains(&raw) {
        let seconds = (raw * 86_400.0).round() as u64;
        return clock_text(seconds);
    }
    let (year, month, day, hour, minute, second, milli) = value.to_ymd_hms_milli();
    let date = format!("{year:04}-{month:02}-{day:02}");
    if hour == 0 && minute == 0 && second == 0 && milli == 0 {
        return date;
    }
    let clock = format!("{hour:02}:{minute:02}:{second:02}");
    if milli == 0 {
        format!("{date}T{clock}")
    } else {
        format!("{date}T{clock}.{milli:03}")
    }
}

fn duration_text(raw: f64) -> String {
    let negative = raw < 0.0;
    let seconds = (raw.abs() * 86_400.0).round() as u64;
    let clock = clock_text(seconds);
    if negative {
        format!("-{clock}")
    } else {
        clock
    }
}

fn clock_text(seconds: u64) -> String {
    format!(
        "{}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::convert_bytes;

    fn fixture(name: &str) -> Vec<u8> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    #[test]
    fn detects_office_containers_before_their_extension() {
        assert_eq!(
            detect("report.docx", &fixture("sample-office.docx")).unwrap(),
            Some(OfficeKind::Docx)
        );
        assert_eq!(
            detect("renamed.zip", &fixture("sample-office.docx")).unwrap(),
            Some(OfficeKind::Docx)
        );
        assert_eq!(
            detect("report.docx", &fixture("sample-office.odt")).unwrap(),
            Some(OfficeKind::Odt)
        );
        assert_eq!(
            detect("book.zip", &fixture("sample-office.xlsx")).unwrap(),
            Some(OfficeKind::Xlsx)
        );
        assert_eq!(
            detect("deck.bin", &fixture("sample-office.pptx")).unwrap(),
            Some(OfficeKind::Pptx)
        );
        assert_eq!(
            detect("sheet.ods", &fixture("sample-office.ods")).unwrap(),
            Some(OfficeKind::Ods)
        );
        assert_eq!(
            detect("sheet.xls", &fixture("sample-office.xls")).unwrap(),
            Some(OfficeKind::Xls)
        );
        // A ZIP that holds none of an office document's parts is just an
        // archive, and a DOCX whose container is not a package is corrupt.
        assert_eq!(
            detect("bundle.zip", &fixture("sample-office-plain.zip")).unwrap(),
            None
        );
        assert!(matches!(
            detect("note.docx", b"hello"),
            Err(ConvertError::OfficeCorrupt { .. })
        ));
    }

    #[test]
    fn docx_becomes_text_with_headings_lists_and_tables() {
        let files = convert_bytes(
            "report.docx",
            &fixture("sample-office.docx"),
            OutputFormat::Text,
        )
        .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, PathBuf::from("report.txt"));
        let text = String::from_utf8(files[0].bytes.clone()).unwrap();
        assert_eq!(
            text,
            "\
# Quarterly report

The first paragraph, with a tab\tand a line break
that continues here.

## Details

- first bullet
  - nested bullet

| Item | Quantity | Notes |
| --- | --- | --- |
| Widgets | 3 | new |
| Gadgets | 4 |  |
| Spanned |  | last |

A closing paragraph with an & in it.
"
        );
    }

    #[test]
    fn docx_json_is_the_extracted_text() {
        let text = convert_bytes(
            "report.docx",
            &fixture("sample-office.docx"),
            OutputFormat::Text,
        )
        .unwrap();
        let json = convert_bytes(
            "report.docx",
            &fixture("sample-office.docx"),
            OutputFormat::Json,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&json[0].bytes).unwrap();
        assert_eq!(
            value,
            serde_json::Value::String(String::from_utf8(text[0].bytes.clone()).unwrap())
        );
    }

    #[test]
    fn docx_csv_is_the_one_column_text_table() {
        let files = convert_bytes(
            "report.docx",
            &fixture("sample-office.docx"),
            OutputFormat::Csv,
        )
        .unwrap();
        let mut reader = csv::Reader::from_reader(files[0].bytes.as_slice());
        assert_eq!(
            reader.headers().unwrap(),
            &csv::StringRecord::from(vec!["text"])
        );
        assert_eq!(
            reader.records().next().unwrap().unwrap().get(0),
            Some("# Quarterly report")
        );
    }

    #[test]
    fn docx_text_encodings_round_trip() {
        for target in [
            OutputFormat::TextUtf8Bom,
            OutputFormat::TextUtf16Le,
            OutputFormat::TextUtf16Be,
            OutputFormat::TextUtf32Le,
            OutputFormat::TextUtf32Be,
        ] {
            let encoded =
                convert_bytes("report.docx", &fixture("sample-office.docx"), target).unwrap();
            let decoded =
                convert_bytes("report.txt", &encoded[0].bytes, OutputFormat::Text).unwrap();
            let plain = convert_bytes(
                "report.docx",
                &fixture("sample-office.docx"),
                OutputFormat::Text,
            )
            .unwrap();
            assert_eq!(decoded[0].bytes, plain[0].bytes);
        }
    }

    #[test]
    fn pptx_slides_follow_the_presentation_order() {
        let files = convert_bytes(
            "deck.pptx",
            &fixture("sample-office.pptx"),
            OutputFormat::Text,
        )
        .unwrap();
        let text = String::from_utf8(files[0].bytes.clone()).unwrap();
        assert_eq!(
            text,
            "\
# Slide 1

Second slide part, first in the deck

# Slide 2

First slide part, second in the deck
"
        );
    }

    #[test]
    fn odt_becomes_text_with_headings_lists_and_tables() {
        let files = convert_bytes(
            "notes.odt",
            &fixture("sample-office.odt"),
            OutputFormat::Text,
        )
        .unwrap();
        let text = String::from_utf8(files[0].bytes.clone()).unwrap();
        assert_eq!(
            text,
            "\
# OpenDocument heading

A paragraph with   three spaces and a tab\there.

- one
- two

| Name | Value |
| --- | --- |
| Alpha | 1 |
| Beta | 2 |
| Gamma |  |
"
        );
    }

    #[test]
    fn xlsx_sheets_become_one_csv_per_sheet() {
        let files = convert_bytes(
            "book.xlsx",
            &fixture("sample-office.xlsx"),
            OutputFormat::Csv,
        )
        .unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|file| file.name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["book_Inventory.csv", "book_Notes.csv"]);

        let mut reader = csv::Reader::from_reader(files[0].bytes.as_slice());
        assert_eq!(
            reader.headers().unwrap(),
            &csv::StringRecord::from(vec!["Item", "Quantity", "Price", "In stock", "Updated"])
        );
        let rows: Vec<Vec<String>> = reader
            .records()
            .map(|record| record.unwrap().iter().map(str::to_owned).collect())
            .collect();
        assert_eq!(rows[0], ["Widgets", "3", "1.5", "TRUE", "2024-03-09"]);
        assert_eq!(
            rows[1],
            ["Gadgets", "4", "2.25", "FALSE", "2024-03-10T12:00:00"]
        );
    }

    #[test]
    fn xlsx_json_keeps_cell_types() {
        let files = convert_bytes(
            "book.xlsx",
            &fixture("sample-office.xlsx"),
            OutputFormat::Json,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&files[0].bytes).unwrap();
        assert_eq!(value["sheets"][0]["name"], "Inventory");
        assert_eq!(value["sheets"][0]["rows"][0][0], "Item");
        assert_eq!(value["sheets"][0]["rows"][1][1], 3);
        assert_eq!(value["sheets"][0]["rows"][1][2], 1.5);
        assert_eq!(value["sheets"][0]["rows"][1][3], true);
        assert_eq!(value["sheets"][0]["rows"][1][4], "2024-03-09");
        assert_eq!(value["sheets"][1]["name"], "Notes");
    }

    #[test]
    fn workbook_text_groups_rows_by_sheet() {
        let files = convert_bytes(
            "book.xlsx",
            &fixture("sample-office.xlsx"),
            OutputFormat::Text,
        )
        .unwrap();
        let text = String::from_utf8(files[0].bytes.clone()).unwrap();
        assert!(
            text.starts_with("## Inventory\n\nItem\tQuantity\tPrice\tIn stock\tUpdated\n"),
            "{text}"
        );
        assert!(
            text.contains("\n\n## Notes\n\nA note in the second sheet\n"),
            "{text}"
        );
    }

    #[test]
    fn legacy_and_opendocument_workbooks_are_read() {
        let xls =
            convert_bytes("book.xls", &fixture("sample-office.xls"), OutputFormat::Csv).unwrap();
        assert_eq!(xls[0].name, PathBuf::from("book_Legacy.csv"));
        assert_eq!(
            String::from_utf8(xls[0].bytes.clone()).unwrap(),
            "Name,Count,Flag\nfirst,1,TRUE\nsecond,2,FALSE\n"
        );

        let ods =
            convert_bytes("book.ods", &fixture("sample-office.ods"), OutputFormat::Csv).unwrap();
        assert_eq!(ods[0].name, PathBuf::from("book_Data.csv"));
        assert_eq!(
            String::from_utf8(ods[0].bytes.clone()).unwrap(),
            "Name,Count\nthird,3\nfourth,4\n"
        );
    }

    #[test]
    fn encrypted_corrupt_and_empty_documents_are_typed_errors() {
        let encrypted = convert_bytes(
            "secret.docx",
            &fixture("sample-office-encrypted.docx"),
            OutputFormat::Text,
        )
        .unwrap_err();
        assert!(
            matches!(encrypted, ConvertError::OfficeEncrypted),
            "{encrypted}"
        );

        let encrypted_odf = convert_bytes(
            "secret.odt",
            &fixture("sample-office-encrypted.odt"),
            OutputFormat::Text,
        )
        .unwrap_err();
        assert!(
            matches!(encrypted_odf, ConvertError::OfficeEncrypted),
            "{encrypted_odf}"
        );

        let corrupt = convert_bytes(
            "broken.docx",
            &fixture("sample-office-corrupt.docx"),
            OutputFormat::Text,
        )
        .unwrap_err();
        assert!(
            matches!(corrupt, ConvertError::OfficeCorrupt { .. }),
            "{corrupt}"
        );

        let empty = convert_bytes(
            "empty.docx",
            &fixture("sample-office-empty.docx"),
            OutputFormat::Text,
        )
        .unwrap_err();
        assert!(
            matches!(empty, ConvertError::OfficeEmptyBody { .. }),
            "{empty}"
        );
    }

    #[test]
    fn a_part_over_the_size_limit_is_refused() {
        let document = fixture("sample-office.docx");
        let mut archive = package_of(&document, "DOCX").unwrap();
        let error = read_part_limited(&mut archive, "word/document.xml", "DOCX", 16).unwrap_err();
        assert!(
            matches!(error, ConvertError::OfficePartTooLarge { max: 16, .. }),
            "{error}"
        );
    }

    #[test]
    fn office_inputs_get_only_the_text_and_data_targets() {
        for target in [
            OutputFormat::Png,
            OutputFormat::Pdf,
            OutputFormat::Mp3,
            OutputFormat::Yaml,
            OutputFormat::Toml,
        ] {
            let error =
                convert_bytes("report.docx", &fixture("sample-office.docx"), target).unwrap_err();
            assert!(
                matches!(error, ConvertError::Unsupported { .. }),
                "{target:?}: {error}"
            );
        }
    }

    #[test]
    fn sheet_file_components_stay_safe_and_unique() {
        assert_eq!(file_component("Sheet 1"), "Sheet_1");
        assert_eq!(file_component("../etc/passwd"), "etc_passwd");
        assert_eq!(file_component("   "), "sheet");
        assert_eq!(file_component(""), "sheet");

        let mut used = HashSet::new();
        assert_eq!(
            sheet_output_name("book.xlsx", "Data", &mut used),
            PathBuf::from("book_Data.csv")
        );
        assert_eq!(
            sheet_output_name("book.xlsx", "data", &mut used),
            PathBuf::from("book_data_2.csv")
        );
    }

    #[test]
    fn headings_are_recognised_by_style_and_outline_level() {
        assert_eq!(heading_level(&Some("Heading1".to_owned())), Some(1));
        assert_eq!(heading_level(&Some("Heading 3".to_owned())), Some(3));
        assert_eq!(heading_level(&Some("heading_2".to_owned())), Some(2));
        assert_eq!(heading_level(&Some("4".to_owned())), Some(4));
        assert_eq!(heading_level(&Some("Title".to_owned())), None);
        assert_eq!(heading_level(&None), None);
        assert_eq!(heading_level(&Some("Heading12".to_owned())), None);
    }

    #[test]
    fn serial_dates_and_durations_render_as_iso_8601() {
        let date = ExcelDateTime::new(45_360.0, calamine::ExcelDateTimeType::DateTime, false);
        assert_eq!(date_time_text(&date), "2024-03-09");
        let noon = ExcelDateTime::new(45_361.5, calamine::ExcelDateTimeType::DateTime, false);
        assert_eq!(date_time_text(&noon), "2024-03-10T12:00:00");
        let time = ExcelDateTime::new(0.25, calamine::ExcelDateTimeType::DateTime, false);
        assert_eq!(date_time_text(&time), "6:00:00");
        let duration = ExcelDateTime::new(1.5, calamine::ExcelDateTimeType::TimeDelta, false);
        assert_eq!(date_time_text(&duration), "36:00:00");
    }
}
