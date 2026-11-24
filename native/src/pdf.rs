// SPDX-License-Identifier: GPL-2.0-or-later

//! PDF page rasterisation.
//!
//! `hayro` interprets a PDF and rasterises each page on the CPU. It is pure
//! Rust with no C library and no external binary, and its default features
//! bundle permissively licensed substitutes for the 14 standard fonts and the
//! predefined CMaps, so a document that embeds no fonts still draws real
//! glyphs.
//!
//! The web app's pdfjs handler renders every page and names the results
//! `<stem>_<page>.<ext>`, zero-based. This module renders in page order and
//! hands each page to the caller, which does the same naming. A page is drawn
//! at one pixel per point, which is the scale pdfjs uses by default.
//!
//! `hayro` cannot decrypt, so an encrypted document is reported as an error
//! rather than rendered.

use crate::error::ConvertError;
use hayro::hayro_interpret::hayro_syntax::page::Page;
use hayro::hayro_interpret::hayro_syntax::{LoadPdfError, Pdf};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::vello_cpu::peniko::ImageAlphaType;
use hayro::vello_cpu::Pixmap;
use hayro::{PixmapSettings, RenderCache, RenderSettings};
use hayro_svg::{RenderCache as SvgRenderCache, SvgRenderSettings};
use image::DynamicImage;

/// The largest page edge this app will rasterise, in pixels. The rasteriser
/// addresses a page with 16-bit coordinates, so a longer edge would be
/// truncated; 8192 points is also 113 inches, far past any real page.
pub(crate) const MAX_PAGE_EDGE: u32 = 8192;

/// The largest page area this app will rasterise, in pixels: 4096 x 4096, or
/// 64 MiB as RGBA. A page is rendered at one pixel per point, so an A0 sheet
/// (841 x 1189) and a 2 m poster both fit comfortably; only a deliberately
/// oversized page hits the limit.
pub(crate) const MAX_PAGE_PIXELS: u64 = 4096 * 4096;

/// Renders every page of a PDF to an RGBA image, one page at a time.
///
/// `emit` receives the zero-based page index and the page image, in page order.
/// It exists so a long document is encoded and released page by page instead of
/// being held in memory as a whole. Returning an error from `emit` stops the
/// render and propagates that error.
pub(crate) fn render_pages<F>(bytes: &[u8], mut emit: F) -> Result<(), ConvertError>
where
    F: FnMut(usize, DynamicImage) -> Result<(), ConvertError>,
{
    let render = || {
        let document = open(bytes)?;
        let cache = RenderCache::new();
        let settings = InterpreterSettings::default();
        let render_settings = RenderSettings::default();
        for (index, page) in document.pages().iter().enumerate() {
            check_page_size(page, index)?;
            let pixmap = hayro::render(
                page,
                &cache,
                &settings,
                &render_settings,
                &PixmapSettings {
                    x_scale: 1.0,
                    y_scale: 1.0,
                    bg_color: WHITE,
                },
            );
            emit(index, to_image(pixmap)?)?;
        }
        Ok(())
    };
    // The renderer documents panics on malformed input as a known robustness
    // gap. A panic must not take the whole app down, so it is caught here the
    // same way PDF text extraction catches its own.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(render)) {
        Ok(result) => result,
        Err(_) => Err(ConvertError::Pdf(
            "the PDF renderer stopped on a malformed document".to_owned(),
        )),
    }
}

/// The number of pages in a PDF, without rendering any of them.
///
/// Reading the page tree costs far less than drawing a page, so a caller can
/// size a progress bar before the first page is rendered. An encrypted or
/// unreadable document reports the same error `render_pages` would.
pub(crate) fn page_count(bytes: &[u8]) -> Result<usize, ConvertError> {
    Ok(open(bytes)?.pages().len())
}

/// Writes every page of a PDF as a standalone SVG document, one page at a time.
///
/// `emit` receives the zero-based page index and the SVG text, in page order.
/// Returning an error from `emit` stops the conversion and propagates that
/// error. The page is drawn on an opaque white background, matching the raster
/// path, so a page that leans on transparency still reads as paper.
pub(crate) fn render_svg_pages<F>(bytes: &[u8], mut emit: F) -> Result<(), ConvertError>
where
    F: FnMut(usize, String) -> Result<(), ConvertError>,
{
    let render = || {
        let document = open(bytes)?;
        let cache = SvgRenderCache::new();
        let settings = InterpreterSettings::default();
        let render_settings = SvgRenderSettings {
            bg_color: [255, 255, 255, 255],
        };
        for (index, page) in document.pages().iter().enumerate() {
            let (width, height) = page.render_dimensions();
            if !(width.is_finite() && height.is_finite()) || width < 1.0 || height < 1.0 {
                return Err(ConvertError::Pdf(format!(
                    "page {} has no usable size ({width} x {height})",
                    index + 1
                )));
            }
            emit(
                index,
                hayro_svg::convert(page, &cache, &settings, &render_settings),
            )?;
        }
        Ok(())
    };
    // The SVG backend documents panics on malformed input as a known gap, the
    // same as the rasteriser, so it is caught at the same boundary.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(render)) {
        Ok(result) => result,
        Err(_) => Err(ConvertError::Pdf(
            "the SVG renderer stopped on a malformed document".to_owned(),
        )),
    }
}

fn open(bytes: &[u8]) -> Result<Pdf, ConvertError> {
    Pdf::new(bytes.to_vec()).map_err(|error| match error {
        LoadPdfError::Decryption(_) => ConvertError::PdfEncrypted,
        LoadPdfError::Invalid => {
            ConvertError::Pdf("the file is not a PDF this app can read".to_owned())
        }
    })
}

/// Rejects a page the rasteriser cannot address before it is rendered.
fn check_page_size(page: &Page<'_>, index: usize) -> Result<(), ConvertError> {
    let (width, height) = page.render_dimensions();
    let too_large = !(width.is_finite() && height.is_finite())
        || width < 1.0
        || height < 1.0
        || width > MAX_PAGE_EDGE as f32
        || height > MAX_PAGE_EDGE as f32
        || width as f64 * height as f64 > MAX_PAGE_PIXELS as f64;
    if too_large {
        return Err(ConvertError::PdfPageTooLarge {
            page: index + 1,
            width,
            height,
        });
    }
    Ok(())
}

/// Turns a rendered page into an image. The page was drawn onto an opaque white
/// background, so the alpha channel is 255 everywhere and the image is opaque.
fn to_image(pixmap: Pixmap) -> Result<DynamicImage, ConvertError> {
    let width = u32::from(pixmap.width());
    let height = u32::from(pixmap.height());
    let pixels = pixmap.take_rgba8(ImageAlphaType::Alpha);
    image::RgbaImage::from_raw(width, height, pixels)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| {
            ConvertError::Pdf(format!(
                "the renderer returned a {width} x {height} page with the wrong pixel count"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::OutputFormat;
    use pdf_writer::{Content, Pdf, Rect, Ref};

    /// A PDF with one page per size, each page carrying a 40 x 40 point black
    /// rectangle whose lower-left corner sits at (20, 20). Small enough to
    /// assert on individual pixels and large enough to survive anti-aliasing.
    fn sample_pdf(pages: &[(f32, f32)]) -> Vec<u8> {
        let mut pdf = Pdf::new();
        let catalog_id = Ref::new(1);
        let pages_id = Ref::new(2);
        pdf.catalog(catalog_id).pages(pages_id);
        let kids: Vec<Ref> = (0..pages.len())
            .map(|index| Ref::new(3 + index as i32 * 2))
            .collect();
        pdf.pages(pages_id).kids(kids).count(pages.len() as i32);
        for (index, (width, height)) in pages.iter().copied().enumerate() {
            let page_id = Ref::new(3 + index as i32 * 2);
            let content_id = Ref::new(4 + index as i32 * 2);
            let mut content = Content::new();
            content.set_fill_color([0.0, 0.0, 0.0]);
            content.rect(20.0, 20.0, 40.0, 40.0);
            content.fill_nonzero();
            let content = content.finish();
            {
                let mut page = pdf.page(page_id);
                page.parent(pages_id)
                    .media_box(Rect::new(0.0, 0.0, width, height))
                    .contents(content_id);
            }
            pdf.stream(content_id, &content);
        }
        pdf.finish()
    }

    /// A password-protected PDF with the standard 40-bit security handler. The
    /// owner and user strings are zeroed, so no password matches and hayro
    /// reports the document as password-protected rather than corrupt. The
    /// cross-reference table is written by hand so its offsets stay exact.
    fn encrypted_pdf() -> Vec<u8> {
        let mut out = Vec::new();
        let mut offsets = Vec::new();
        out.extend_from_slice(b"%PDF-1.7\n");
        let objects: [&[u8]; 4] = [
            b"<< /Type /Catalog /Pages 2 0 R >>",
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] >>",
            b"<< /Filter /Standard /V 1 /R 2 /O <0000000000000000000000000000000000000000000000000000000000000000> /U <0000000000000000000000000000000000000000000000000000000000000000> /P -1 >>",
        ];
        for (index, body) in objects.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        out.extend_from_slice(b"0000000000 65535 f \n");
        for offset in &offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R /Encrypt 4 0 R /ID [<00> <00>] >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    fn render_all(bytes: &[u8]) -> Result<Vec<DynamicImage>, ConvertError> {
        let mut pages = Vec::new();
        render_pages(bytes, |_, image| {
            pages.push(image);
            Ok(())
        })?;
        Ok(pages)
    }

    #[test]
    fn renders_a_page_to_an_image() {
        let pages = render_all(&sample_pdf(&[(200.0, 100.0)])).unwrap();
        assert_eq!(pages.len(), 1);
        let image = &pages[0];
        assert_eq!((image.width(), image.height()), (200, 100));
        let rgba = image.to_rgba8();
        // PDF coordinates put the rectangle at (20, 20) to (60, 60), so in the
        // image it spans columns 20..60 and rows 40..80 counted from the top.
        assert_eq!(rgba.get_pixel(40, 60).0, [0, 0, 0, 255]);
        assert_eq!(rgba.get_pixel(0, 0).0, [255, 255, 255, 255]);
    }

    #[test]
    fn renders_every_page_of_a_multi_page_pdf() {
        let mut indices = Vec::new();
        let mut sizes = Vec::new();
        render_pages(
            &sample_pdf(&[(200.0, 100.0), (100.0, 50.0)]),
            |index, image| {
                indices.push(index);
                sizes.push((image.width(), image.height()));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(indices, [0, 1]);
        assert_eq!(sizes, [(200, 100), (100, 50)]);
    }

    #[test]
    fn rasterises_a_pdf_this_app_wrote() {
        // The text writer uses Courier without embedding it, so this also
        // proves the bundled standard-font substitute renders real glyphs.
        let pdf = crate::document::text_to_pdf(
            b"Hello, rasteriser.",
            crate::options::ConversionOptions::default(),
        )
        .unwrap();
        let files = crate::convert::convert_bytes("notes.pdf", &pdf, OutputFormat::Png).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name.to_string_lossy(), "notes_0.png");
        assert!(files[0].bytes.starts_with(b"\x89PNG"));
    }

    /// A page written by Chrome's PDF backend from `testdata/pdf-source.html`:
    /// an A4 page of subset-embedded Abel text, a coloured inline SVG and a
    /// bordered table. Chrome's writer is the shape of PDF real users hand this
    /// app, so this pins glyph rendering against a document no code in this
    /// crate produced.
    #[test]
    fn rasters_a_chrome_written_pdf() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sample-chrome.pdf");
        let bytes = std::fs::read(path).unwrap();
        let pages = render_all(&bytes).unwrap();
        assert_eq!(pages.len(), 1);
        let image = &pages[0];
        // A4 is 594.96 x 841.92 points, and both pdfjs and this module floor the
        // page size to whole pixels, so the render is 594 x 841.
        assert_eq!((image.width(), image.height()), (594, 841));
        let rgba = image.to_rgba8();
        let (mut ink, mut accent, mut distinct) = (0u32, 0u32, std::collections::HashSet::new());
        for pixel in rgba.pixels() {
            let [r, g, b, a] = pixel.0;
            assert_eq!(a, 255, "the page background must be opaque");
            if r < 240 || g < 240 || b < 240 {
                ink += 1;
            }
            if r.abs_diff(0xb3) <= 2 && g.abs_diff(0x26) <= 2 && b.abs_diff(0x1e) <= 2 {
                accent += 1;
            }
            distinct.insert(pixel.0);
        }
        let total = image.width() * image.height();
        assert!(
            ink * 20 > total,
            "expected the page to carry text and vector ink, found {ink} of {total} pixels"
        );
        assert!(
            accent > 100,
            "expected the accent red to survive, found {accent} pixels"
        );
        assert!(
            distinct.len() > 50,
            "expected anti-aliased glyphs, found only {} colours",
            distinct.len()
        );
    }

    #[test]
    fn names_rasterised_pages_like_the_web_handler() {
        let files = crate::convert::convert_bytes(
            "report.pdf",
            &sample_pdf(&[(200.0, 100.0), (100.0, 50.0)]),
            OutputFormat::Png,
        )
        .unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|file| file.name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["report_0.png", "report_1.png"]);
        assert!(files.iter().all(|file| file.bytes.starts_with(b"\x89PNG")));
    }

    fn render_svg_all(bytes: &[u8]) -> Result<Vec<String>, ConvertError> {
        let mut pages = Vec::new();
        render_svg_pages(bytes, |_, svg| {
            pages.push(svg);
            Ok(())
        })?;
        Ok(pages)
    }

    #[test]
    fn counts_the_pages_of_a_multi_page_pdf() {
        let bytes = sample_pdf(&[(200.0, 100.0), (100.0, 50.0)]);
        assert_eq!(page_count(&bytes).unwrap(), 2);
        assert!(matches!(
            page_count(&encrypted_pdf()),
            Err(ConvertError::PdfEncrypted)
        ));
    }

    #[test]
    fn writes_an_svg_page_per_pdf_page() {
        let bytes = sample_pdf(&[(200.0, 100.0), (100.0, 50.0)]);
        let files =
            crate::convert::convert_bytes("report.pdf", &bytes, OutputFormat::SvgPage).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|file| file.name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["report_0.svg", "report_1.svg"]);

        let pages = render_svg_all(&bytes).unwrap();
        assert_eq!(pages.len(), 2);
        for (page, expected) in pages.iter().zip(["0 0 200 100", "0 0 100 50"]) {
            assert!(page.starts_with("<svg"), "expected an SVG document: {page}");
            assert!(
                page.contains(&format!("viewBox=\"{expected}\"")),
                "expected the page's own point size as the viewBox: {page}"
            );
            // The rectangle stays geometry: the source's black fill is written
            // as a colour, not sampled into a bitmap.
            assert!(
                page.contains("fill=\"#000000\""),
                "expected vector ink: {page}"
            );
            assert!(
                !page.contains("<image"),
                "expected no raster fallback: {page}"
            );
        }
    }

    #[test]
    fn rejects_an_encrypted_pdf() {
        let error = render_all(&encrypted_pdf()).unwrap_err();
        assert!(
            matches!(error, ConvertError::PdfEncrypted),
            "expected PdfEncrypted, got {error:?}"
        );
    }

    #[test]
    fn rejects_a_truncated_pdf() {
        let error = render_all(b"%PDF-1.7\nthis is not a pdf").unwrap_err();
        assert!(
            matches!(error, ConvertError::Pdf(_)),
            "expected Pdf, got {error:?}"
        );
    }

    #[test]
    fn rejects_an_oversized_page() {
        let error = render_all(&sample_pdf(&[(5000.0, 5000.0)])).unwrap_err();
        assert!(
            matches!(
                error,
                ConvertError::PdfPageTooLarge {
                    page: 1,
                    width: 5000.0,
                    height: 5000.0
                }
            ),
            "expected PdfPageTooLarge, got {error:?}"
        );
    }
}
