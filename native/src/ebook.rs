// SPDX-License-Identifier: GPL-2.0-or-later

//! EPUB reading and writing, and the HTML and Markdown text around it.
//!
//! An EPUB is a ZIP whose first entry is an uncompressed `mimetype` holding
//! `application/epub+zip`, followed by an OPF package (manifest and spine) and
//! the XHTML content documents the spine names. Reading is therefore three
//! steps: check the container, walk the spine in reading order, and render each
//! content document as text, Markdown or HTML. Writing is the reverse, and what
//! this module writes is read back and checked before it is returned: the
//! mimetype entry first and stored, a manifest and spine that agree, and a
//! navigation document or an NCX.

use crate::convert::ConvertedFile;
use crate::error::ConvertError;
use crate::format::OutputFormat;
use crate::shared::{is_html_extension, is_markdown_extension, output_name};
use crate::text::{decode_text, encode_text};
use html5ever::{parse_document, tendril::TendrilSink, ParseOpts};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use rbook::epub::{Epub, EpubChapter};
use std::io::{Cursor, Read};
use std::path::Path;
use std::rc::Rc;

/// Entries in an EPUB container. A book needs a handful; this bound is what
/// keeps a hostile container from turning one input into a directory listing.
pub(crate) const MAX_EBOOK_ENTRIES: usize = 10_000;

/// Total *declared* uncompressed size of every entry. The declaration is read
/// from the ZIP central directory before anything is decompressed, so a member
/// that claims to expand past this bound is refused instead of expanding.
pub(crate) const MAX_EBOOK_BYTES: u64 = 512 * 1024 * 1024;

/// The manifest, spine and navigation documents of a well-formed package are a
/// few kilobytes. An entry larger than this is not one of them.
const MAX_PACKAGE_BYTES: u64 = 32 * 1024 * 1024;

/// Total content-document bytes held at once while a book is read. A ZIP entry
/// is inflated up to its declared size, which the container check bounds, and
/// this bounds the sum of what the chapters put in memory.
pub(crate) const MAX_EBOOK_TEXT_BYTES: usize = 64 * 1024 * 1024;

/// `mimetype` must be the first entry, stored, and hold exactly this.
const MIMETYPE_BYTES: &[u8] = b"application/epub+zip";

/// Font obfuscation is declared in `META-INF/encryption.xml` with these
/// algorithms. It hides the font bytes from casual extraction, not the text,
/// so an obfuscated font is not DRM and must not be reported as such.
const FONT_OBFUSCATION_ALGORITHMS: [&str; 2] = [
    "http://www.idpf.org/2008/embedding",
    "http://ns.adobe.com/pdf/enc#RC",
];

/// Whether `target` is one an EPUB or an HTML/Markdown document can become.
pub(crate) fn is_ebook_target(target: OutputFormat) -> bool {
    matches!(
        target,
        OutputFormat::Epub
            | OutputFormat::Html
            | OutputFormat::Markdown
            | OutputFormat::Text
            | OutputFormat::TextUtf8NoBom
            | OutputFormat::TextUtf8Bom
            | OutputFormat::TextUtf16Le
            | OutputFormat::TextUtf16Be
            | OutputFormat::TextUtf32Le
            | OutputFormat::TextUtf32Be
    )
}

/// Whether `bytes` are an EPUB container: the ZIP's first local entry is
/// `mimetype`, stored uncompressed, holding `application/epub+zip`.
///
/// This is the difference between reading the container and trusting the
/// extension: a `.epub` that is really a CBZ, or a `.zip` that is really an
/// EPUB, is recognised by what it says about itself.
pub(crate) fn is_epub_container(bytes: &[u8]) -> bool {
    const SIG: &[u8] = b"PK\x03\x04";
    if bytes.len() < 30 || &bytes[..4] != SIG {
        return false;
    }
    // Local file header: compression method at 8, uncompressed size at 18,
    // name length at 26 and extra length at 28, all little-endian.
    let method = u16::from_le_bytes([bytes[8], bytes[9]]);
    if method != 0 {
        return false;
    }
    let name_len = u16::from_le_bytes([bytes[26], bytes[27]]) as usize;
    let extra_len = u16::from_le_bytes([bytes[28], bytes[29]]) as usize;
    let name_start = 30;
    let name_end = name_start + name_len;
    let content_start = name_end + extra_len;
    let content_end = content_start + MIMETYPE_BYTES.len();
    bytes.len() >= content_end
        && &bytes[name_start..name_end] == b"mimetype"
        && &bytes[content_start..content_end] == MIMETYPE_BYTES
}

/// Whether `bytes` are a PalmDOC container, which is what MOBI and AZW3 are:
/// the Palm database header names `BOOKMOBI` in its type/creator fields, or
/// `TEXtREAd` for a plain PalmDOC.
///
/// The bookmark is read from the container rather than the extension so a
/// Kindle file named `book.epub` is still reported as what it is.
pub(crate) fn is_mobi_container(bytes: &[u8]) -> bool {
    bytes.len() >= 68 && (&bytes[60..68] == b"BOOKMOBI" || &bytes[60..68] == b"TEXtREAd")
}

/// The PalmDOC encryption type: 0 for none, 1 for the old Mobipocket scheme and
/// 2 for Mobipocket DRM. It lives at offset 12 of record 0, which starts after
/// the 78-byte Palm database header.
fn palmdoc_encryption(bytes: &[u8]) -> Option<u16> {
    let offset = 78 + 12;
    bytes
        .get(offset..offset + 2)
        .map(|field| u16::from_be_bytes([field[0], field[1]]))
}

/// Refuses a MOBI or AZW3 input with an error that names what was found, so a
/// user is not told the file is unsupported when the format is recognised.
pub(crate) fn refuse_mobi(input: &[u8]) -> ConvertError {
    match palmdoc_encryption(input) {
        Some(1 | 2) => ConvertError::EbookDrm {
            scheme: "Mobipocket".to_owned(),
        },
        _ => ConvertError::EbookFormatDeferred {
            format: "MOBI and AZW3",
        },
    }
}

/// Converts an EPUB to text, a text encoding, Markdown or a single HTML file.
pub(crate) fn convert_epub(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    let epub = open_epub(input)?;
    let chapters = read_chapters(&epub)?;
    if chapters.is_empty() {
        return Err(ConvertError::EbookEmpty);
    }

    // Each content document is parsed once and its text, Markdown and HTML are
    // all derived from the same tree.
    let rendered: Vec<RenderedChapter> = chapters
        .into_iter()
        .map(|chapter| {
            let document = Rc::new(parse_html(&chapter.xhtml));
            let nodes = body_nodes(&document);
            let text = text_nodes(&nodes);
            RenderedChapter {
                title: chapter.title,
                href: chapter.href,
                _document: document,
                nodes,
                text,
            }
        })
        .collect();
    if rendered
        .iter()
        .all(|chapter| chapter.text.trim().is_empty())
    {
        return Err(ConvertError::EbookEmpty);
    }

    let bytes = match target {
        OutputFormat::Html => {
            let title = epub_title(&epub).unwrap_or_else(|| title_from_name(input_name));
            let language = epub_language(&epub);
            let chapters: Vec<HtmlChapter> = rendered
                .iter()
                .map(|chapter| HtmlChapter {
                    title: chapter.title.clone(),
                    body: xhtml_fragment(
                        &chapter.nodes,
                        Some(&image_resolver(&epub, &chapter.href)),
                    ),
                })
                .collect();
            html_document(&title, &language, &chapters).into_bytes()
        }
        OutputFormat::Markdown => {
            let text = rendered_markdown(&rendered);
            if text.trim().is_empty() {
                return Err(ConvertError::EbookEmpty);
            }
            text.into_bytes()
        }
        OutputFormat::Text
        | OutputFormat::TextUtf8NoBom
        | OutputFormat::TextUtf8Bom
        | OutputFormat::TextUtf16Le
        | OutputFormat::TextUtf16Be
        | OutputFormat::TextUtf32Le
        | OutputFormat::TextUtf32Be => {
            let text = rendered_text(&rendered);
            if text.trim().is_empty() {
                return Err(ConvertError::EbookEmpty);
            }
            encode_text(&text, target)?
        }
        _ => {
            return Err(ConvertError::Unsupported {
                input_extension: "epub".to_owned(),
                target: target.label(),
            });
        }
    };

    Ok(vec![ConvertedFile {
        name: output_name(input_name, target.extension()),
        bytes,
    }])
}

/// Converts an HTML or Markdown document to text, a text encoding, or to the
/// other text markup. Both are stored documents rather than containers, so
/// nothing here needs the reading order an EPUB's spine supplies.
pub(crate) fn convert_document(
    input_name: &str,
    input: &[u8],
    extension: &str,
    target: OutputFormat,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    let markup = if is_markdown_extension(extension) {
        markdown_to_html(&decode_text(input)?)
    } else {
        decode_text(input)?
    };
    let dom = parse_html(&markup);
    let title = document_title(&dom).unwrap_or_else(|| title_from_name(input_name));
    let nodes = body_nodes(&dom);
    let document = Rc::new(dom);
    let chapters: Vec<RenderedChapter> = split_chapters(&nodes, &title)
        .into_iter()
        .map(|chapter| RenderedChapter {
            title: chapter.title,
            href: String::new(),
            _document: Rc::clone(&document),
            text: text_nodes(&chapter.nodes),
            nodes: chapter.nodes,
        })
        .collect();

    let bytes = match target {
        OutputFormat::Html => {
            let chapters: Vec<HtmlChapter> = chapters
                .iter()
                .map(|chapter| HtmlChapter {
                    title: chapter.title.clone(),
                    body: xhtml_fragment(&chapter.nodes, None),
                })
                .collect();
            html_document(&title, "und", &chapters).into_bytes()
        }
        OutputFormat::Markdown => {
            let text = rendered_markdown(&chapters);
            if text.trim().is_empty() {
                return Err(ConvertError::EbookEmpty);
            }
            text.into_bytes()
        }
        OutputFormat::Text
        | OutputFormat::TextUtf8NoBom
        | OutputFormat::TextUtf8Bom
        | OutputFormat::TextUtf16Le
        | OutputFormat::TextUtf16Be
        | OutputFormat::TextUtf32Le
        | OutputFormat::TextUtf32Be => {
            let text = rendered_text(&chapters);
            if text.trim().is_empty() {
                return Err(ConvertError::EbookEmpty);
            }
            encode_text(&text, target)?
        }
        _ => {
            return Err(ConvertError::Unsupported {
                input_extension: extension.to_owned(),
                target: target.label(),
            });
        }
    };

    Ok(vec![ConvertedFile {
        name: output_name(input_name, target.extension()),
        bytes,
    }])
}

/// Builds an EPUB from plain text, Markdown or HTML.
///
/// Text becomes one chapter of paragraphs; Markdown is rendered through the
/// CommonMark parser and HTML is read as it is, and both are split into chapters
/// at their top-level `h1` headings. Nothing is rendered to a page and no
/// external file is read: the book holds what the input holds.
pub(crate) fn convert_to_epub(
    input_name: &str,
    input: &[u8],
    extension: &str,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    let mut title = title_from_name(input_name);
    let chapters = if extension == "txt" {
        text_chapters(&decode_text(input)?, &title)
    } else if is_markdown_extension(extension) {
        let dom = parse_html(&markdown_to_html(&decode_text(input)?));
        title = document_title(&dom).unwrap_or(title);
        chapters_from_nodes(&body_nodes(&dom), &title)
    } else if is_html_extension(extension) {
        let dom = parse_html(&String::from_utf8_lossy(input));
        title = document_title(&dom).unwrap_or(title);
        chapters_from_nodes(&body_nodes(&dom), &title)
    } else {
        return Err(crate::shared::unsupported(extension, OutputFormat::Epub));
    };

    let chapters: Vec<EpubChapterData> = chapters
        .into_iter()
        .filter(|chapter| !chapter.body.trim().is_empty())
        .collect();
    if chapters.is_empty() {
        return Err(ConvertError::EbookEmpty);
    }

    let mut builder = Epub::builder()
        .identifier(identifier_for(input))
        .title(title.clone())
        .language("und");
    for chapter in &chapters {
        builder = builder.chapter(
            EpubChapter::new(chapter.title.clone()).xhtml_body(chapter.body.clone().into_bytes()),
        );
    }
    let epub = builder.build();
    let bytes = epub
        .write()
        .compression(6)
        .to_vec()
        .map_err(|error| ConvertError::EbookWrite(error.to_string()))?;
    validate_written_epub(&bytes, chapters.len())?;

    Ok(vec![ConvertedFile {
        name: output_name(input_name, OutputFormat::Epub.extension()),
        bytes,
    }])
}

/// One content document in reading order.
struct Chapter {
    title: String,
    /// The href the document was read from, used to resolve its relative image
    /// references.
    href: String,
    xhtml: String,
}

/// One content document parsed once, with the three renderings of it all taken
/// from the same tree.
struct RenderedChapter {
    title: String,
    /// The container href the document was read from, empty for a document that
    /// was not read out of a container.
    href: String,
    /// Held only to keep the tree alive: `rcdom`'s text nodes alias the parser's
    /// buffer queue, which is freed with the `RcDom`, so a handle kept on its
    /// own reads back as empty text.
    _document: Rc<RcDom>,
    nodes: Vec<Handle>,
    text: String,
}

/// One chapter as it will be written.
struct EpubChapterData {
    title: String,
    body: String,
}

/// One chapter of an HTML output document.
struct HtmlChapter {
    title: String,
    body: String,
}

/// One chapter split out of an HTML or Markdown document.
struct DocumentChapter {
    title: String,
    nodes: Vec<Handle>,
}

fn open_epub(input: &[u8]) -> Result<Epub, ConvertError> {
    check_epub_container(input)?;
    Epub::read(Cursor::new(input.to_vec())).map_err(|error| ConvertError::EbookCorrupt {
        message: error.to_string(),
    })
}

/// Refuses a container that is DRM-protected, too large, or holds too many
/// entries, before any parser gets to decompress it.
fn check_epub_container(input: &[u8]) -> Result<(), ConvertError> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(input)).map_err(|error| ConvertError::EbookCorrupt {
            message: error.to_string(),
        })?;
    let mut declared = 0_u64;
    for index in 0..archive.len() {
        let entry = archive
            .by_index_raw(index)
            .map_err(|error| ConvertError::EbookCorrupt {
                message: error.to_string(),
            })?;
        declared = declared.saturating_add(entry.size());
        let name = entry.name().to_owned();
        drop(entry);
        let lower = name.to_ascii_lowercase();
        if lower == "meta-inf/rights.xml" || lower == "meta-inf/sinf.xml" {
            return Err(ConvertError::EbookDrm {
                scheme: "Adobe ADEPT".to_owned(),
            });
        }
        if lower == "meta-inf/encryption.xml" {
            let mut text = String::new();
            archive
                .by_name(&name)
                .map_err(|error| ConvertError::EbookCorrupt {
                    message: error.to_string(),
                })?
                .take(MAX_PACKAGE_BYTES + 1)
                .read_to_string(&mut text)?;
            if let Some(scheme) = encryption_scheme(&text) {
                return Err(ConvertError::EbookDrm { scheme });
            }
        }
        check_ebook_limits(archive.len(), declared)?;
    }
    check_ebook_limits(archive.len(), declared)
}

/// The bound every EPUB read goes through, in one place so both the scan and
/// its test use the same rule.
fn check_ebook_limits(entries: usize, declared_bytes: u64) -> Result<(), ConvertError> {
    if entries > MAX_EBOOK_ENTRIES {
        return Err(ConvertError::EbookTooManyEntries {
            count: entries,
            max: MAX_EBOOK_ENTRIES,
        });
    }
    if declared_bytes > MAX_EBOOK_BYTES {
        return Err(ConvertError::EbookTooLarge {
            max: MAX_EBOOK_BYTES,
        });
    }
    Ok(())
}

/// The DRM scheme an `encryption.xml` declares, or `None` when every algorithm
/// it names is font obfuscation.
///
/// Adobe ADEPT and Kindle/Kobo schemes name a content cipher such as
/// `aes128-cbc` and point at the encrypted key in `rights.xml`; the IDPF and
/// Adobe font algorithms hide only the embedded fonts. Anything that is neither
/// known algorithm is treated as DRM: refusing a book that might be copyable is
/// the safe direction, because decrypting is not something this app does.
fn encryption_scheme(encryption_xml: &str) -> Option<String> {
    for part in encryption_xml.split("Algorithm").skip(1) {
        let Some(value) = part.split('"').nth(1) else {
            continue;
        };
        if FONT_OBFUSCATION_ALGORITHMS.contains(&value) {
            continue;
        }
        return Some(scheme_name(value));
    }
    None
}

fn scheme_name(algorithm: &str) -> String {
    if algorithm.contains("kip") {
        "Kobo".to_owned()
    } else if algorithm.contains("aes") {
        "Adobe ADEPT".to_owned()
    } else {
        "an unknown content cipher".to_owned()
    }
}

fn read_chapters(epub: &Epub) -> Result<Vec<Chapter>, ConvertError> {
    let labels = toc_labels(epub);
    let mut chapters = Vec::new();
    let mut total_bytes = 0_usize;
    let mut last_error = None;
    for content in epub.reader_builder().skip_unresolved(true).create() {
        let content = match content {
            Ok(content) => content,
            Err(error) => {
                last_error = Some(error.to_string());
                continue;
            }
        };
        let entry = content.manifest_entry();
        let href = normalized_href(entry.href().as_ref());
        let xhtml = content.content().to_owned();
        total_bytes = total_bytes.saturating_add(xhtml.len());
        if total_bytes > MAX_EBOOK_TEXT_BYTES {
            return Err(ConvertError::EbookTooLarge {
                max: MAX_EBOOK_TEXT_BYTES as u64,
            });
        }
        let title = labels
            .get(&href)
            .filter(|label| !label.trim().is_empty())
            .cloned()
            .or_else(|| first_heading(&parse_html(&xhtml)))
            .unwrap_or_else(|| format!("Chapter {}", chapters.len() + 1));
        chapters.push(Chapter { title, href, xhtml });
    }
    if let (true, Some(message)) = (chapters.is_empty(), last_error) {
        return Err(ConvertError::EbookCorrupt { message });
    }
    Ok(chapters)
}

/// Maps a content document's href to the label the book's navigation document
/// gives it, so a chapter keeps the title the book declared for it.
fn toc_labels(epub: &Epub) -> std::collections::HashMap<String, String> {
    let mut labels = std::collections::HashMap::new();
    for root in epub.toc().iter() {
        if let Some(href) = root.href() {
            labels.insert(normalized_href(href.as_ref()), root.label().to_owned());
        }
        for entry in root.flatten() {
            if let Some(href) = entry.href() {
                labels.insert(normalized_href(href.as_ref()), entry.label().to_owned());
            }
        }
    }
    labels
}

/// An href without its container-absolute slash, percent-encoding or fragment,
/// so a spine href and a navigation href can be compared.
fn normalized_href(href: &str) -> String {
    let href = href.trim_start_matches('/');
    let href = href.split('#').next().unwrap_or(href);
    percent_decode(href)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                decoded.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn epub_title(epub: &Epub) -> Option<String> {
    epub.metadata()
        .title()
        .map(|title| title.value().trim().to_owned())
        .filter(|title| !title.is_empty())
}

fn epub_language(epub: &Epub) -> String {
    epub.metadata()
        .language()
        .map(|language| language.value().trim().to_owned())
        .filter(|language| !language.is_empty())
        .unwrap_or_else(|| "und".to_owned())
}

/// Joins the chapters as plain text, one blank line between them.
fn rendered_text(chapters: &[RenderedChapter]) -> String {
    let mut text = String::new();
    for chapter in chapters {
        if chapter.text.trim().is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        if chapter_needs_title(chapters, chapter) {
            text.push_str(chapter.title.trim());
            text.push_str("\n\n");
        }
        text.push_str(&chapter.text);
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

/// Joins the chapters as CommonMark, one blank line between them.
fn rendered_markdown(chapters: &[RenderedChapter]) -> String {
    let mut text = String::new();
    for chapter in chapters {
        let markdown = markdown_nodes(&chapter.nodes, 0);
        let markdown = markdown.trim();
        if markdown.is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        if chapter_needs_title(chapters, chapter) {
            text.push_str("# ");
            text.push_str(&escape_markdown_heading(&chapter.title));
            text.push_str("\n\n");
        }
        text.push_str(markdown);
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

/// A chapter title is written above the text only when the book has more than
/// one chapter and that chapter's own content carries no heading. A one-chapter
/// book is a document whose text is the whole content, so a generated title
/// would be an invented line rather than the book's.
fn chapter_needs_title(chapters: &[RenderedChapter], chapter: &RenderedChapter) -> bool {
    chapters.len() > 1 && heading_text(&chapter.nodes).is_none() && !chapter.title.trim().is_empty()
}

fn title_from_name(input_name: &str) -> String {
    Path::new(input_name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().trim().to_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "Converted book".to_owned())
}

/// A deterministic identifier, so converting the same bytes twice writes the
/// same package. EPUB requires a unique-identifier and a URN is its usual form.
fn identifier_for(input: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in input {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("urn:convert2it:{hash:016x}")
}

fn text_chapters(text: &str, title: &str) -> Vec<EpubChapterData> {
    let mut body = String::new();
    for block in text.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        body.push_str("<p>");
        body.push_str(&escape_xml(block));
        body.push_str("</p>\n");
    }
    if body.is_empty() {
        return Vec::new();
    }
    vec![EpubChapterData {
        title: title.to_owned(),
        body,
    }]
}

fn chapters_from_nodes(nodes: &[Handle], fallback: &str) -> Vec<EpubChapterData> {
    let split = split_chapters(nodes, fallback);
    split
        .into_iter()
        .map(|chapter| EpubChapterData {
            title: chapter.title,
            body: xhtml_fragment(&chapter.nodes, None),
        })
        .collect()
}

/// Splits a document's top-level nodes into chapters at its `h1` headings.
/// Content before the first heading stays a chapter of its own, and a document
/// without a heading is one chapter, which is the shape a stored document has
/// rather than a book.
fn split_chapters(nodes: &[Handle], fallback: &str) -> Vec<DocumentChapter> {
    let headings: Vec<usize> = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| element_name(node).as_deref() == Some("h1"))
        .map(|(index, _)| index)
        .collect();

    if headings.len() <= 1 {
        return vec![DocumentChapter {
            title: heading_text(nodes).unwrap_or_else(|| fallback.to_owned()),
            nodes: nodes.to_vec(),
        }];
    }

    let mut chapters = Vec::new();
    if headings[0] > 0 {
        chapters.push(DocumentChapter {
            title: fallback.to_owned(),
            nodes: nodes[..headings[0]].to_vec(),
        });
    }
    for (position, start) in headings.iter().enumerate() {
        let end = headings.get(position + 1).copied().unwrap_or(nodes.len());
        let title = nodes[*start]
            .children
            .borrow()
            .iter()
            .map(node_text)
            .collect::<Vec<_>>()
            .join("")
            .trim()
            .to_owned();
        chapters.push(DocumentChapter {
            title: if title.is_empty() {
                fallback.to_owned()
            } else {
                title
            },
            nodes: nodes[*start..end].to_vec(),
        });
    }
    chapters
}

fn heading_text(nodes: &[Handle]) -> Option<String> {
    for heading in ["h1", "h2", "h3"] {
        for node in nodes {
            if element_name(node).as_deref() == Some(heading) {
                let text = node_text(node).trim().to_owned();
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

fn document_title(dom: &RcDom) -> Option<String> {
    let html = dom
        .document
        .children
        .borrow()
        .iter()
        .find(|node| element_name(node).as_deref() == Some("html"))
        .cloned()?;
    let head = html
        .children
        .borrow()
        .iter()
        .find(|node| element_name(node).as_deref() == Some("head"))
        .cloned()?;
    for child in head.children.borrow().iter() {
        if element_name(child).as_deref() == Some("title") {
            let title = raw_text(child).trim().to_owned();
            if !title.is_empty() {
                return Some(title);
            }
        }
    }
    None
}

fn first_heading(dom: &RcDom) -> Option<String> {
    heading_text(&body_nodes(dom))
}

fn parse_html(input: &str) -> RcDom {
    parse_document(RcDom::default(), ParseOpts::default()).one(input)
}

/// The body's child nodes, or the document's when there is no body. `body` is
/// the reading content of an EPUB content document; `head` holds its title and
/// stylesheets, neither of which is text.
fn body_nodes(dom: &RcDom) -> Vec<Handle> {
    let document_children = dom.document.children.borrow();
    let html = document_children
        .iter()
        .find(|node| element_name(node).as_deref() == Some("html"));
    let Some(html) = html else {
        return document_children.clone();
    };
    let html_children = html.children.borrow();
    let body = html_children
        .iter()
        .find(|node| element_name(node).as_deref() == Some("body"));
    match body {
        Some(body) => body.children.borrow().clone(),
        None => html_children.clone(),
    }
}

fn element_name(node: &Handle) -> Option<String> {
    match &node.data {
        NodeData::Element { name, .. } => Some(name.local.to_string().to_ascii_lowercase()),
        _ => None,
    }
}

/// The text of a node without any markup, with `br` as a line break and inline
/// runs collapsed the way a rendered paragraph collapses them.
fn node_text(node: &Handle) -> String {
    let mut sink = TextSink::default();
    sink.nodes(std::slice::from_ref(node));
    sink.finish()
}

/// Elements whose content is not reading text.
const SKIPPED_ELEMENTS: &[&str] = &[
    "script", "style", "head", "title", "template", "noscript", "svg", "canvas", "iframe",
    "object", "embed", "audio", "video", "source", "track", "link", "meta", "base", "input",
    "button", "select", "textarea", "form", "map", "area", "param",
];

/// Elements that start a new block of text.
const BLOCK_ELEMENTS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "caption",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "tr",
    "ul",
];

#[derive(Default)]
struct TextSink {
    out: String,
    pending_space: bool,
    preserve: usize,
}

impl TextSink {
    fn finish(mut self) -> String {
        while self
            .out
            .ends_with(|character: char| character.is_whitespace())
        {
            self.out.pop();
        }
        self.out
    }

    fn nodes(&mut self, nodes: &[Handle]) {
        for node in nodes {
            self.node(node);
        }
    }

    fn node(&mut self, node: &Handle) {
        match &node.data {
            NodeData::Text { contents } => {
                let text = contents.borrow();
                if self.preserve > 0 {
                    self.out.push_str(&text);
                } else {
                    self.collapsed(&text);
                }
            }
            NodeData::Element { name, .. } => {
                let name = name.local.to_string().to_ascii_lowercase();
                if SKIPPED_ELEMENTS.contains(&name.as_str()) {
                    return;
                }
                if self.preserve > 0 {
                    if name == "br" {
                        self.out.push('\n');
                    } else {
                        self.nodes(&node.children.borrow());
                    }
                    return;
                }
                match name.as_str() {
                    "br" => {
                        self.pending_space = false;
                        self.out.push('\n');
                    }
                    "td" | "th" => {
                        if !self.out.is_empty() && !self.out.ends_with(['\n', ' ']) {
                            self.out.push_str(" | ");
                        }
                        self.nodes(&node.children.borrow());
                    }
                    "tr" => {
                        if !self.out.is_empty() && !self.out.ends_with('\n') {
                            self.out.push('\n');
                        }
                        self.nodes(&node.children.borrow());
                        self.out.push('\n');
                    }
                    "pre" => {
                        self.block_boundary();
                        self.preserve += 1;
                        self.nodes(&node.children.borrow());
                        self.preserve -= 1;
                        self.block_boundary();
                    }
                    "li" => {
                        self.newline_if_needed();
                        self.out.push_str("- ");
                        self.nodes(&node.children.borrow());
                    }
                    "ul" | "ol" => {
                        self.block_boundary();
                        self.nodes(&node.children.borrow());
                        self.block_boundary();
                    }
                    _ if BLOCK_ELEMENTS.contains(&name.as_str()) => {
                        self.block_boundary();
                        self.nodes(&node.children.borrow());
                        self.block_boundary();
                    }
                    _ => self.nodes(&node.children.borrow()),
                }
            }
            _ => {}
        }
    }

    fn collapsed(&mut self, text: &str) {
        for character in text.chars() {
            if character.is_whitespace() {
                self.pending_space = true;
                continue;
            }
            self.flush_space();
            self.out.push(character);
        }
        if text.ends_with(char::is_whitespace) {
            self.pending_space = true;
        }
    }

    fn flush_space(&mut self) {
        if self.pending_space {
            if !self.out.is_empty() && !self.out.ends_with(['\n', ' ']) {
                self.out.push(' ');
            }
            self.pending_space = false;
        }
    }

    fn newline_if_needed(&mut self) {
        self.pending_space = false;
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    fn block_boundary(&mut self) {
        self.pending_space = false;
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        if self.out.is_empty() {
            return;
        }
        let newlines = self.out.len() - self.out.trim_end_matches('\n').len();
        match newlines {
            0 => self.out.push_str("\n\n"),
            1 => self.out.push('\n'),
            _ => {}
        }
    }
}

/// Renders a document's nodes as plain text in reading order.
fn text_nodes(nodes: &[Handle]) -> String {
    let mut sink = TextSink::default();
    sink.nodes(nodes);
    sink.finish()
}

fn markdown_nodes(nodes: &[Handle], indent: usize) -> String {
    let mut out = String::new();
    markdown_into(nodes, &mut out, indent);
    out
}

fn markdown_into(nodes: &[Handle], out: &mut String, indent: usize) {
    for node in nodes {
        markdown_node(node, out, indent);
    }
}

fn markdown_node(node: &Handle, out: &mut String, indent: usize) {
    match &node.data {
        NodeData::Text { contents } => {
            let text = collapse_spaces(&contents.borrow());
            out.push_str(&text);
        }
        NodeData::Element { name, .. } => {
            let name = name.local.to_string().to_ascii_lowercase();
            if SKIPPED_ELEMENTS.contains(&name.as_str()) {
                return;
            }
            match name.as_str() {
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    let level = name[1..].parse::<usize>().unwrap_or(1);
                    block_break(out);
                    out.push_str(&"#".repeat(level));
                    out.push(' ');
                    markdown_into(&node.children.borrow(), out, indent);
                    out.push_str("\n\n");
                }
                "p" => {
                    block_break(out);
                    markdown_into(&node.children.borrow(), out, indent);
                    out.push_str("\n\n");
                }
                "br" => out.push_str("  \n"),
                "hr" => {
                    block_break(out);
                    out.push_str("---\n\n");
                }
                "pre" => {
                    block_break(out);
                    let code = raw_text(node);
                    out.push_str("```\n");
                    out.push_str(&code);
                    if !code.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("```\n\n");
                }
                "blockquote" => {
                    block_break(out);
                    let mut inner = String::new();
                    markdown_into(&node.children.borrow(), &mut inner, indent);
                    for line in inner.trim().lines() {
                        out.push_str("> ");
                        out.push_str(line);
                        out.push('\n');
                    }
                    out.push('\n');
                }
                "strong" | "b" => {
                    out.push_str("**");
                    markdown_into(&node.children.borrow(), out, indent);
                    out.push_str("**");
                }
                "em" | "i" => {
                    out.push('*');
                    markdown_into(&node.children.borrow(), out, indent);
                    out.push('*');
                }
                "code" | "kbd" | "samp" => {
                    out.push('`');
                    out.push_str(&node_text(node));
                    out.push('`');
                }
                "a" => {
                    let href = attribute(node, "href");
                    match href {
                        Some(href) if !href.trim().is_empty() => {
                            out.push('[');
                            markdown_into(&node.children.borrow(), out, indent);
                            out.push_str("](");
                            out.push_str(href.trim());
                            out.push(')');
                        }
                        _ => markdown_into(&node.children.borrow(), out, indent),
                    }
                }
                "ul" | "ol" => {
                    // A list inside a list item belongs to that item, so it
                    // continues the parent list instead of breaking it apart.
                    let nested = indent > 0;
                    if nested {
                        if !out.ends_with('\n') {
                            out.push('\n');
                        }
                    } else {
                        block_break(out);
                    }
                    let ordered = name == "ol";
                    let mut index = 0;
                    for item in node.children.borrow().iter() {
                        if element_name(item).as_deref() != Some("li") {
                            markdown_node(item, out, indent + 4);
                            continue;
                        }
                        index += 1;
                        out.push_str(&" ".repeat(indent));
                        if ordered {
                            out.push_str(&format!("{index}. "));
                        } else {
                            out.push_str("- ");
                        }
                        markdown_into(&item.children.borrow(), out, indent + 4);
                        out.push('\n');
                    }
                    if !nested {
                        out.push('\n');
                    }
                }
                "li" => {
                    out.push_str("- ");
                    markdown_into(&node.children.borrow(), out, indent);
                }
                "table" => {
                    block_break(out);
                    markdown_table(node, out);
                    out.push('\n');
                }
                _ if BLOCK_ELEMENTS.contains(&name.as_str()) => {
                    block_break(out);
                    markdown_into(&node.children.borrow(), out, indent);
                    block_break(out);
                }
                _ => markdown_into(&node.children.borrow(), out, indent),
            }
        }
        _ => {}
    }
}

fn markdown_table(table: &Handle, out: &mut String) {
    let mut rows = 0;
    for row in table.children.borrow().iter() {
        let cells: Vec<Handle> = row
            .children
            .borrow()
            .iter()
            .filter(|cell| matches!(element_name(cell).as_deref(), Some("td" | "th")))
            .cloned()
            .collect();
        if cells.is_empty() {
            continue;
        }
        out.push('|');
        for cell in &cells {
            out.push(' ');
            out.push_str(node_text(cell).trim());
            out.push_str(" |");
        }
        out.push('\n');
        if rows == 0 {
            out.push('|');
            for _ in &cells {
                out.push_str(" --- |");
            }
            out.push('\n');
        }
        rows += 1;
    }
}

fn block_break(out: &mut String) {
    if out.is_empty() {
        return;
    }
    let newlines = out.len() - out.trim_end_matches('\n').len();
    if newlines == 0 {
        out.push_str("\n\n");
    } else if newlines == 1 {
        out.push('\n');
    }
}

fn collapse_spaces(text: &str) -> String {
    let mut collapsed = String::with_capacity(text.len());
    let mut pending = false;
    for character in text.chars() {
        if character.is_whitespace() {
            pending = true;
            continue;
        }
        if pending && !collapsed.is_empty() {
            collapsed.push(' ');
        }
        pending = false;
        collapsed.push(character);
    }
    if text.ends_with(char::is_whitespace) && !collapsed.is_empty() {
        collapsed.push(' ');
    }
    collapsed
}

fn raw_text(node: &Handle) -> String {
    let mut text = String::new();
    for child in node.children.borrow().iter() {
        match &child.data {
            NodeData::Text { contents } => text.push_str(&contents.borrow()),
            _ => text.push_str(&raw_text(child)),
        }
    }
    text
}

fn attribute(node: &Handle, name: &str) -> Option<String> {
    match &node.data {
        NodeData::Element { attrs, .. } => attrs
            .borrow()
            .iter()
            .find(|attribute| attribute.name.local.to_string() == name)
            .map(|attribute| attribute.value.to_string()),
        _ => None,
    }
}

/// Resolves an image reference against the document it appears in. Returning
/// `None` keeps the reference as written.
type ImageResolver<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Serialises nodes as a well-formed XHTML fragment.
///
/// HTML as browsers accept it is not XHTML: tags may be unclosed, attributes
/// unquoted and entities bare. This writes the tree back out with every element
/// closed, every attribute quoted and every text run escaped, which is what an
/// EPUB content document requires. Elements that would need a resource the book
/// does not carry — scripts, stylesheets, media — are dropped.
fn xhtml_fragment(nodes: &[Handle], images: Option<ImageResolver<'_>>) -> String {
    let mut out = String::new();
    for node in nodes {
        write_xhtml(node, &mut out, images);
    }
    out
}

fn write_xhtml(node: &Handle, out: &mut String, images: Option<ImageResolver<'_>>) {
    match &node.data {
        NodeData::Text { contents } => escape_xml_into(&contents.borrow(), out),
        NodeData::Element { name, .. } => {
            let name = name.local.to_string().to_ascii_lowercase();
            if SKIPPED_ELEMENTS.contains(&name.as_str()) || (name == "img" && images.is_none()) {
                return;
            }
            out.push('<');
            out.push_str(&name);
            if let NodeData::Element { attrs, .. } = &node.data {
                for attribute in attrs.borrow().iter() {
                    let local = attribute.name.local.to_string().to_ascii_lowercase();
                    if local.starts_with("on") || local == "xmlns" {
                        continue;
                    }
                    let mut value = attribute.value.to_string();
                    if local == "src" && name == "img" {
                        if let Some(resolve) = images {
                            if let Some(resolved) = resolve(&value) {
                                value = resolved;
                            }
                        }
                    }
                    out.push(' ');
                    out.push_str(&local);
                    out.push_str("=\"");
                    escape_xml_attribute_into(&value, out);
                    out.push('"');
                }
            }
            if is_void(&name) {
                out.push_str("/>");
                return;
            }
            out.push('>');
            for child in node.children.borrow().iter() {
                write_xhtml(child, out, images);
            }
            out.push_str("</");
            out.push_str(&name);
            out.push('>');
        }
        _ => {}
    }
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn escape_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    escape_xml_into(text, &mut out);
    out
}

fn escape_xml_into(text: &str, out: &mut String) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' | '\n' | '\r' => out.push(character),
            control if control.is_control() => {}
            other => out.push(other),
        }
    }
}

fn escape_xml_attribute_into(text: &str, out: &mut String) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            control if control.is_control() => {}
            other => out.push(other),
        }
    }
}

fn escape_markdown_heading(text: &str) -> String {
    text.replace(['\n', '\r', '#'], " ").trim().to_owned()
}

/// Renders CommonMark as HTML, which is then read back as a tree. Raw HTML in
/// the source is escaped rather than passed through: an EPUB content document
/// must be well-formed, and the tree walk is what makes it so.
fn markdown_to_html(markdown: &str) -> String {
    let options = pulldown_cmark::Options::ENABLE_TABLES
        | pulldown_cmark::Options::ENABLE_FOOTNOTES
        | pulldown_cmark::Options::ENABLE_STRIKETHROUGH
        | pulldown_cmark::Options::ENABLE_TASKLISTS;
    let parser = pulldown_cmark::Parser::new_ext(markdown, options);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    html
}

fn html_document(title: &str, language: &str, chapters: &[HtmlChapter]) -> String {
    let mut html = String::new();
    html.push_str("<!DOCTYPE html>\n<html lang=\"");
    escape_xml_attribute_into(language, &mut html);
    html.push_str("\">\n<head>\n<meta charset=\"utf-8\">\n<title>");
    escape_xml_into(title, &mut html);
    html.push_str(
        "</title>\n<style>\nbody{font-family:system-ui,sans-serif;max-width:800px;margin:2rem auto;padding:0 2rem;line-height:1.6;color:#1a1a1a}\nimg{max-width:100%;height:auto}\n</style>\n</head>\n<body>\n",
    );
    for (index, chapter) in chapters.iter().enumerate() {
        html.push_str("<section class=\"epub-chapter\" id=\"chapter-");
        html.push_str(&(index + 1).to_string());
        html.push_str("\" data-title=\"");
        escape_xml_attribute_into(&chapter.title, &mut html);
        html.push_str("\">\n");
        html.push_str(&chapter.body);
        html.push_str("\n</section>\n");
    }
    html.push_str("</body>\n</html>\n");
    html
}

/// Resolves an image reference against the chapter it appears in and returns a
/// data URL, which keeps the HTML file self-contained when the book's resources
/// are not extracted beside it.
fn image_resolver<'a>(
    epub: &'a Epub,
    chapter_href: &'a str,
) -> impl Fn(&str) -> Option<String> + 'a {
    let directory = chapter_href
        .rsplit_once('/')
        .map(|(directory, _)| directory)
        .unwrap_or("");
    let directory = directory.to_owned();
    move |src: &str| {
        let src = src.trim();
        if src.is_empty()
            || src.starts_with("data:")
            || src.starts_with("http:")
            || src.starts_with("https:")
        {
            return None;
        }
        let resolved = resolve_href(&directory, src);
        let entry = epub.manifest().by_href(&format!("/{resolved}"))?;
        let media_type = entry.media_type().to_owned();
        let bytes = entry.read_bytes().ok()?;
        Some(format!("data:{media_type};base64,{}", base64(&bytes)))
    }
}

/// Joins a relative href onto a container directory, resolving `.` and `..`
/// without escaping the container root.
fn resolve_href(directory: &str, href: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !href.starts_with('/') {
        parts.extend(directory.split('/').filter(|part| !part.is_empty()));
    }
    for part in href.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0b11) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(ALPHABET[(((second & 0b1111) << 2) | (third >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(ALPHABET[(third & 0b111111) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

/// Reads back what was written and checks the parts a reader checks: the
/// mimetype entry first and stored, a spine whose entries resolve, and a
/// navigation document or an NCX.
fn validate_written_epub(bytes: &[u8], expected_chapters: usize) -> Result<(), ConvertError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| {
        ConvertError::EbookWrite(format!("the written container is not a ZIP: {error}"))
    })?;
    let first = archive
        .by_index(0)
        .map_err(|error| ConvertError::EbookWrite(format!("no first entry: {error}")))?;
    let first_name = first.name().to_owned();
    let stored = first.compression() == zip::CompressionMethod::Stored;
    drop(first);
    if first_name != "mimetype" || !stored {
        return Err(ConvertError::EbookWrite(
            "the mimetype entry is not the first, stored entry".to_owned(),
        ));
    }
    let mut mimetype = String::new();
    archive
        .by_name("mimetype")
        .map_err(|error| ConvertError::EbookWrite(error.to_string()))?
        .read_to_string(&mut mimetype)?;
    if mimetype != "application/epub+zip" {
        return Err(ConvertError::EbookWrite(
            "the mimetype entry does not say application/epub+zip".to_owned(),
        ));
    }

    let mut package = None;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| ConvertError::EbookWrite(error.to_string()))?;
        if entry.name().ends_with(".opf") {
            let mut text = String::new();
            entry
                .by_ref()
                .take(MAX_PACKAGE_BYTES)
                .read_to_string(&mut text)?;
            package = Some(text);
            break;
        }
    }
    let Some(package) = package else {
        return Err(ConvertError::EbookWrite(
            "the written EPUB has no package document".to_owned(),
        ));
    };
    if !package.contains("media-type=\"application/x-dtbncx+xml\"")
        && !package.contains("properties=\"nav\"")
    {
        return Err(ConvertError::EbookWrite(
            "the written EPUB has neither an NCX nor a navigation document".to_owned(),
        ));
    }

    let epub = Epub::read(Cursor::new(bytes.to_vec())).map_err(|error| {
        ConvertError::EbookWrite(format!("the written EPUB does not parse: {error}"))
    })?;
    let spine = epub.spine();
    if spine.len() != expected_chapters {
        return Err(ConvertError::EbookWrite(format!(
            "the written EPUB spine has {} entries for {expected_chapters} chapters",
            spine.len()
        )));
    }
    for entry in spine.iter() {
        if entry.manifest_entry().is_none() {
            return Err(ConvertError::EbookWrite(format!(
                "spine entry {:?} does not resolve to a manifest item",
                entry.idref()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::convert_bytes;
    use crate::test_support::{
        epub3_fixture, epub_fixture, epub_with_encryption, epub_with_image, zip_with_entries,
    };

    fn epub_text(bytes: &[u8]) -> String {
        let converted = convert_bytes("book.epub", bytes, OutputFormat::Text).unwrap();
        String::from_utf8(converted[0].bytes.clone()).unwrap()
    }

    #[test]
    fn recognizes_an_epub_container_and_not_a_plain_zip() {
        let epub = epub3_fixture();
        assert!(is_epub_container(&epub));
        let plain = zip_with_entries(&[("mimetype".to_owned(), b"not an epub".to_vec())]);
        assert!(!is_epub_container(&plain));
        assert!(!is_epub_container(b"PK\x05\x06"));
        assert!(!is_epub_container(&epub3_fixture_without_first_entry()));
    }

    fn epub3_fixture_without_first_entry() -> Vec<u8> {
        zip_with_entries(&[("content.txt".to_owned(), b"hello".to_vec())])
    }

    #[test]
    fn reads_epub2_and_epub3_in_spine_order() {
        for bytes in [epub_fixture(), epub3_fixture()] {
            let text = epub_text(&bytes);
            let first = text.find("A quiet beginning").expect("first chapter");
            let second = text.find("The second chapter").expect("second chapter");
            assert!(first < second, "spine order was not preserved: {text}");
            assert!(text.contains("A quiet beginning."), "{text}");
        }
    }

    #[test]
    fn converts_epub_to_a_text_encoding_markdown_and_html() {
        let bytes = epub3_fixture();
        let utf16 = convert_bytes("book.epub", &bytes, OutputFormat::TextUtf16Le).unwrap();
        assert!(utf16[0].bytes.starts_with(&[0xff, 0xfe]));

        let markdown = String::from_utf8(
            convert_bytes("book.epub", &bytes, OutputFormat::Markdown).unwrap()[0]
                .bytes
                .clone(),
        )
        .unwrap();
        assert!(markdown.contains("# Chapter One"), "{markdown}");
        assert!(markdown.contains("*quiet*"), "{markdown}");

        let html = String::from_utf8(
            convert_bytes("book.epub", &bytes, OutputFormat::Html).unwrap()[0]
                .bytes
                .clone(),
        )
        .unwrap();
        assert!(html.starts_with("<!DOCTYPE html>"), "{html}");
        assert!(html.contains("<p>A quiet beginning.</p>"), "{html}");
        assert!(html.contains("id=\"chapter-2\""), "{html}");
        assert!(!html.contains("<script"), "{html}");
    }

    #[test]
    fn decodes_entities_and_keeps_line_breaks() {
        let text = epub_text(&epub_fixture());
        assert!(text.contains("tea & cake"), "{text}");
        assert!(text.contains("first line\nsecond line"), "{text}");
    }

    #[test]
    fn inline_images_in_html_output() {
        let html = String::from_utf8(
            convert_bytes("comic.epub", &epub_with_image(), OutputFormat::Html).unwrap()[0]
                .bytes
                .clone(),
        )
        .unwrap();
        assert!(html.contains("data:image/png;base64,iVBORw0KGgo"), "{html}");
    }

    #[test]
    fn refuses_drm_with_a_typed_error() {
        let bytes = epub_with_encryption(
            "<EncryptionMethod Algorithm=\"http://www.w3.org/2001/04/xmlenc#aes128-cbc\"/>",
            false,
        );
        let error = convert_bytes("book.epub", &bytes, OutputFormat::Text).unwrap_err();
        assert!(
            matches!(error, ConvertError::EbookDrm { ref scheme } if scheme == "Adobe ADEPT"),
            "{error}"
        );
        let error = convert_bytes("book.epub", &bytes, OutputFormat::Html).unwrap_err();
        assert!(matches!(error, ConvertError::EbookDrm { .. }), "{error}");
    }

    #[test]
    fn font_obfuscation_is_not_drm() {
        let bytes = epub_with_encryption(
            "<EncryptionMethod Algorithm=\"http://www.idpf.org/2008/embedding\"/>",
            false,
        );
        assert!(epub_text(&bytes).contains("A quiet beginning"));
    }

    #[test]
    fn rights_xml_is_drm() {
        let bytes = epub_with_encryption("", true);
        let error = convert_bytes("book.epub", &bytes, OutputFormat::Text).unwrap_err();
        assert!(matches!(error, ConvertError::EbookDrm { .. }), "{error}");
    }

    #[test]
    fn an_epub_repacks_as_an_archive() {
        let zip = convert_bytes("book.epub", &epub3_fixture(), OutputFormat::Zip).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(zip[0].bytes.clone())).unwrap();
        assert!(archive.by_name("META-INF/container.xml").is_ok());
        assert!(archive.by_name("OEBPS/content.opf").is_ok());
        assert!(archive.by_name("OEBPS/ch1.xhtml").is_ok());
    }

    #[test]
    fn reports_a_corrupt_container_and_an_empty_book() {
        let error =
            convert_bytes("book.epub", b"not a zip at all", OutputFormat::Text).unwrap_err();
        assert!(
            matches!(error, ConvertError::EbookCorrupt { .. }),
            "{error}"
        );

        let error = convert_bytes(
            "book.epub",
            &zip_with_entries(&[("mimetype".to_owned(), MIMETYPE_BYTES.to_vec())]),
            OutputFormat::Text,
        )
        .unwrap_err();
        assert!(
            matches!(error, ConvertError::EbookCorrupt { .. }),
            "{error}"
        );

        let empty = epub_fixture_with_body("<p></p>");
        let error = convert_bytes("book.epub", &empty, OutputFormat::Text).unwrap_err();
        assert!(matches!(error, ConvertError::EbookEmpty), "{error}");
    }

    fn epub_fixture_with_body(body: &str) -> Vec<u8> {
        crate::test_support::epub2_with_bodies(&[body])
    }

    #[test]
    fn bounds_entries_and_declared_size() {
        assert!(check_ebook_limits(1, 0).is_ok());
        assert!(matches!(
            check_ebook_limits(MAX_EBOOK_ENTRIES + 1, 0),
            Err(ConvertError::EbookTooManyEntries { .. })
        ));
        assert!(matches!(
            check_ebook_limits(1, MAX_EBOOK_BYTES + 1),
            Err(ConvertError::EbookTooLarge { .. })
        ));

        let entries: Vec<(String, Vec<u8>)> = (0..=MAX_EBOOK_ENTRIES)
            .map(|index| (format!("entry{index}.txt"), Vec::new()))
            .collect();
        let many = zip_with_entries(&entries);
        let error = convert_bytes("book.epub", &many, OutputFormat::Text).unwrap_err();
        assert!(
            matches!(error, ConvertError::EbookTooManyEntries { .. }),
            "{error}"
        );
    }

    #[test]
    fn builds_an_epub_that_opens_and_keeps_its_chapters() {
        let text = "A first paragraph.\n\nA second paragraph.\n";
        let epub = convert_bytes("notes.txt", text.as_bytes(), OutputFormat::Epub).unwrap();
        let bytes = &epub[0].bytes;
        assert_eq!(epub[0].name, std::path::PathBuf::from("notes.epub"));
        assert!(is_epub_container(bytes));

        let mut archive = zip::ZipArchive::new(Cursor::new(bytes.clone())).unwrap();
        let first = archive.by_index(0).unwrap();
        assert_eq!(first.name(), "mimetype");
        assert_eq!(first.compression(), zip::CompressionMethod::Stored);
        drop(first);
        let mut mimetype = String::new();
        archive
            .by_name("mimetype")
            .unwrap()
            .read_to_string(&mut mimetype)
            .unwrap();
        assert_eq!(mimetype, "application/epub+zip");

        let round_trip = epub_text(bytes);
        assert_eq!(round_trip, text);
    }

    #[test]
    fn builds_an_epub_from_markdown_and_html() {
        let markdown = "# One\n\nFirst *chapter*.\n\n# Two\n\nSecond **chapter**.\n";
        let epub = convert_bytes("book.md", markdown.as_bytes(), OutputFormat::Epub).unwrap();
        let epub_reader = Epub::read(Cursor::new(epub[0].bytes.clone())).unwrap();
        assert_eq!(epub_reader.spine().len(), 2);
        let text = epub_text(&epub[0].bytes);
        assert!(text.contains("First chapter."), "{text}");
        assert!(text.contains("Second chapter."), "{text}");

        let html = "<!DOCTYPE html><html><head><title>Web Page</title></head><body><h1>Top</h1><p>Body text.</p><script>alert(1)</script><p>Unclosed<br></body></html>";
        let epub = convert_bytes("page.html", html.as_bytes(), OutputFormat::Epub).unwrap();
        let reader = Epub::read(Cursor::new(epub[0].bytes.clone())).unwrap();
        assert_eq!(reader.metadata().title().unwrap().value(), "Web Page");
        let xhtml = String::from_utf8(
            convert_bytes("page.epub", &epub[0].bytes, OutputFormat::Html).unwrap()[0]
                .bytes
                .clone(),
        )
        .unwrap();
        assert!(xhtml.contains("Body text."), "{xhtml}");
        assert!(!xhtml.contains("alert(1)"), "{xhtml}");
    }

    #[test]
    fn converts_html_and_markdown_documents_to_each_other_and_to_text() {
        let html = "<html><head><title>T</title></head><body><h1>Heading</h1><p>Text &amp; more</p></body></html>";
        let markdown = String::from_utf8(
            convert_bytes("page.html", html.as_bytes(), OutputFormat::Markdown).unwrap()[0]
                .bytes
                .clone(),
        )
        .unwrap();
        assert!(markdown.contains("# Heading"), "{markdown}");
        assert!(markdown.contains("Text & more"), "{markdown}");

        let text = String::from_utf8(
            convert_bytes("page.html", html.as_bytes(), OutputFormat::Text).unwrap()[0]
                .bytes
                .clone(),
        )
        .unwrap();
        assert!(text.contains("Heading"), "{text}");
        assert!(text.contains("Text & more"), "{text}");

        let back = String::from_utf8(
            convert_bytes("page.md", markdown.as_bytes(), OutputFormat::Html).unwrap()[0]
                .bytes
                .clone(),
        )
        .unwrap();
        assert!(back.contains("<h1>Heading</h1>"), "{back}");
        assert!(back.contains("Text &amp; more"), "{back}");
    }

    #[test]
    fn refuses_mobi_and_azw3_containers_by_name() {
        let mut mobi = vec![0_u8; 128];
        mobi[60..68].copy_from_slice(b"BOOKMOBI");
        assert!(is_mobi_container(&mobi));
        let error = convert_bytes("book.mobi", &mobi, OutputFormat::Text).unwrap_err();
        assert!(
            matches!(error, ConvertError::EbookFormatDeferred { format } if format.contains("MOBI")),
            "{error}"
        );

        let mut drm = mobi.clone();
        drm[90..92].copy_from_slice(&2_u16.to_be_bytes());
        let error = convert_bytes("book.azw3", &drm, OutputFormat::Text).unwrap_err();
        assert!(
            matches!(error, ConvertError::EbookDrm { ref scheme } if scheme == "Mobipocket"),
            "{error}"
        );

        let error = convert_bytes("book.mobi", &mobi, OutputFormat::Epub).unwrap_err();
        assert!(
            matches!(error, ConvertError::EbookFormatDeferred { .. }),
            "{error}"
        );
    }

    #[test]
    fn escapes_markup_and_attributes_in_xhtml() {
        let html = r#"<p title="a &quot;b&quot;">5 &lt; 6 &amp; 7 &gt; 4</p><br><img src="x.png">"#;
        let dom = parse_html(html);
        let fragment = xhtml_fragment(&body_nodes(&dom), None);
        assert!(fragment.contains("5 &lt; 6 &amp; 7 &gt; 4"), "{fragment}");
        assert!(fragment.contains("title=\"a &quot;b&quot;\""), "{fragment}");
        assert!(fragment.contains("<br/>"), "{fragment}");
        assert!(!fragment.contains("<img"), "{fragment}");
    }

    #[test]
    fn markdown_is_well_formed_xhtml() {
        let markdown = "# Title\n\nA [link](https://example.com) and `code`.\n\n- one\n- two\n\n> quote\n\n```\nraw <tag>\n```\n";
        let html = markdown_to_html(markdown);
        let dom = parse_html(&html);
        let fragment = xhtml_fragment(&body_nodes(&dom), None);
        assert!(fragment.contains("<h1>Title</h1>"), "{fragment}");
        assert!(
            fragment.contains("href=\"https://example.com\""),
            "{fragment}"
        );
        assert!(fragment.contains("<li>one</li>"), "{fragment}");
        assert!(fragment.contains("raw &lt;tag&gt;"), "{fragment}");
        assert_xml_balanced(&fragment);
    }

    /// Checks that every element the serialiser opened is closed, in order,
    /// which is what makes the fragment well-formed XML rather than HTML soup.
    fn assert_xml_balanced(fragment: &str) {
        let mut open: Vec<String> = Vec::new();
        let mut rest = fragment;
        while let Some(start) = rest.find('<') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('>') else {
                panic!("unterminated tag in {fragment}");
            };
            let tag = &after[..end];
            rest = &after[end + 1..];
            if tag.starts_with('!') || tag.starts_with('?') {
                continue;
            }
            if let Some(name) = tag.strip_prefix('/') {
                let expected = open.pop().expect("a closing tag with nothing open");
                assert_eq!(
                    expected,
                    name.trim(),
                    "closing tag out of order in {fragment}"
                );
            } else if !tag.ends_with('/') {
                let name = tag
                    .split_whitespace()
                    .next()
                    .expect("a tag has a name")
                    .to_owned();
                open.push(name);
            }
        }
        assert!(open.is_empty(), "unclosed elements {open:?} in {fragment}");
    }

    #[test]
    fn splits_chapters_at_top_level_headings() {
        let html = "<h1>One</h1><p>a</p><h1>Two</h1><p>b</p>";
        let dom = parse_html(html);
        let chapters = split_chapters(&body_nodes(&dom), "fallback");
        assert_eq!(chapters.len(), 2);
        assert_eq!(chapters[0].title, "One");
        assert_eq!(chapters[1].title, "Two");

        let dom = parse_html("<p>only</p>");
        let chapters = split_chapters(&body_nodes(&dom), "fallback");
        assert_eq!(chapters.len(), 1);
        assert_eq!(chapters[0].title, "fallback");
    }

    #[test]
    fn resolves_relative_image_hrefs_inside_the_container() {
        assert_eq!(
            resolve_href("OEBPS/text", "images/a.png"),
            "OEBPS/text/images/a.png"
        );
        assert_eq!(
            resolve_href("OEBPS/text", "../images/a.png"),
            "OEBPS/images/a.png"
        );
        assert_eq!(resolve_href("OEBPS/text", "/images/a.png"), "images/a.png");
        assert_eq!(resolve_href("", "a.png"), "a.png");
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn reads_a_book_with_a_nav_document_and_no_ncx() {
        let epub = epub3_fixture();
        let mut archive = zip::ZipArchive::new(Cursor::new(epub.clone())).unwrap();
        assert!(archive.by_name("OEBPS/toc.ncx").is_err());
        assert!(archive.by_name("OEBPS/nav.xhtml").is_ok());
        assert!(epub_text(&epub).contains("A quiet beginning"));
    }
}
