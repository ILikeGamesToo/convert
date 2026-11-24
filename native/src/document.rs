// SPDX-License-Identifier: GPL-2.0-or-later

//! PDF writing and PDF text extraction.

use crate::error::ConvertError;
use crate::images::{composite_channel, exif_orientation, load_image, rasterize_svg};
use crate::options::ConversionOptions;
use crate::text::decode_text;
use pdf_writer::{Content, Filter, Name, Pdf, Rect, Ref, Str};
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Write};

pub(crate) const MAX_PDF_PAGES: usize = 5_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PdfPageSize {
    A3,
    A4,
    A5,
    Letter,
    Legal,
}

impl PdfPageSize {
    pub const ALL: [Self; 5] = [Self::A3, Self::A4, Self::A5, Self::Letter, Self::Legal];

    pub fn label(self) -> &'static str {
        match self {
            Self::A3 => "A3",
            Self::A4 => "A4",
            Self::A5 => "A5",
            Self::Letter => "LETTER",
            Self::Legal => "LEGAL",
        }
    }

    /// Page dimensions in points, matching PDFKit's page sizes.
    pub(crate) fn dimensions(self) -> (f32, f32) {
        match self {
            Self::A3 => (841.89, 1190.55),
            Self::A4 => (595.28, 841.89),
            Self::A5 => (419.53, 595.28),
            Self::Letter => (612.0, 792.0),
            Self::Legal => (612.0, 1008.0),
        }
    }

    fn dimensions_for(self, orientation: PdfOrientation) -> (f32, f32) {
        let (width, height) = self.dimensions();
        match orientation {
            PdfOrientation::Portrait => (width, height),
            PdfOrientation::Landscape => (height, width),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PdfOrientation {
    Portrait,
    Landscape,
}

impl PdfOrientation {
    pub const ALL: [Self; 2] = [Self::Portrait, Self::Landscape];

    pub fn label(self) -> &'static str {
        match self {
            Self::Portrait => "Portrait",
            Self::Landscape => "Landscape",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PdfImageFit {
    Contain,
    Cover,
}

impl PdfImageFit {
    pub const ALL: [Self; 2] = [Self::Contain, Self::Cover];

    pub fn label(self) -> &'static str {
        match self {
            Self::Contain => "Contain",
            Self::Cover => "Cover",
        }
    }
}

pub(crate) fn extract_pdf_text(input: &[u8]) -> Result<String, ConvertError> {
    // pdf-extract is pure Rust and handles the common text operators. It is
    // deliberately wrapped because malformed PDFs can otherwise unwind through
    // the conversion API instead of returning a normal error.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem(input)
    }));
    match result {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(error)) => Err(ConvertError::Pdf(format!("{error:?}"))),
        Err(_) => Err(ConvertError::Pdf(
            "the PDF parser stopped on a malformed document".to_owned(),
        )),
    }
}

pub(crate) const PDF_TEXT_LINE_HEIGHT: f32 = 1.2;

/// Courier advances 0.6 em per character.
pub(crate) const PDF_TEXT_CHAR_WIDTH: f32 = 0.6;

pub(crate) fn image_to_pdf(
    input: &[u8],
    extension: &str,
    options: ConversionOptions,
) -> Result<Vec<u8>, ConvertError> {
    let orientation = exif_orientation(input).unwrap_or(1);
    if orientation == 1 {
        if let Some(components @ (1 | 3)) = jpeg_component_count(input) {
            let (width, height) = image::ImageReader::new(Cursor::new(input))
                .with_guessed_format()?
                .into_dimensions()?;
            return Ok(build_image_pdf(
                input,
                width,
                height,
                Filter::DctDecode,
                components == 1,
                options,
            ));
        }
    }

    let image = if extension == "svg" {
        rasterize_svg(input)?
    } else {
        load_image(input, options)?
    };
    let width = image.width();
    let height = image.height();
    let compressed = deflate_bytes(&flatten_to_rgb(&image, options.image_background))?;
    Ok(build_image_pdf(
        &compressed,
        width,
        height,
        Filter::FlateDecode,
        false,
        options,
    ))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ImagePlacement {
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

pub(crate) fn image_placement(
    image_width: f32,
    image_height: f32,
    page_width: f32,
    page_height: f32,
    fit: PdfImageFit,
) -> ImagePlacement {
    let scale_x = page_width / image_width;
    let scale_y = page_height / image_height;
    let scale = match fit {
        PdfImageFit::Contain => scale_x.min(scale_y),
        PdfImageFit::Cover => scale_x.max(scale_y),
    };
    let width = image_width * scale;
    let height = image_height * scale;
    ImagePlacement {
        x: (page_width - width) / 2.0,
        y: (page_height - height) / 2.0,
        width,
        height,
    }
}

pub(crate) fn build_image_pdf(
    data: &[u8],
    width: u32,
    height: u32,
    filter: Filter,
    grayscale: bool,
    options: ConversionOptions,
) -> Vec<u8> {
    let (page_width, page_height) = options
        .pdf_page_size
        .dimensions_for(options.pdf_orientation);
    let placement = image_placement(
        width as f32,
        height as f32,
        page_width,
        page_height,
        options.pdf_image_fit,
    );

    let catalog_id = Ref::new(1);
    let pages_id = Ref::new(2);
    let page_id = Ref::new(3);
    let content_id = Ref::new(4);
    let image_id = Ref::new(5);

    let mut content = Content::new();
    content.save_state();
    content.transform([
        placement.width,
        0.0,
        0.0,
        placement.height,
        placement.x,
        placement.y,
    ]);
    content.x_object(Name(b"Im1"));
    content.restore_state();
    let content = content.finish();

    let mut pdf = Pdf::new();
    pdf.catalog(catalog_id).pages(pages_id);
    pdf.pages(pages_id).kids([page_id]).count(1);
    {
        let mut page = pdf.page(page_id);
        page.parent(pages_id)
            .media_box(Rect::new(0.0, 0.0, page_width, page_height))
            .contents(content_id);
        page.resources().x_objects().pair(Name(b"Im1"), image_id);
    }
    pdf.stream(content_id, &content);
    {
        let mut x_object = pdf.image_xobject(image_id, data);
        x_object.width(width as i32);
        x_object.height(height as i32);
        if grayscale {
            x_object.color_space().device_gray();
        } else {
            x_object.color_space().device_rgb();
        }
        x_object.bits_per_component(8);
        x_object.filter(filter);
    }
    pdf.finish()
}

/// Reads the component count from a JPEG's SOF marker so RGB and grayscale
/// photos can be embedded without re-encoding.
pub(crate) fn jpeg_component_count(bytes: &[u8]) -> Option<u8> {
    if bytes.len() < 4 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut index = 2;
    while index + 4 <= bytes.len() {
        if bytes[index] != 0xff {
            return None;
        }
        let mut marker_index = index + 1;
        while marker_index < bytes.len() && bytes[marker_index] == 0xff {
            marker_index += 1;
        }
        let marker = *bytes.get(marker_index)?;
        if marker == 0x01 || marker == 0xd8 || (0xd0..=0xd7).contains(&marker) {
            index = marker_index + 1;
            continue;
        }
        if marker == 0xda || marker == 0xd9 {
            return None;
        }
        let length =
            u16::from_be_bytes([*bytes.get(marker_index + 1)?, *bytes.get(marker_index + 2)?])
                as usize;
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            return bytes.get(marker_index + 8).copied();
        }
        index = marker_index + 1 + length;
    }
    None
}

pub(crate) fn flatten_to_rgb(image: &image::DynamicImage, background: [u8; 3]) -> Vec<u8> {
    if !image.color().has_alpha() {
        return image.to_rgb8().into_raw();
    }
    let rgba = image.to_rgba8();
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for pixel in rgba.pixels() {
        for (channel, background) in pixel.0[..3].iter().zip(background) {
            rgb.push(composite_channel(*channel, pixel[3], background));
        }
    }
    rgb
}

pub(crate) fn deflate_bytes(data: &[u8]) -> Result<Vec<u8>, ConvertError> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

pub(crate) fn text_to_pdf(
    input: &[u8],
    options: ConversionOptions,
) -> Result<Vec<u8>, ConvertError> {
    let text = decode_text(input)?;
    let (page_width, page_height) = options.pdf_page_size.dimensions();
    let font_size = options.pdf_font_size.clamp(
        ConversionOptions::MIN_PDF_FONT_SIZE,
        ConversionOptions::MAX_PDF_FONT_SIZE,
    ) as f32;
    let margin = options.pdf_margin.clamp(
        ConversionOptions::MIN_PDF_MARGIN,
        ConversionOptions::MAX_PDF_MARGIN,
    ) as f32;
    let leading = font_size * PDF_TEXT_LINE_HEIGHT;
    let max_chars =
        (((page_width - 2.0 * margin) / (font_size * PDF_TEXT_CHAR_WIDTH)).floor() as usize).max(1);
    let lines_per_page = (((page_height - 2.0 * margin) / leading).floor() as usize).max(1);
    let lines = wrap_text_lines(&text, max_chars);
    let page_count = lines.len().div_ceil(lines_per_page);
    if page_count > MAX_PDF_PAGES {
        return Err(ConvertError::PdfTooManyPages {
            pages: page_count,
            max: MAX_PDF_PAGES,
        });
    }

    let catalog_id = Ref::new(1);
    let pages_id = Ref::new(2);
    let font_id = Ref::new(3);

    let mut pdf = Pdf::new();
    pdf.catalog(catalog_id).pages(pages_id);
    pdf.type1_font(font_id)
        .base_font(Name(b"Courier"))
        .encoding_predefined(Name(b"WinAnsiEncoding"));
    pdf.pages(pages_id)
        .kids((0..page_count).map(|index| Ref::new(4 + (index as i32) * 2)))
        .count(page_count as i32);

    for page_index in 0..page_count {
        let page_id = Ref::new(4 + (page_index as i32) * 2);
        let content_id = Ref::new(5 + (page_index as i32) * 2);
        let start = page_index * lines_per_page;
        let end = (start + lines_per_page).min(lines.len());

        let mut content = Content::new();
        content.begin_text();
        content.set_font(Name(b"F1"), font_size);
        content.set_text_matrix([1.0, 0.0, 0.0, 1.0, margin, page_height - margin - font_size]);
        for (line_index, line) in lines[start..end].iter().enumerate() {
            if line_index > 0 {
                content.next_line(0.0, -leading);
            }
            content.show(Str(&win_ansi_bytes(line)));
        }
        content.end_text();
        let content = content.finish();

        {
            let mut page = pdf.page(page_id);
            page.parent(pages_id)
                .media_box(Rect::new(0.0, 0.0, page_width, page_height))
                .contents(content_id);
            page.resources().fonts().pair(Name(b"F1"), font_id);
        }
        pdf.stream(content_id, &content);
    }
    Ok(pdf.finish())
}

pub(crate) fn wrap_text_lines(text: &str, max_chars: usize) -> Vec<String> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut lines = Vec::new();
    for raw_line in normalized.split('\n') {
        if raw_line.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in raw_line.split(' ') {
            if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > max_chars
            {
                lines.push(std::mem::take(&mut current));
            }
            if current.is_empty() {
                if word.chars().count() <= max_chars {
                    current.push_str(word);
                } else {
                    hard_wrap_word(word, &mut lines, &mut current, max_chars);
                }
            } else {
                current.push(' ');
                current.push_str(word);
            }
        }
        lines.push(current);
    }
    lines
}

pub(crate) fn hard_wrap_word(
    word: &str,
    lines: &mut Vec<String>,
    current: &mut String,
    max_chars: usize,
) {
    let characters: Vec<char> = word.chars().collect();
    let mut start = 0;
    while start + max_chars < characters.len() {
        lines.push(characters[start..start + max_chars].iter().collect());
        start += max_chars;
    }
    current.extend(characters[start..].iter());
}

pub(crate) fn win_ansi_bytes(text: &str) -> Vec<u8> {
    text.chars()
        .map(|character| win_ansi_byte(character).unwrap_or(b'?'))
        .collect()
}

pub(crate) fn win_ansi_byte(character: char) -> Option<u8> {
    match character {
        '\u{20}'..='\u{7e}' | '\u{a0}'..='\u{ff}' => Some(character as u8),
        '\u{20ac}' => Some(0x80),
        '\u{201a}' => Some(0x82),
        '\u{0192}' => Some(0x83),
        '\u{201e}' => Some(0x84),
        '\u{2026}' => Some(0x85),
        '\u{2020}' => Some(0x86),
        '\u{2021}' => Some(0x87),
        '\u{02c6}' => Some(0x88),
        '\u{2030}' => Some(0x89),
        '\u{0160}' => Some(0x8a),
        '\u{2039}' => Some(0x8b),
        '\u{0152}' => Some(0x8c),
        '\u{017d}' => Some(0x8e),
        '\u{2018}' => Some(0x91),
        '\u{2019}' => Some(0x92),
        '\u{201c}' => Some(0x93),
        '\u{201d}' => Some(0x94),
        '\u{2022}' => Some(0x95),
        '\u{2013}' => Some(0x96),
        '\u{2014}' => Some(0x97),
        '\u{02dc}' => Some(0x98),
        '\u{2122}' => Some(0x99),
        '\u{0161}' => Some(0x9a),
        '\u{203a}' => Some(0x9b),
        '\u{0153}' => Some(0x9c),
        '\u{017e}' => Some(0x9e),
        '\u{0178}' => Some(0x9f),
        '\t' => Some(b' '),
        _ => None,
    }
}
