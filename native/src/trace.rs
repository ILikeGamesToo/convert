// SPDX-License-Identifier: GPL-2.0-or-later

//! Raster to SVG vector tracing.
//!
//! A trace is a new document built out of the pixels, not a wrapper around the
//! raster: the pixels are grouped into colour layers, each layer's boundary is
//! fitted with paths, and the paths are written back to front. `vtracer` does
//! the grouping and the fitting in pure Rust; this module owns the limits and
//! the configuration, because those two decide whether a trace is usable.
//!
//! The configuration is deliberately one fixed set rather than a preset menu.
//! It was chosen by measuring both fixtures in `testdata/`: a five-colour logo
//! with hard edges, where the round trip should be near-exact, and a synthetic
//! photograph, where it can only be approximate. See [`config`] for the numbers
//! behind each choice.

use crate::error::ConvertError;
use visioncortex::PathSimplifyMode;
use vtracer::{ColorImage, ColorMode, Config, Hierarchical};

/// Largest side a traced image may have, in pixels.
pub const MAX_TRACE_DIMENSION: u32 = 4_096;

/// Largest pixel count a traced image may have.
///
/// Tracing cost grows faster than the pixel count: measured on the release
/// build of this crate, a 320x240 image takes 0.07 s, 1.9 MP takes 7.8 s and
/// 4 MP takes 26 s with a 400 MB peak, while 8 MP takes 59 s and 850 MB. 4 MP is
/// the largest that finishes in a time a person will sit through, so anything
/// bigger is refused with [`ConvertError::TraceTooLarge`] instead of grinding.
pub const MAX_TRACE_PIXELS: u64 = 4_000_000;

/// Whether an input extension is traced. Every raster the app decodes is, but
/// an SVG is already vector: tracing it would rasterise it first and lose the
/// very detail the input was made of.
pub(crate) fn is_traceable_extension(extension: &str) -> bool {
    crate::shared::is_image_input_extension(extension) && extension != "svg"
}

/// The configuration every trace uses.
///
/// Colour precision stays at 6 bits per channel with a layer difference of 16.
/// Going to 8 bits and 48 levels — the numbers behind `vtracer`'s own photo
/// preset — moves the mean round-trip error on the photo fixture by under
/// 0.5/255 while multiplying the path count (64 to 98 paths at 320x240, 256 to
/// 1112 at 1.9 MP) and roughly doubling the time. Path count is what turns a
/// trace into a blob, so the cheaper setting wins.
///
/// Spline is chosen over polygon even though polygon is 3.7 times smaller: on
/// the 320x240 photo the same 64 paths cost 119 KiB as splines and 32 KiB as
/// line segments. The pixel error at native size cannot tell them apart, but a
/// trace exists to be scaled. Rendered 16 times up against the logo fixture's
/// circle, the spline boundary sits 0.15 px (RMS, worst 0.27) from the true
/// edge, while the polygon chords it at 0.33 px (worst 0.52) and always to the
/// inside — 8 px of visible faceting at that scale. Half a pixel of systematic
/// error is worth 3.7 times the bytes.
///
/// `filter_speckle` 4 drops areas under 4x4 pixels, which is what keeps the
/// grain in a photograph from becoming thousands of one-pixel paths.
/// `corner_threshold` 60 keeps a right angle sharp, so a rectangle stays a
/// rectangle, and `path_precision` 2 keeps coordinates at a hundredth of a
/// pixel.
fn config() -> Config {
    Config {
        color_mode: ColorMode::Color,
        hierarchical: Hierarchical::Stacked,
        filter_speckle: 4,
        color_precision: 6,
        layer_difference: 16,
        mode: PathSimplifyMode::Spline,
        corner_threshold: 60,
        length_threshold: 4.0,
        max_iterations: 10,
        splice_threshold: 45,
        path_precision: Some(2),
    }
}

/// Traces a decoded image into an SVG document.
pub(crate) fn trace(image: &image::DynamicImage) -> Result<String, ConvertError> {
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return Err(ConvertError::Trace(
            "the image has no pixels to trace".to_owned(),
        ));
    }
    if width > MAX_TRACE_DIMENSION
        || height > MAX_TRACE_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_TRACE_PIXELS
    {
        return Err(ConvertError::TraceTooLarge {
            width,
            height,
            max_side: MAX_TRACE_DIMENSION,
            max_pixels: MAX_TRACE_PIXELS,
        });
    }

    let mut pixels = image.to_rgba8().into_raw();
    // A fully transparent pixel carries no colour of its own, and encoders
    // commonly store black there. The tracer works on colour, so a transparent
    // region it does not key out would otherwise be traced as a black shape.
    // Giving those pixels white keeps the alpha (a transparent background is
    // still keyed out and stays transparent) and makes the residue the
    // background this app already composites alpha onto.
    for pixel in pixels.as_chunks_mut::<4>().0 {
        if pixel[3] == 0 {
            pixel[..3].fill(255);
        }
    }

    let svg = vtracer::convert(
        ColorImage {
            pixels,
            width: width as usize,
            height: height as usize,
        },
        config(),
    )
    .map_err(ConvertError::Trace)?;
    Ok(svg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::convert_bytes;
    use crate::format::OutputFormat;
    use resvg::tiny_skia;
    use resvg::usvg;

    /// How far the trace of a fixture lands from the fixture itself once it is
    /// rendered back at the same size: the mean absolute per-channel
    /// difference, the difference 99% of pixels stay under, and the size of the
    /// document that did it.
    struct RoundTrip {
        mean: f64,
        p99: u8,
        paths: usize,
        bytes: usize,
    }

    fn render(svg: &str, width: u32, height: u32) -> image::RgbaImage {
        let tree = usvg::Tree::from_data(svg.as_bytes(), &usvg::Options::default())
            .expect("the trace is a parseable SVG document");
        let mut pixmap =
            tiny_skia::Pixmap::new(width, height).expect("a pixmap of the source size");
        resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
        image::RgbaImage::from_raw(width, height, pixmap.take()).expect("rendered pixels")
    }

    fn round_trip(fixture: &str) -> (RoundTrip, String, image::RgbaImage) {
        let bytes = std::fs::read(format!("testdata/{fixture}")).expect("the fixture is committed");
        let source = image::load_from_memory(&bytes).expect("the fixture decodes");
        let (width, height) = (source.width(), source.height());

        let converted = convert_bytes(fixture, &bytes, OutputFormat::SvgTrace).expect("traced");
        assert_eq!(converted.len(), 1);
        assert_eq!(
            converted[0].name.to_string_lossy(),
            fixture.replace(".png", ".svg")
        );
        let svg = String::from_utf8(converted[0].bytes.clone()).expect("the trace is UTF-8");
        assert!(svg.starts_with("<?xml"));
        assert!(svg.contains("<svg"));
        assert!(svg.trim_end().ends_with("</svg>"));

        let rendered = render(&svg, width, height);
        let source = source.to_rgba8();
        let mut total: u64 = 0;
        let mut worst = Vec::with_capacity((width * height) as usize);
        for (pixel, other) in source.pixels().zip(rendered.pixels()) {
            let mut pixel_worst = 0u8;
            for channel in 0..3 {
                let diff = i32::from(pixel[channel]).abs_diff(i32::from(other[channel]));
                total += u64::from(diff);
                pixel_worst = pixel_worst.max(diff as u8);
            }
            worst.push(pixel_worst);
        }
        worst.sort_unstable();
        let count = worst.len();
        let trip = RoundTrip {
            mean: total as f64 / (count as f64 * 3.0),
            p99: worst[count * 99 / 100],
            paths: svg.matches("<path").count(),
            bytes: svg.len(),
        };
        (trip, svg, rendered)
    }

    #[test]
    fn logo_round_trips_near_exactly() {
        let (trip, _svg, _rendered) = round_trip("trace-logo.png");
        // Measured 0.64/255 mean, 0 worst difference on 99% of pixels.
        assert!(trip.mean < 1.5, "mean error {} is too high", trip.mean);
        assert!(trip.p99 <= 8, "99th percentile error {}", trip.p99);
        // Five flat regions, so five paths: a trace that shatters the shapes
        // would still pass an error bound but would be useless.
        assert_eq!(trip.paths, 5, "one path per colour region");
        assert!(
            trip.bytes < 4_096,
            "{} bytes is too large for five shapes",
            trip.bytes
        );
    }

    #[test]
    fn photo_round_trips_within_a_looser_bound() {
        let (trip, _svg, _rendered) = round_trip("trace-photo.png");
        // Measured 5.06/255 mean, 19 at the 99th percentile.
        assert!(trip.mean < 10.0, "mean error {} is too high", trip.mean);
        assert!(trip.p99 <= 40, "99th percentile error {}", trip.p99);
        // The grain must not become one path per speck.
        assert!(
            trip.paths <= 128,
            "{} paths is a blob, not a trace",
            trip.paths
        );
        assert!(trip.bytes < 262_144, "{} bytes is too large", trip.bytes);
    }

    #[test]
    fn a_transparent_background_stays_transparent() {
        let mut image = image::RgbaImage::from_pixel(64, 64, image::Rgba([0, 0, 0, 0]));
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            if (16..48).contains(&x) && (16..48).contains(&y) {
                *pixel = image::Rgba([200, 30, 30, 255]);
            }
        }
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();

        let converted = convert_bytes("square.png", png.get_ref(), OutputFormat::SvgTrace).unwrap();
        let svg = String::from_utf8(converted[0].bytes.clone()).unwrap();
        let rendered = render(&svg, 64, 64);
        assert_eq!(
            rendered.get_pixel(0, 0)[3],
            0,
            "the corner stays transparent"
        );
        assert_eq!(rendered.get_pixel(32, 32)[3], 255, "the shape is opaque");
        assert!(rendered.get_pixel(32, 32)[0] > 150);
    }

    #[test]
    fn refuses_images_over_the_trace_limit() {
        let image =
            image::RgbaImage::from_pixel(MAX_TRACE_DIMENSION + 1, 8, image::Rgba([1, 2, 3, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let error = convert_bytes("wide.png", png.get_ref(), OutputFormat::SvgTrace).unwrap_err();
        assert!(
            matches!(error, ConvertError::TraceTooLarge { width, .. } if width == MAX_TRACE_DIMENSION + 1)
        );

        let tall =
            image::RgbaImage::from_pixel(8, MAX_TRACE_DIMENSION + 1, image::Rgba([1, 2, 3, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(tall)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        assert!(matches!(
            convert_bytes("tall.png", png.get_ref(), OutputFormat::SvgTrace).unwrap_err(),
            ConvertError::TraceTooLarge { .. }
        ));
    }

    #[test]
    fn refuses_inputs_that_are_not_raster_images() {
        // A PDF already has an exact vector target (`svg-page`), so tracing one
        // would only lose detail; a font is vector input too.
        let pdf = std::fs::read("testdata/sample-chrome.pdf").expect("the fixture is committed");
        assert!(matches!(
            convert_bytes("sample-chrome.pdf", &pdf, OutputFormat::SvgTrace).unwrap_err(),
            ConvertError::Unsupported { .. }
        ));
        assert!(matches!(
            convert_bytes("song.mp3", b"ID3", OutputFormat::SvgTrace).unwrap_err(),
            ConvertError::Unsupported { .. }
        ));
    }
}
