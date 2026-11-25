// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, TryRecvError},
        Arc,
    },
    thread,
    time::Duration,
};

use convert_to_it_native::{
    audio_output_name, audio_tracks_from_path, convert_audio_file_to_writer_with_cancel,
    convert_batch_with_options, convert_bytes_with_video_config, input_format,
    input_format::InputFormat, planned_output_count, supported_input_extensions, AudioBitDepth,
    AudioChannels, AudioTrackInfo, ConversionOptions, ConvertError, Mp3Bitrate, OutputFormat,
    PdfImageFit, PdfOrientation, PdfPageSize, VideoToolConfig,
};
use eframe::egui::{self, Align, Color32, FontId, Layout, RichText, Stroke, Vec2};
use input_format::{common_format, detect};
use rfd::FileDialog;
use serde::{Deserialize, Serialize};

const BG: Color32 = Color32::from_rgb(25, 25, 25);
const CARD: Color32 = Color32::from_rgb(32, 32, 32);
const SOFT: Color32 = Color32::from_rgb(37, 37, 37);
const BORDER: Color32 = Color32::from_rgb(55, 55, 55);
const PRIMARY: Color32 = Color32::from_rgb(35, 131, 226);
const TEXT: Color32 = Color32::from_rgb(209, 209, 209);
// Secondary text. 154 keeps body-size text at 5.45:1 on `SOFT` and 5.79:1 on
// `CARD`; 128 was 3.88:1, under the 4.5:1 that small text needs.
const MUTED: Color32 = Color32::from_rgb(154, 154, 154);
const FOOTER_TEXT: Color32 = Color32::from_rgb(94, 94, 94);
const GREEN: Color32 = Color32::from_rgb(83, 190, 132);
const RED: Color32 = Color32::from_rgb(235, 93, 93);
const ACTION_BAR_HEIGHT: f32 = 70.0;
const FORMAT_LIST_SCROLLBAR_GUTTER: f32 = 15.0;
const HOME_CARD_HEIGHT: f32 = 354.36;
const HOME_FOOTER_HEIGHT: f32 = 27.78;
const APP_ID: &str = "Convert to it!";
const SETTINGS_KEY: &str = "convert-to-it/settings";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct PersistedSettings {
    output_directory: Option<PathBuf>,
    target: Option<OutputFormat>,
    conversion_options: ConversionOptions,
}

fn main() -> eframe::Result<()> {
    let options = native_options(persistence_path());
    let result = eframe::run_native(
        APP_ID,
        options,
        Box::new(|creation_context| {
            apply_theme(&creation_context.egui_ctx);
            Ok(Box::new(ConvertApp::from_storage(creation_context.storage)))
        }),
    );
    if let Err(error) = &result {
        rfd::MessageDialog::new()
            .set_title("Convert to it! couldn't start")
            .set_description(error.to_string())
            .set_level(rfd::MessageLevel::Error)
            .show();
    }
    result
}

fn native_options(persistence_path: Option<PathBuf>) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([767.0, 675.0])
            .with_min_inner_size([600.0, 600.0]),
        persistence_path,
        ..Default::default()
    }
}

fn persistence_path() -> Option<PathBuf> {
    persistence_path_in(&eframe::storage_dir(APP_ID)?)
}

fn persistence_path_in(directory: &Path) -> Option<PathBuf> {
    fs::create_dir_all(directory).ok()?;
    Some(directory.join("app.ron"))
}

fn default_output_directory() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    if let Some(home) = home {
        let downloads = PathBuf::from(home).join("Downloads");
        if downloads.is_dir() {
            return Some(downloads);
        }
    }
    std::env::current_dir().ok()
}

fn apply_theme(context: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = CARD;
    visuals.extreme_bg_color = SOFT;
    visuals.faint_bg_color = SOFT;
    visuals.selection.bg_fill = Color32::from_rgb(35, 83, 131);
    visuals.selection.stroke = Stroke::new(1.0_f32, PRIMARY);
    visuals.widgets.noninteractive.bg_fill = CARD;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    visuals.widgets.inactive.bg_fill = SOFT;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(49, 49, 49);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, PRIMARY);
    visuals.widgets.active.bg_fill = Color32::from_rgb(41, 98, 157);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    visuals.widgets.open.bg_fill = SOFT;
    visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, TEXT);
    visuals.hyperlink_color = PRIMARY;
    visuals.override_text_color = Some(TEXT);
    context.set_visuals(visuals);

    let mut style = (*context.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(14.0, 9.0);
    style.visuals.window_corner_radius = egui::CornerRadius::same(12);
    context.set_style(style);

    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "Inter Variable".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../assets/InterVariable.ttf"
        ))),
    );
    fonts
        .families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "Inter Variable".to_owned());
    fonts.families.insert(
        egui::FontFamily::Name("Inter Variable".into()),
        vec!["Inter Variable".to_owned()],
    );
    context.set_fonts(fonts);
}

fn paint_dashed_border(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    paint_dashed_border_with_radius(painter, rect, color, 12.0);
}

fn paint_dashed_border_with_radius(
    painter: &egui::Painter,
    rect: egui::Rect,
    color: Color32,
    radius: f32,
) {
    let points = rounded_rect_outline(rect, radius);
    let perimeter: f32 = points
        .windows(2)
        .map(|pair| pair[0].distance(pair[1]))
        .sum();
    let dash_count = (perimeter / 10.0).round().max(1.0);
    let dash_length = perimeter / dash_count * 0.6;
    let dash_gap = perimeter / dash_count * 0.4;
    let stroke = Stroke::new(2.0_f32, color);
    let mut drawing_dash = true;
    let mut remaining = dash_length;

    for pair in points.windows(2) {
        let start = pair[0];
        let direction = pair[1] - start;
        let length = direction.length();
        if length == 0.0 {
            continue;
        }

        let direction = direction / length;
        let mut offset = 0.0;
        while offset < length {
            let segment_length = remaining.min(length - offset);
            if drawing_dash {
                painter.line_segment(
                    [
                        start + direction * offset,
                        start + direction * (offset + segment_length),
                    ],
                    stroke,
                );
            }
            offset += segment_length;
            remaining -= segment_length;

            if remaining <= f32::EPSILON {
                drawing_dash = !drawing_dash;
                remaining = if drawing_dash { dash_length } else { dash_gap };
            }
        }
    }
}

fn rounded_rect_outline(rect: egui::Rect, radius: f32) -> Vec<egui::Pos2> {
    let radius = radius.min(rect.width() * 0.5).min(rect.height() * 0.5);
    let corners = [
        (
            egui::pos2(rect.right() - radius, rect.top() + radius),
            -90.0,
            0.0,
        ),
        (
            egui::pos2(rect.right() - radius, rect.bottom() - radius),
            0.0,
            90.0,
        ),
        (
            egui::pos2(rect.left() + radius, rect.bottom() - radius),
            90.0,
            180.0,
        ),
        (
            egui::pos2(rect.left() + radius, rect.top() + radius),
            180.0,
            270.0,
        ),
    ];
    let mut points = vec![egui::pos2(rect.left() + radius, rect.top())];

    for (center, start, end) in corners {
        match (start, end) {
            (-90.0, 0.0) => points.push(egui::pos2(rect.right() - radius, rect.top())),
            (0.0, 90.0) => points.push(egui::pos2(rect.right(), rect.bottom() - radius)),
            (90.0, 180.0) => points.push(egui::pos2(rect.left() + radius, rect.bottom())),
            (180.0, 270.0) => points.push(egui::pos2(rect.left(), rect.top() + radius)),
            _ => {}
        }

        for step in 1..=12 {
            let angle = (start + (end - start) * step as f32 / 12.0).to_radians();
            points.push(center + Vec2::new(angle.cos(), angle.sin()) * radius);
        }
    }

    points.push(points[0]);
    points
}

type AudioTrackResponse = (u64, Result<Vec<AudioTrackInfo>, String>);

#[derive(Default)]
struct ConvertApp {
    inputs: Vec<PathBuf>,
    input_formats: Vec<InputFormat>,
    input_format_override: Option<InputFormat>,
    show_input_format_picker: bool,
    input_format_search: String,
    input_format_show_all: bool,
    page: AppPage,
    file_scroll_offset: f32,
    output_directory: Option<PathBuf>,
    target: Option<OutputFormat>,
    conversion_options: ConversionOptions,
    audio_track_choices: Vec<AudioTrackInfo>,
    audio_track_error: Option<String>,
    audio_track_source: Option<PathBuf>,
    audio_track_request: Option<Receiver<AudioTrackResponse>>,
    audio_track_generation: u64,
    audio_tracks_loading: bool,
    search_filter: String,
    selected_category: Option<FormatCategory>,
    status: Option<(String, bool)>,
    conversion_job: Option<ConversionJob>,
    conversion_notice: Option<(String, String, bool)>,
    show_settings: bool,
    show_conversion_settings: bool,
    show_help: bool,
}

enum ConversionEvent {
    Progress {
        completed: usize,
        total: usize,
        message: String,
    },
    Finished(Result<String, String>),
    Cancelled,
}

struct ConversionJob {
    receiver: Receiver<ConversionEvent>,
    cancel: Arc<AtomicBool>,
    completed: usize,
    total: usize,
    message: String,
    source_label: String,
    target_label: String,
}

#[derive(Default, PartialEq, Eq)]
enum AppPage {
    #[default]
    Upload,
    Conversion,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FormatCategory {
    Image,
    Document,
    Text,
    Data,
    Audio,
    Video,
    Archive,
    Font,
}

impl FormatCategory {
    const ALL: [Self; 8] = [
        Self::Image,
        Self::Document,
        Self::Text,
        Self::Data,
        Self::Audio,
        Self::Video,
        Self::Archive,
        Self::Font,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Image => "Image",
            Self::Document => "Document",
            Self::Text => "Text",
            Self::Data => "Data",
            Self::Audio => "Audio",
            Self::Video => "Video",
            Self::Archive => "Archive",
            Self::Font => "Font",
        }
    }
}

fn format_category(format: OutputFormat) -> FormatCategory {
    match format {
        OutputFormat::Png
        | OutputFormat::Jpeg
        | OutputFormat::Webp
        | OutputFormat::Gif
        | OutputFormat::Bmp
        | OutputFormat::Tiff
        | OutputFormat::Ico
        | OutputFormat::Ppm
        | OutputFormat::Pbm
        | OutputFormat::Pgm
        | OutputFormat::Pam
        | OutputFormat::Tga
        | OutputFormat::Qoi
        | OutputFormat::Hdr
        | OutputFormat::Farbfeld => FormatCategory::Image,
        // A PDF page rendered as SVG is a page you can look at or drop into a
        // layout, so it belongs beside the raster targets rather than with the
        // documents the app reads.
        OutputFormat::SvgPage => FormatCategory::Image,
        OutputFormat::Pdf => FormatCategory::Document,
        OutputFormat::Text
        | OutputFormat::TextUtf8NoBom
        | OutputFormat::TextUtf8Bom
        | OutputFormat::TextUtf16Le
        | OutputFormat::TextUtf16Be
        | OutputFormat::TextUtf32Le
        | OutputFormat::TextUtf32Be => FormatCategory::Text,
        OutputFormat::Csv | OutputFormat::Json | OutputFormat::Yaml | OutputFormat::Toml => {
            FormatCategory::Data
        }
        OutputFormat::Wav
        | OutputFormat::Flac
        | OutputFormat::Aiff
        | OutputFormat::Au
        | OutputFormat::Opus
        | OutputFormat::Mp3 => FormatCategory::Audio,
        OutputFormat::Mp4 | OutputFormat::Webm => FormatCategory::Video,
        OutputFormat::Zip
        | OutputFormat::Tar
        | OutputFormat::TarGz
        | OutputFormat::Gzip
        | OutputFormat::Cbz
        | OutputFormat::Files => FormatCategory::Archive,
        OutputFormat::Ttf
        | OutputFormat::Otf
        | OutputFormat::Woff
        | OutputFormat::Woff2
        | OutputFormat::Svg => FormatCategory::Font,
        OutputFormat::Epub | OutputFormat::Html => FormatCategory::Document,
        OutputFormat::Markdown => FormatCategory::Text,
    }
}

fn format_extension(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Png => "PNG",
        OutputFormat::Jpeg => "JPG",
        OutputFormat::Webp => "WEBP",
        OutputFormat::Gif => "GIF",
        OutputFormat::Bmp => "BMP",
        OutputFormat::Tiff => "TIFF",
        OutputFormat::Ico => "ICO",
        OutputFormat::Ppm => "PPM",
        OutputFormat::Pbm => "PBM",
        OutputFormat::Pgm => "PGM",
        OutputFormat::Pam => "PAM",
        OutputFormat::Tga => "TGA",
        OutputFormat::Qoi => "QOI",
        OutputFormat::Hdr => "HDR",
        OutputFormat::Farbfeld => "FF",
        OutputFormat::Pdf => "PDF",
        OutputFormat::Text => "TXT",
        OutputFormat::TextUtf8NoBom => "UTF8",
        OutputFormat::TextUtf8Bom => "UTF8 BOM",
        OutputFormat::TextUtf16Le => "UTF16LE",
        OutputFormat::TextUtf16Be => "UTF16BE",
        OutputFormat::TextUtf32Le => "UTF32LE",
        OutputFormat::TextUtf32Be => "UTF32BE",
        OutputFormat::Csv => "CSV",
        OutputFormat::Json => "JSON",
        OutputFormat::Yaml => "YAML",
        OutputFormat::Toml => "TOML",
        OutputFormat::Wav => "WAV",
        OutputFormat::Flac => "FLAC",
        OutputFormat::Aiff => "AIFF",
        OutputFormat::Au => "AU",
        OutputFormat::Opus => "OPUS",
        OutputFormat::Mp3 => "MP3",
        OutputFormat::Zip => "ZIP",
        OutputFormat::Tar => "TAR",
        OutputFormat::TarGz => "TAR.GZ",
        OutputFormat::Gzip => "GZ",
        OutputFormat::Cbz => "CBZ",
        OutputFormat::Mp4 => "MP4",
        OutputFormat::Webm => "WEBM",
        OutputFormat::Files => "FILES",
        OutputFormat::Ttf => "TTF",
        OutputFormat::Otf => "OTF",
        OutputFormat::Woff => "WOFF",
        OutputFormat::Woff2 => "WOFF2",
        OutputFormat::Svg => "SVG",
        OutputFormat::SvgPage => "SVG PAGE",
        OutputFormat::Epub => "EPUB",
        OutputFormat::Html => "HTML",
        OutputFormat::Markdown => "MD",
    }
}

fn format_description(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Png => "PNG image",
        OutputFormat::Jpeg => "JPEG image",
        OutputFormat::Webp => "WebP image",
        OutputFormat::Gif => "GIF image",
        OutputFormat::Bmp => "BMP image",
        OutputFormat::Tiff => "TIFF image",
        OutputFormat::Ico => "Windows icon",
        OutputFormat::Ppm => "Portable pixmap image",
        OutputFormat::Pbm => "Portable bitmap image",
        OutputFormat::Pgm => "Portable graymap image",
        OutputFormat::Pam => "Portable anymap image",
        OutputFormat::Tga => "Truevision TGA image",
        OutputFormat::Qoi => "Quite OK Image",
        OutputFormat::Hdr => "Radiance HDR image",
        OutputFormat::Farbfeld => "Farbfeld image",
        OutputFormat::Pdf => "Portable Document Format",
        OutputFormat::Text => "Plain text",
        OutputFormat::TextUtf8NoBom => "Plain text, UTF-8 without BOM",
        OutputFormat::TextUtf8Bom => "Plain text, UTF-8 with BOM",
        OutputFormat::TextUtf16Le => "Plain text, UTF-16 little-endian",
        OutputFormat::TextUtf16Be => "Plain text, UTF-16 big-endian",
        OutputFormat::TextUtf32Le => "Plain text, UTF-32 little-endian",
        OutputFormat::TextUtf32Be => "Plain text, UTF-32 big-endian",
        OutputFormat::Csv => "Comma-separated values",
        OutputFormat::Json => "JavaScript Object Notation",
        OutputFormat::Yaml => "YAML data",
        OutputFormat::Toml => "TOML data",
        OutputFormat::Wav => "WAV audio",
        OutputFormat::Flac => "FLAC audio",
        OutputFormat::Aiff => "AIFF audio",
        OutputFormat::Au => "AU audio",
        OutputFormat::Opus => "Opus audio",
        OutputFormat::Mp3 => "MP3 audio",
        OutputFormat::Zip => "ZIP archive",
        OutputFormat::Tar => "TAR archive",
        OutputFormat::TarGz => "Gzipped TAR archive",
        OutputFormat::Gzip => "GZIP compressed file",
        OutputFormat::Cbz => "Comic book archive (CBZ)",
        OutputFormat::Mp4 => "MP4 video (stream copy)",
        OutputFormat::Webm => "WebM video (stream copy)",
        OutputFormat::Files => "Extract archive into files",
        OutputFormat::Ttf => "TrueType font",
        OutputFormat::Otf => "OpenType font",
        OutputFormat::Woff => "WOFF font",
        OutputFormat::Woff2 => "WOFF2 font",
        OutputFormat::Svg => "SVG font specimen",
        OutputFormat::SvgPage => "SVG page",
        OutputFormat::Epub => "EPUB ebook",
        OutputFormat::Html => "HTML document",
        OutputFormat::Markdown => "Markdown document",
    }
}

const ARCHIVE_TARGETS: [OutputFormat; 5] = [
    OutputFormat::Files,
    OutputFormat::Zip,
    OutputFormat::Tar,
    OutputFormat::TarGz,
    OutputFormat::Gzip,
];

fn available_targets(input_format: Option<InputFormat>) -> Vec<OutputFormat> {
    let Some(input_format) = input_format else {
        return OutputFormat::ALL.to_vec();
    };
    let Some(extension) = input_format.canonical_extension() else {
        return ARCHIVE_TARGETS.to_vec();
    };

    let sample = format!("sample.{extension}");
    OutputFormat::ALL
        .into_iter()
        .filter(|target| target.supports_input(&sample))
        .collect()
}

fn close_icon_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(30.0), egui::Sense::click());
    ui.painter().rect_filled(
        rect,
        egui::CornerRadius::same(7),
        if response.hovered() { SOFT } else { CARD },
    );
    ui.painter().rect_stroke(
        rect,
        egui::CornerRadius::same(7),
        Stroke::new(1.0_f32, BORDER),
        egui::StrokeKind::Inside,
    );
    let color = if response.hovered() { TEXT } else { MUTED };
    let center = rect.center();
    let arm = 5.5;
    let stroke = Stroke::new(1.5_f32, color);
    ui.painter().line_segment(
        [
            egui::pos2(center.x - arm, center.y - arm),
            egui::pos2(center.x + arm, center.y + arm),
        ],
        stroke,
    );
    ui.painter().line_segment(
        [
            egui::pos2(center.x + arm, center.y - arm),
            egui::pos2(center.x - arm, center.y + arm),
        ],
        stroke,
    );
    response
}

fn secondary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(label).size(12.8).color(TEXT))
            .fill(CARD)
            .stroke(Stroke::new(1.0_f32, BORDER))
            .corner_radius(egui::CornerRadius::same(8))
            .min_size(Vec2::new(0.0, 34.0)),
    )
}

fn primary_button(ui: &mut egui::Ui, label: &str, width: f32) -> egui::Response {
    ui.add_sized(
        Vec2::new(width, 36.0),
        egui::Button::new(
            RichText::new(label)
                .size(13.0)
                .strong()
                .color(Color32::WHITE),
        )
        .fill(PRIMARY)
        .stroke(Stroke::NONE)
        .corner_radius(egui::CornerRadius::same(8)),
    )
}

fn target_has_applied_setting(target: OutputFormat, options: &ConversionOptions) -> bool {
    let image_changed = (target.uses_image_background()
        && options.image_background != ConversionOptions::DEFAULT_IMAGE_BACKGROUND)
        || !options.image_apply_exif_orientation;
    match target {
        OutputFormat::Jpeg => {
            options.jpeg_quality.is_some()
                || image_changed
                || options.video_frame_time_ms > 0
                || options.video_thumbnail_width > 0
        }
        OutputFormat::Png => {
            options.png_compression.is_some()
                || image_changed
                || options.video_frame_time_ms > 0
                || options.video_thumbnail_width > 0
        }
        OutputFormat::Zip | OutputFormat::Cbz | OutputFormat::TarGz | OutputFormat::Gzip => {
            options.archive_compression.is_some()
        }
        OutputFormat::Mp4 | OutputFormat::Webm => false,
        OutputFormat::Wav | OutputFormat::Flac => {
            options.audio_sample_rate.is_some()
                || options.audio_channels.is_some()
                || options.audio_track.is_some()
                || options.audio_bit_depth != AudioBitDepth::Bit16
        }
        OutputFormat::Aiff | OutputFormat::Au | OutputFormat::Opus => {
            options.audio_sample_rate.is_some()
                || options.audio_channels.is_some()
                || options.audio_track.is_some()
        }
        OutputFormat::Mp3 => {
            options.audio_sample_rate.is_some()
                || options.audio_channels.is_some()
                || options.audio_track.is_some()
                || options.mp3_bitrate != ConversionOptions::DEFAULT_MP3_BITRATE
        }
        OutputFormat::Pdf => {
            options.pdf_page_size != ConversionOptions::DEFAULT_PDF_PAGE_SIZE
                || options.pdf_orientation != ConversionOptions::DEFAULT_PDF_ORIENTATION
                || options.pdf_image_fit != ConversionOptions::DEFAULT_PDF_IMAGE_FIT
                || options.pdf_font_size != ConversionOptions::DEFAULT_PDF_FONT_SIZE
                || options.pdf_margin != ConversionOptions::DEFAULT_PDF_MARGIN
                || image_changed
        }
        target if format_category(target) == FormatCategory::Image => {
            image_changed || options.video_frame_time_ms > 0 || options.video_thumbnail_width > 0
        }
        _ => false,
    }
}

fn settings_section_heading(ui: &mut egui::Ui, title: &str, note: Option<&str>) {
    ui.label(
        RichText::new(title.to_uppercase())
            .size(11.2)
            .strong()
            .color(MUTED),
    );
    if let Some(note) = note {
        ui.label(RichText::new(note).size(12.0).color(MUTED));
    }
}

fn settings_input_frame(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(SOFT)
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn optional_setting(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<u8>,
    range: std::ops::RangeInclusive<u8>,
    default: u8,
) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(label).size(12.8).strong().color(TEXT));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut automatic = value.is_none();
                if ui
                    .checkbox(
                        &mut automatic,
                        RichText::new("Auto").size(11.5).color(MUTED),
                    )
                    .changed()
                {
                    *value = if automatic { None } else { Some(default) };
                }
            });
        });
        settings_input_frame(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(value) = value.as_mut() {
                    ui.add(egui::DragValue::new(value).range(range).speed(1.0));
                } else {
                    ui.label(RichText::new("Auto").size(13.0).color(MUTED));
                }
            });
        });
    });
}

fn optional_sample_rate(ui: &mut egui::Ui, sample_rate: &mut Option<u32>) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Sample rate").size(12.8).strong().color(TEXT));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut automatic = sample_rate.is_none();
                if ui
                    .checkbox(
                        &mut automatic,
                        RichText::new("Auto").size(11.5).color(MUTED),
                    )
                    .changed()
                {
                    *sample_rate = if automatic {
                        None
                    } else {
                        Some(ConversionOptions::DEFAULT_AUDIO_SAMPLE_RATE)
                    };
                }
            });
        });
        settings_input_frame(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(rate) = sample_rate.as_mut() {
                    ui.add(
                        egui::DragValue::new(rate)
                            .range(
                                ConversionOptions::MIN_AUDIO_SAMPLE_RATE
                                    ..=ConversionOptions::MAX_AUDIO_SAMPLE_RATE,
                            )
                            .speed(100.0),
                    );
                } else {
                    ui.label(RichText::new("Auto").size(13.0).color(MUTED));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("Hz").size(11.0).color(MUTED));
                });
            });
        });
    });
}

fn optional_channels(ui: &mut egui::Ui, channels: &mut Option<AudioChannels>) {
    ui.vertical(|ui| {
        ui.label(RichText::new("Channels").size(12.8).strong().color(TEXT));
        settings_input_frame(ui, |ui| {
            egui::ComboBox::from_id_salt("audio-channels")
                .selected_text((*channels).map(AudioChannels::label).unwrap_or("Auto"))
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    if ui.selectable_label(channels.is_none(), "Auto").clicked() {
                        *channels = None;
                    }
                    for option in [AudioChannels::Mono, AudioChannels::Stereo] {
                        if ui
                            .selectable_label(*channels == Some(option), option.label())
                            .clicked()
                        {
                            *channels = Some(option);
                        }
                    }
                });
        });
    });
}

fn mp3_bitrate_setting(ui: &mut egui::Ui, bitrate: &mut Mp3Bitrate) {
    ui.vertical(|ui| {
        ui.label(RichText::new("Bitrate").size(12.8).strong().color(TEXT));
        ui.label(
            RichText::new("MP3 is lossy; bit depth does not apply.")
                .size(11.5)
                .color(MUTED),
        );
        settings_input_frame(ui, |ui| {
            egui::ComboBox::from_id_salt("mp3-bitrate")
                .selected_text(bitrate.label())
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for option in Mp3Bitrate::ALL {
                        if ui
                            .selectable_label(*bitrate == option, option.label())
                            .clicked()
                        {
                            *bitrate = option;
                        }
                    }
                });
        });
    });
}

const FLAC_AUDIO_BIT_DEPTHS: [AudioBitDepth; 2] = [AudioBitDepth::Bit16, AudioBitDepth::Bit24];

fn audio_bit_depth_choices(target: OutputFormat) -> &'static [AudioBitDepth] {
    if target == OutputFormat::Flac {
        &FLAC_AUDIO_BIT_DEPTHS
    } else {
        &AudioBitDepth::ALL
    }
}

/// A depth chosen for WAV may not be valid after switching the target to FLAC.
/// Fall back to the default instead of leaving an unsupported value selected
/// but hidden from the combo box.
fn normalize_audio_bit_depth(depth: &mut AudioBitDepth, target: OutputFormat) {
    if !audio_bit_depth_choices(target).contains(depth) {
        *depth = AudioBitDepth::Bit16;
    }
}

fn audio_bit_depth_setting(ui: &mut egui::Ui, depth: &mut AudioBitDepth, target: OutputFormat) {
    normalize_audio_bit_depth(depth, target);
    let choices = audio_bit_depth_choices(target);
    ui.vertical(|ui| {
        ui.label(RichText::new("Bit depth").size(12.8).strong().color(TEXT));
        settings_input_frame(ui, |ui| {
            egui::ComboBox::from_id_salt("audio-bit-depth")
                .selected_text(depth.label())
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for option in choices {
                        if ui
                            .selectable_label(*depth == *option, option.label())
                            .clicked()
                        {
                            *depth = *option;
                        }
                    }
                });
        });
    });
}

fn image_settings(ui: &mut egui::Ui, options: &mut ConversionOptions, show_background: bool) {
    if show_background {
        ui.vertical(|ui| {
            ui.label(RichText::new("Background").size(12.8).strong().color(TEXT));
            ui.label(
                RichText::new("Used when transparency is saved to a format without alpha.")
                    .size(11.5)
                    .color(MUTED),
            );
            settings_input_frame(ui, |ui| {
                ui.horizontal(|ui| {
                    egui::color_picker::color_edit_button_srgb(ui, &mut options.image_background);
                    ui.label(
                        RichText::new(format!(
                            "#{:02X}{:02X}{:02X}",
                            options.image_background[0],
                            options.image_background[1],
                            options.image_background[2]
                        ))
                        .size(12.0)
                        .color(MUTED),
                    );
                });
            });
        });
        ui.add_space(14.0);
    }
    ui.checkbox(
        &mut options.image_apply_exif_orientation,
        RichText::new("Apply EXIF orientation")
            .size(12.8)
            .color(TEXT),
    );
}

fn pdf_choice_setting<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    label: &str,
    id: &str,
    selected: &mut T,
    options: &[T],
    option_label: impl Fn(T) -> &'static str,
) {
    ui.vertical(|ui| {
        ui.label(RichText::new(label).size(12.8).strong().color(TEXT));
        settings_input_frame(ui, |ui| {
            egui::ComboBox::from_id_salt(id)
                .selected_text(option_label(*selected))
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for option in options {
                        if ui
                            .selectable_label(*selected == *option, option_label(*option))
                            .clicked()
                        {
                            *selected = *option;
                        }
                    }
                });
        });
    });
}

fn number_setting(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut u32,
    range: std::ops::RangeInclusive<u32>,
    unit: &str,
) {
    ui.vertical(|ui| {
        ui.label(RichText::new(label).size(12.8).strong().color(TEXT));
        settings_input_frame(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(value).range(range).speed(1.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(unit).size(11.0).color(MUTED));
                });
            });
        });
    });
}

fn video_settings(ui: &mut egui::Ui, options: &mut ConversionOptions, target: OutputFormat) {
    settings_section_heading(ui, "Video", None);
    ui.add_space(10.0);
    if target == OutputFormat::Webm {
        ui.label(
            RichText::new(
                "Compatible codecs are stream-copied without re-encoding. Incompatible codec/container pairs are rejected.",
            )
            .size(11.5)
            .color(MUTED),
        );
    } else if target == OutputFormat::Mp4 {
        ui.label(
            RichText::new("Streams are copied into the MP4 container without re-encoding.")
                .size(11.5)
                .color(MUTED),
        );
    } else {
        number_setting(
            ui,
            "Frame time",
            &mut options.video_frame_time_ms,
            0..=ConversionOptions::MAX_VIDEO_FRAME_TIME_MS,
            "ms",
        );
        ui.add_space(14.0);
        number_setting(
            ui,
            "Thumbnail width",
            &mut options.video_thumbnail_width,
            0..=ConversionOptions::MAX_VIDEO_THUMBNAIL_WIDTH,
            "px (0 = source)",
        );
    }
    ui.add_space(10.0);
    ui.label(
        RichText::new("Requires ffmpeg on PATH or CONVERT2IT_FFMPEG.")
            .size(11.0)
            .color(MUTED),
    );
}

fn audio_track_selection_label(options: &ConversionOptions, choices: &[AudioTrackInfo]) -> String {
    options
        .audio_track
        .and_then(|index| choices.get(index))
        .map(AudioTrackInfo::label)
        .unwrap_or_else(|| "Auto (first track)".to_owned())
}

fn audio_track_picker_is_visible(choices: &[AudioTrackInfo]) -> bool {
    choices.len() > 1
}

impl eframe::App for ConvertApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_conversion();
        self.poll_audio_tracks();
        if !self.is_converting() {
            self.collect_dropped_files(context);
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(egui::Margin::ZERO))
            .show(context, |ui| match self.page {
                AppPage::Upload => self.show_upload_page(ui),
                AppPage::Conversion => self.show_conversion_page(ui),
            });

        self.show_home_dialogs(context);
        if !self.is_converting() && context.input(|input| !input.raw.hovered_files.is_empty()) {
            self.show_drop_overlay(context);
        }

        context.request_repaint_after(Duration::from_millis(100));
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let settings = PersistedSettings {
            output_directory: self.output_directory.clone(),
            target: self.target,
            conversion_options: self.conversion_options,
        };
        eframe::set_value(storage, SETTINGS_KEY, &settings);
        storage.flush();
    }
}

impl ConvertApp {
    fn from_storage(storage: Option<&dyn eframe::Storage>) -> Self {
        let mut app = Self::default();
        if let Some(settings) = storage
            .and_then(|storage| eframe::get_value::<PersistedSettings>(storage, SETTINGS_KEY))
        {
            app.output_directory = settings.output_directory.filter(|path| path.is_dir());
            app.conversion_options = settings.conversion_options;
            app.target = settings.target;
        }
        if app.output_directory.is_none() {
            app.output_directory = default_output_directory();
        }
        app
    }

    fn show_upload_page(&mut self, ui: &mut egui::Ui) {
        let viewport = ui.max_rect();
        let body_height = (viewport.height() - HOME_FOOTER_HEIGHT).max(0.0);
        let card_width = (viewport.width() * 0.7)
            .clamp(380.0, 520.0)
            .min(viewport.width() - 32.0);
        let top_space = ((body_height - HOME_CARD_HEIGHT) / 2.0).max(0.0);
        let item_spacing = ui.spacing().item_spacing;
        ui.spacing_mut().item_spacing.y = 0.0;

        ui.add_space(top_space);
        ui.vertical_centered(|ui| {
            let (card_rect, _) = ui.allocate_exact_size(
                Vec2::new(card_width, HOME_CARD_HEIGHT),
                egui::Sense::hover(),
            );
            self.paint_upload_card(ui, card_rect);
        });
        ui.add_space((body_height - top_space - HOME_CARD_HEIGHT).max(0.0));
        self.show_home_footer(ui, viewport.width());
        ui.spacing_mut().item_spacing = item_spacing;
    }

    fn paint_upload_card(&mut self, ui: &mut egui::Ui, card: egui::Rect) {
        let painter = ui.painter();
        painter.rect_filled(card, egui::CornerRadius::same(16), CARD);
        painter.rect_stroke(
            card,
            egui::CornerRadius::same(16),
            Stroke::new(1.0_f32, BORDER),
            egui::StrokeKind::Inside,
        );

        let logo_icon = egui::Rect::from_min_size(
            egui::pos2(card.center().x - 82.06, card.top() + 24.33),
            Vec2::splat(36.0),
        );
        paint_refresh_icon(painter, logo_icon, PRIMARY);
        painter.text(
            egui::pos2(logo_icon.right() + 9.6, logo_icon.center().y),
            egui::Align2::LEFT_CENTER,
            "Convert to it!",
            bold_font(19.2),
            TEXT,
        );

        self.paint_home_dropzone(ui, card);
        self.paint_home_actions(ui, card);
    }

    fn paint_home_dropzone(&mut self, ui: &mut egui::Ui, card: egui::Rect) {
        let dropzone = egui::Rect::from_min_size(
            egui::pos2(card.left() + 21.0, card.top() + 77.0),
            Vec2::new(card.width() - 42.0, 204.36),
        );
        let response = ui.interact(
            dropzone,
            ui.make_persistent_id("upload-dropzone"),
            egui::Sense::click(),
        );
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Click to select a file")
        });
        let hovering_files = ui.input(|input| {
            !input.raw.hovered_files.is_empty()
                && input
                    .pointer
                    .hover_pos()
                    .map(|position| dropzone.contains(position))
                    .unwrap_or(false)
        });
        let hovered = response.hovered() || hovering_files;
        let background = if hovered {
            Color32::from_rgb(47, 47, 47)
        } else {
            SOFT
        };
        ui.painter()
            .rect_filled(dropzone, egui::CornerRadius::same(12), background);
        paint_dashed_border(
            ui.painter(),
            dropzone,
            if hovered { PRIMARY } else { BORDER },
        );

        let icon_center = egui::pos2(dropzone.center().x, dropzone.top() + 73.6);
        ui.painter().circle_filled(icon_center, 33.5, CARD);
        paint_upload_icon(
            ui.painter(),
            egui::Rect::from_center_size(icon_center, Vec2::splat(36.0)),
            PRIMARY,
        );
        ui.painter().text(
            egui::pos2(dropzone.center().x, dropzone.top() + 132.3),
            egui::Align2::CENTER_CENTER,
            "Click to select a file",
            FontId::proportional(15.0),
            PRIMARY,
        );
        ui.painter().text(
            egui::pos2(dropzone.center().x, dropzone.top() + 155.2),
            egui::Align2::CENTER_CENTER,
            "or drag and drop",
            FontId::proportional(13.0),
            MUTED,
        );

        if response.clicked() {
            self.pick_files();
        }
    }

    fn paint_home_actions(&mut self, ui: &mut egui::Ui, card: egui::Rect) {
        let first_x = card.center().x - 66.0;
        let top = card.top() + 298.0;
        let ids = ["settings", "help", "formats"];
        for (index, id) in ids.iter().enumerate() {
            let button = egui::Rect::from_min_size(
                egui::pos2(first_x + index as f32 * 48.0, top),
                Vec2::splat(36.0),
            );
            let tooltip = match *id {
                "settings" => "App settings",
                "help" => "Help & Info",
                _ => "View supported formats",
            };
            let response = ui.interact(
                button,
                ui.make_persistent_id(("upload-action", id)),
                egui::Sense::click(),
            );
            response
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tooltip));
            let fill = if response.hovered() { SOFT } else { CARD };
            ui.painter()
                .rect_filled(button, egui::CornerRadius::same(10), fill);
            ui.painter().rect_stroke(
                button,
                egui::CornerRadius::same(10),
                Stroke::new(1.0_f32, BORDER),
                egui::StrokeKind::Inside,
            );
            match *id {
                "settings" => paint_settings_icon(ui.painter(), button.shrink(9.0), FOOTER_TEXT),
                "help" => paint_help_icon(ui.painter(), button.shrink(9.0), FOOTER_TEXT),
                "formats" => paint_formats_icon(ui.painter(), button.shrink(9.0), FOOTER_TEXT),
                _ => {}
            }

            if response.clicked() {
                match *id {
                    "settings" => self.show_settings = true,
                    "help" => self.show_help = true,
                    "formats" => {
                        self.page = AppPage::Conversion;
                        self.search_filter.clear();
                        self.selected_category = None;
                    }
                    _ => {}
                }
            }
            response.on_hover_text(tooltip);
        }
    }

    fn show_home_footer(&mut self, ui: &mut egui::Ui, width: f32) {
        let (footer, _) =
            ui.allocate_exact_size(Vec2::new(width, HOME_FOOTER_HEIGHT), egui::Sense::hover());
        ui.painter().rect_filled(footer, 0.0, SOFT);
        ui.painter().text(
            egui::pos2(footer.left() + 16.0, footer.center().y),
            egui::Align2::LEFT_CENTER,
            "© 2026, p2r3",
            FontId::proportional(12.0),
            FOOTER_TEXT,
        );

        let painter = ui.painter().clone();
        let source_font = FontId::proportional(12.0);
        let source_text_width = painter
            .layout_no_wrap("Source".to_owned(), source_font.clone(), FOOTER_TEXT)
            .size()
            .x;
        let source_width = 12.0 + 4.8 + source_text_width;
        let source = egui::Rect::from_min_size(
            egui::pos2(footer.right() - 16.0 - source_width, footer.top()),
            Vec2::new(source_width, footer.height()),
        );
        let response = ui.interact(
            source,
            ui.make_persistent_id("source-link"),
            egui::Sense::click(),
        );
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Link, true, "Source"));
        let color = if response.hovered() {
            MUTED
        } else {
            FOOTER_TEXT
        };
        paint_github_icon(
            &painter,
            egui::Rect::from_min_size(
                egui::pos2(source.left(), source.center().y - 6.0),
                Vec2::splat(12.0),
            ),
            color,
        );
        painter.text(
            egui::pos2(source.left() + 16.8, source.center().y),
            egui::Align2::LEFT_CENTER,
            "Source",
            source_font,
            color,
        );
        if response.clicked() {
            if let Err(error) = webbrowser::open("https://github.com/p2r3/convert") {
                self.status = Some((format!("Couldn't open source link: {error}"), true));
            }
        }
    }

    fn show_home_dialogs(&mut self, context: &egui::Context) {
        self.show_app_settings(context);
        self.show_input_format_dialog(context);
        self.show_help_modal(context);
        self.show_conversion_notice(context);
        self.show_conversion_progress(context);
    }

    fn show_app_settings(&mut self, context: &egui::Context) {
        if !self.show_settings {
            return;
        }

        let screen = context.screen_rect();
        let modal_width = (screen.width() - 96.0).clamp(320.0, 640.0);
        let modal_height = (screen.height() - 96.0).clamp(360.0, 536.0);
        let current_folder = self.output_directory.clone();
        let mut choose_folder = false;
        let mut dismiss = false;

        let modal = egui::Modal::new(egui::Id::new("app-settings-modal"))
            .frame(
                egui::Frame::new()
                    .fill(CARD)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .corner_radius(egui::CornerRadius::same(14))
                    .inner_margin(egui::Margin::same(28)),
            )
            .show(context, |ui| {
                ui.set_width(modal_width);
                ui.set_min_height(modal_height);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("App settings")
                            .size(19.2)
                            .strong()
                            .color(TEXT),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if close_icon_button(ui).clicked() {
                            dismiss = true;
                        }
                    });
                });
                ui.add_space(16.0);
                ui.label(RichText::new("GENERAL").size(11.2).strong().color(MUTED));
                ui.add_space(6.0);

                egui::Frame::new()
                    .fill(SOFT)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .corner_radius(egui::CornerRadius::same(10))
                    .inner_margin(egui::Margin::symmetric(16, 14))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new("Output folder")
                                        .size(14.4)
                                        .strong()
                                        .color(TEXT),
                                );
                                ui.label(
                                    RichText::new("Where converted files are saved.")
                                        .size(12.8)
                                        .color(MUTED),
                                );
                                ui.label(
                                    RichText::new(
                                        current_folder
                                            .as_deref()
                                            .map(|path| path.display().to_string())
                                            .unwrap_or_else(|| "No folder selected".to_owned()),
                                    )
                                    .size(12.0)
                                    .color(MUTED),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if secondary_button(ui, "Choose folder").clicked() {
                                        choose_folder = true;
                                    }
                                },
                            );
                        });
                    });

                let used = ui.min_rect().height();
                ui.add_space((modal_height - used - 44.0).max(0.0));
                ui.vertical_centered(|ui| {
                    if primary_button(ui, "Done", 180.0).clicked() {
                        dismiss = true;
                    }
                });
            });

        if choose_folder {
            let mut dialog = FileDialog::new();
            if let Some(directory) = self.output_directory.as_deref() {
                dialog = dialog.set_directory(directory);
            }
            if let Some(directory) = dialog.pick_folder() {
                self.output_directory = Some(directory);
            }
        }

        if dismiss || modal.should_close() {
            self.show_settings = false;
        }
    }

    fn show_conversion_settings_panel(&mut self, ui: &mut egui::Ui, width: f32, height: f32) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), egui::Sense::hover());
        ui.painter().rect_filled(rect, 0.0, CARD);
        ui.painter().line_segment(
            [rect.left_top(), rect.left_bottom()],
            Stroke::new(1.0_f32, BORDER),
        );

        let mut panel = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        panel.set_clip_rect(rect);
        panel.spacing_mut().item_spacing = Vec2::ZERO;

        panel.add_space(12.0);
        panel.horizontal(|ui| {
            ui.add_space(20.0);
            ui.label(
                RichText::new("Conversion settings")
                    .size(15.2)
                    .strong()
                    .color(TEXT),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(12.0);
                if close_icon_button(ui).clicked() {
                    self.show_conversion_settings = false;
                }
                if self.target.is_some_and(|target| {
                    target_has_applied_setting(target, &self.conversion_options)
                }) {
                    ui.add_space(2.0);
                    if ui
                        .add(
                            egui::Button::new(RichText::new("Reset").size(12.8).color(PRIMARY))
                                .frame(false),
                        )
                        .clicked()
                    {
                        self.conversion_options = ConversionOptions::default();
                    }
                }
            });
        });
        panel.add_space(10.0);

        egui::ScrollArea::vertical()
            .id_salt("conversion-settings-scroll")
            .max_height((height - 56.0).max(0.0))
            .auto_shrink([false, true])
            .show(&mut panel, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    ui.vertical(|ui| {
                        ui.set_width((width - 40.0).max(0.0));
                        self.conversion_settings_body(ui);
                    });
                });
                ui.add_space(20.0);
            });

        if panel
            .ctx()
            .input(|input| input.key_pressed(egui::Key::Escape))
        {
            self.show_conversion_settings = false;
        }
    }

    fn conversion_settings_body(&mut self, ui: &mut egui::Ui) {
        let Some(target) = self.target else {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("Pick an output format to see its settings.")
                        .size(12.8)
                        .color(MUTED),
                );
            });
            return;
        };

        match target {
            OutputFormat::Jpeg => {
                if self.effective_input_format() == Some(InputFormat::Video) {
                    video_settings(ui, &mut self.conversion_options, OutputFormat::Jpeg);
                    ui.add_space(18.0);
                }
                settings_section_heading(
                    ui,
                    "Quality",
                    Some("1 is the smallest file, 100 the best quality."),
                );
                ui.add_space(10.0);
                optional_setting(
                    ui,
                    "Quality",
                    &mut self.conversion_options.jpeg_quality,
                    1..=100,
                    ConversionOptions::DEFAULT_JPEG_QUALITY,
                );
                ui.add_space(18.0);
                settings_section_heading(ui, "Image", None);
                ui.add_space(10.0);
                image_settings(ui, &mut self.conversion_options, true);
            }
            OutputFormat::Png => {
                if self.effective_input_format() == Some(InputFormat::Video) {
                    video_settings(ui, &mut self.conversion_options, OutputFormat::Png);
                    ui.add_space(18.0);
                }
                settings_section_heading(ui, "Other", None);
                ui.add_space(10.0);
                optional_setting(
                    ui,
                    "Compression level",
                    &mut self.conversion_options.png_compression,
                    1..=9,
                    ConversionOptions::DEFAULT_COMPRESSION_LEVEL,
                );
                ui.add_space(18.0);
                settings_section_heading(ui, "Image", None);
                ui.add_space(10.0);
                image_settings(ui, &mut self.conversion_options, false);
            }
            OutputFormat::Zip | OutputFormat::Cbz | OutputFormat::TarGz | OutputFormat::Gzip => {
                settings_section_heading(ui, "Other", None);
                ui.add_space(10.0);
                optional_setting(
                    ui,
                    "Compression level",
                    &mut self.conversion_options.archive_compression,
                    1..=9,
                    ConversionOptions::DEFAULT_COMPRESSION_LEVEL,
                );
            }
            OutputFormat::Wav
            | OutputFormat::Flac
            | OutputFormat::Aiff
            | OutputFormat::Au
            | OutputFormat::Opus
            | OutputFormat::Mp3 => {
                settings_section_heading(ui, "Audio", None);
                ui.add_space(10.0);
                self.audio_track_setting(ui);
                ui.add_space(14.0);
                optional_sample_rate(ui, &mut self.conversion_options.audio_sample_rate);
                ui.add_space(14.0);
                optional_channels(ui, &mut self.conversion_options.audio_channels);
                if target == OutputFormat::Mp3 {
                    ui.add_space(14.0);
                    mp3_bitrate_setting(ui, &mut self.conversion_options.mp3_bitrate);
                }
                if matches!(target, OutputFormat::Wav | OutputFormat::Flac) {
                    ui.add_space(14.0);
                    audio_bit_depth_setting(
                        ui,
                        &mut self.conversion_options.audio_bit_depth,
                        target,
                    );
                }
            }
            OutputFormat::Mp4 | OutputFormat::Webm => {
                video_settings(ui, &mut self.conversion_options, target);
            }
            OutputFormat::Pdf => {
                settings_section_heading(ui, "Page", None);
                ui.add_space(10.0);
                pdf_choice_setting(
                    ui,
                    "Page size",
                    "pdf-page-size",
                    &mut self.conversion_options.pdf_page_size,
                    &PdfPageSize::ALL,
                    PdfPageSize::label,
                );
                if self
                    .effective_input_format()
                    .is_some_and(InputFormat::is_image)
                {
                    ui.add_space(14.0);
                    pdf_choice_setting(
                        ui,
                        "Orientation",
                        "pdf-orientation",
                        &mut self.conversion_options.pdf_orientation,
                        &PdfOrientation::ALL,
                        PdfOrientation::label,
                    );
                    ui.add_space(14.0);
                    pdf_choice_setting(
                        ui,
                        "Image fit",
                        "pdf-image-fit",
                        &mut self.conversion_options.pdf_image_fit,
                        &PdfImageFit::ALL,
                        PdfImageFit::label,
                    );
                } else {
                    ui.add_space(14.0);
                    number_setting(
                        ui,
                        "Font size",
                        &mut self.conversion_options.pdf_font_size,
                        ConversionOptions::MIN_PDF_FONT_SIZE..=ConversionOptions::MAX_PDF_FONT_SIZE,
                        "pt",
                    );
                    ui.add_space(14.0);
                    number_setting(
                        ui,
                        "Margin",
                        &mut self.conversion_options.pdf_margin,
                        ConversionOptions::MIN_PDF_MARGIN..=ConversionOptions::MAX_PDF_MARGIN,
                        "pt",
                    );
                }
                if self
                    .effective_input_format()
                    .is_some_and(InputFormat::is_image)
                {
                    ui.add_space(18.0);
                    settings_section_heading(ui, "Image", None);
                    ui.add_space(10.0);
                    image_settings(ui, &mut self.conversion_options, true);
                }
            }
            _ if format_category(target) == FormatCategory::Image => {
                if self.effective_input_format() == Some(InputFormat::Video) {
                    video_settings(ui, &mut self.conversion_options, target);
                    ui.add_space(18.0);
                }
                settings_section_heading(ui, "Image", None);
                ui.add_space(10.0);
                image_settings(
                    ui,
                    &mut self.conversion_options,
                    target.uses_image_background(),
                );
            }
            _ => {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} doesn't have any settings.",
                            format_extension(target)
                        ))
                        .size(12.8)
                        .color(MUTED),
                    );
                });
            }
        }
    }

    fn audio_track_setting(&mut self, ui: &mut egui::Ui) {
        let choices = self.audio_track_choices.clone();
        if !choices.is_empty() && !audio_track_picker_is_visible(&choices) {
            return;
        }
        ui.vertical(|ui| {
            ui.label(RichText::new("Audio track").size(12.8).strong().color(TEXT));
            if choices.is_empty() {
                ui.label(
                    RichText::new("Track information is unavailable for this file.")
                        .size(11.5)
                        .color(MUTED),
                );
                return;
            }
            ui.label(
                RichText::new("Read from the first selected file.")
                    .size(11.0)
                    .color(MUTED),
            );
            settings_input_frame(ui, |ui| {
                let selected = audio_track_selection_label(&self.conversion_options, &choices);
                egui::ComboBox::from_id_salt("audio-track")
                    .selected_text(selected)
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(
                                self.conversion_options.audio_track.is_none(),
                                "Auto (first track)",
                            )
                            .clicked()
                        {
                            self.conversion_options.audio_track = None;
                        }
                        for (index, track) in choices.iter().enumerate() {
                            if ui
                                .selectable_label(
                                    self.conversion_options.audio_track == Some(index),
                                    track.label(),
                                )
                                .clicked()
                            {
                                self.conversion_options.audio_track = Some(index);
                            }
                        }
                    });
            });
        });
        if self.audio_tracks_loading {
            ui.label(
                RichText::new("Reading audio tracks...")
                    .size(11.0)
                    .color(MUTED),
            );
        } else if let Some(error) = &self.audio_track_error {
            ui.label(RichText::new(error).size(11.0).color(MUTED));
        }
    }

    fn refresh_audio_tracks(&mut self) {
        self.audio_track_generation = self.audio_track_generation.wrapping_add(1);
        let generation = self.audio_track_generation;
        self.audio_track_choices.clear();
        self.audio_track_error = None;
        self.audio_track_request = None;
        let first = self.inputs.first().cloned();
        if first != self.audio_track_source {
            self.audio_track_source = first.clone();
            self.conversion_options.audio_track = None;
        }
        let Some(path) = first else {
            self.audio_tracks_loading = false;
            return;
        };
        self.audio_tracks_loading = true;
        let (sender, receiver) = mpsc::channel();
        self.audio_track_request = Some(receiver);
        thread::spawn(move || {
            let result = audio_tracks_from_path(&path).map_err(|error| error.to_string());
            let _ = sender.send((generation, result));
        });
    }

    fn poll_audio_tracks(&mut self) {
        let received = match self.audio_track_request.as_ref() {
            Some(receiver) => receiver.try_recv(),
            None => return,
        };
        match received {
            Ok((generation, result)) => {
                if generation != self.audio_track_generation {
                    return;
                }
                self.audio_track_request = None;
                self.audio_tracks_loading = false;
                match result {
                    Ok(tracks) => {
                        self.audio_track_choices = tracks;
                        if self
                            .conversion_options
                            .audio_track
                            .is_some_and(|index| index >= self.audio_track_choices.len())
                        {
                            self.conversion_options.audio_track = None;
                        }
                    }
                    Err(error) => self.audio_track_error = Some(error),
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.audio_track_request = None;
                self.audio_tracks_loading = false;
            }
        }
    }

    fn effective_input_format(&self) -> Option<InputFormat> {
        if self.inputs.is_empty() {
            return None;
        }
        Some(
            self.input_format_override
                .unwrap_or_else(|| common_format(&self.input_formats)),
        )
    }

    fn detected_input_format(&self) -> Option<InputFormat> {
        if self.inputs.is_empty() {
            return None;
        }
        let detected = common_format(&self.input_formats);
        (!matches!(detected, InputFormat::Any | InputFormat::Mixed)).then_some(detected)
    }

    fn open_input_format_picker(&mut self) {
        self.input_format_search.clear();
        self.input_format_show_all = self.input_format_override.is_some();
        self.show_input_format_picker = true;
    }

    fn input_format_choices(&self) -> Vec<InputFormat> {
        let detected = self.detected_input_format();
        if self.input_format_show_all || detected.is_none() {
            InputFormat::SELECTABLE.to_vec()
        } else {
            detected.into_iter().collect()
        }
    }

    fn detected_input_label(&self) -> &'static str {
        match common_format(&self.input_formats) {
            InputFormat::Any => "Unknown format",
            InputFormat::Mixed => InputFormat::Mixed.label(),
            format => format.label(),
        }
    }

    fn choose_input_format(&mut self, format: InputFormat) {
        self.input_format_override = if self.effective_input_format() == Some(format) {
            None
        } else {
            Some(format)
        };
        self.target = None;
        self.show_input_format_picker = false;
    }

    fn show_input_format_dialog(&mut self, context: &egui::Context) {
        if !self.show_input_format_picker {
            return;
        }

        let detected = self.detected_input_format();
        let detected_label = self.detected_input_label();
        let effective = self.effective_input_format();
        let current_override = self.input_format_override;
        let using = current_override.or(match effective {
            Some(InputFormat::Any | InputFormat::Mixed) => effective,
            _ => None,
        });
        let mut open = true;
        let mut selected = None;

        egui::Window::new("Input format")
            .collapsible(false)
            .resizable(false)
            .default_width(560.0)
            .open(&mut open)
            .show(context, |ui| {
                ui.label(format!("Detected: {detected_label}"));
                if let Some(format) = using {
                    ui.label(format!("Using: {}", format.label()));
                }
                ui.horizontal(|ui| {
                    if current_override.is_none() && detected.is_some() {
                        if self.input_format_show_all {
                            if ui.button("Show matching only").clicked() {
                                self.input_format_show_all = false;
                            }
                        } else if ui.button("Show all input formats").clicked() {
                            self.input_format_show_all = true;
                        }
                    }
                    if current_override.is_some() && ui.button("Auto-detect").clicked() {
                        self.input_format_override = None;
                        self.target = None;
                    }
                });

                ui.add(
                    egui::TextEdit::singleline(&mut self.input_format_search)
                        .hint_text("Search input formats..."),
                );
                ui.add_space(6.0);

                let term = self.input_format_search.trim().to_ascii_lowercase();
                let formats: Vec<_> = self
                    .input_format_choices()
                    .into_iter()
                    .filter(|format| format.matches_search(&term))
                    .collect();

                if formats.is_empty() {
                    ui.label(RichText::new("No matching input formats.").color(MUTED));
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(360.0)
                        .show(ui, |ui| {
                            egui::Grid::new("input-format-grid")
                                .num_columns(2)
                                .spacing([12.0, 6.0])
                                .show(ui, |ui| {
                                    for format in formats {
                                        let selected_format = self.input_format_override
                                            == Some(format)
                                            || (self.input_format_override.is_none()
                                                && effective == Some(format));
                                        if ui
                                            .selectable_label(selected_format, format.label())
                                            .clicked()
                                        {
                                            selected = Some(format);
                                        }
                                        ui.label(
                                            RichText::new(format.description())
                                                .size(11.0)
                                                .color(FOOTER_TEXT),
                                        );
                                        ui.end_row();
                                    }
                                });
                        });
                }
            });

        self.show_input_format_picker = open;
        if let Some(format) = selected {
            self.choose_input_format(format);
        }
    }

    fn show_help_modal(&mut self, context: &egui::Context) {
        if !self.show_help {
            return;
        }

        let mut dismiss = false;
        let modal = egui::Modal::new(egui::Id::new("help-modal"))
            .frame(
                egui::Frame::new()
                    .fill(CARD)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .corner_radius(egui::CornerRadius::same(14))
                    .inner_margin(egui::Margin::same(28)),
            )
            .show(context, |ui| {
                ui.set_max_width(512.0);
                ui.heading("Convert to it!");
                ui.label(
                    RichText::new("A local private file converter")
                        .strong()
                        .color(PRIMARY),
                );
                ui.add_space(8.0);

                egui::ScrollArea::vertical()
                    .max_height(context.screen_rect().height() * 0.58)
                    .show(ui, |ui| {
                        ui.heading("Why use this?");
                        ui.label(
                            "Your files stay on this device. They are never uploaded to a server.",
                        );
                        ui.label(
                            "Convert supported image, text, data, and archive formats locally.",
                        );
                        ui.add_space(8.0);
                        ui.heading("How to use");
                        ui.label("1. Select one or more files, or drag them onto the upload area.");
                        ui.label("2. Choose an output format.");
                        ui.label("3. Click Convert. Files are saved to your output folder.");
                        ui.add_space(8.0);
                        ui.hyperlink_to(
                            "Watch the more in-depth video.",
                            "https://youtu.be/btUbcsTbVA8",
                        );
                    });

                ui.add_space(12.0);
                ui.vertical_centered(|ui| {
                    if ui.button("Got it").clicked() {
                        dismiss = true;
                    }
                });
            });
        if dismiss || modal.should_close() {
            self.show_help = false;
        }
    }

    fn show_conversion_notice(&mut self, context: &egui::Context) {
        let Some((title, message, is_error)) = self.conversion_notice.clone() else {
            return;
        };
        let mut dismiss = false;
        let modal = egui::Modal::new(egui::Id::new("conversion-notice-modal"))
            .frame(
                egui::Frame::new()
                    .fill(CARD)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .corner_radius(egui::CornerRadius::same(14))
                    .inner_margin(egui::Margin::same(28)),
            )
            .show(context, |ui| {
                ui.set_max_width(512.0);
                ui.heading(&title);
                ui.label(RichText::new(message).size(13.6).color(if is_error {
                    RED
                } else {
                    MUTED
                }));
                ui.add_space(12.0);
                ui.vertical_centered(|ui| {
                    if ui.button("OK").clicked() {
                        dismiss = true;
                    }
                });
            });
        if dismiss || modal.should_close() {
            self.conversion_notice = None;
        }
    }

    fn show_conversion_page(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        let height = ui.available_height();
        let main_height = (height - 56.0 - ACTION_BAR_HEIGHT - HOME_FOOTER_HEIGHT).max(0.0);
        let item_spacing = ui.spacing().item_spacing;
        ui.spacing_mut().item_spacing.y = 0.0;

        self.show_conversion_header(ui, width);
        let panel_width = if self.show_conversion_settings {
            if width <= 640.0 {
                width
            } else {
                320.0
            }
        } else {
            0.0
        };
        let content_width = (width - panel_width).max(0.0);
        if self.show_conversion_settings {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if content_width > 0.0 {
                    self.show_format_explorer(ui, content_width, main_height);
                }
                self.show_conversion_settings_panel(ui, panel_width, main_height);
            });
        } else {
            self.show_format_explorer(ui, width, main_height);
        }
        self.show_action_bar(ui, width);
        self.show_home_footer(ui, width);
        ui.spacing_mut().item_spacing = item_spacing;
    }

    fn show_conversion_header(&mut self, ui: &mut egui::Ui, width: f32) {
        let (header, _) = ui.allocate_exact_size(Vec2::new(width, 56.0), egui::Sense::empty());
        let painter = ui.painter();
        painter.rect_filled(header, 0.0, CARD);
        painter.line_segment(
            [header.left_bottom(), header.right_bottom()],
            Stroke::new(1.0_f32, BORDER),
        );

        let logo = egui::Rect::from_min_size(
            egui::pos2(header.left() + 20.0, header.center().y - 12.0),
            Vec2::splat(24.0),
        );
        paint_refresh_icon(painter, logo, PRIMARY);
        painter.text(
            egui::pos2(logo.right() + 10.0, header.center().y),
            egui::Align2::LEFT_CENTER,
            "Convert to it!",
            bold_font(19.2),
            TEXT,
        );
        let title_width = painter
            .layout_no_wrap("Convert to it!".to_owned(), bold_font(19.2), TEXT)
            .size()
            .x;
        let title = egui::Rect::from_min_max(
            egui::pos2(logo.left(), header.top()),
            egui::pos2(logo.right() + 10.0 + title_width, header.bottom()),
        );
        let logo_response = ui.interact(
            title,
            ui.make_persistent_id("back-to-upload-logo"),
            egui::Sense::click(),
        );
        logo_response.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                !self.is_converting(),
                "Back to upload",
            )
        });
        logo_response.clone().on_hover_text("Back to upload");
        if logo_response.clicked() && !self.is_converting() {
            self.return_to_upload();
        }

        for (index, id) in ["settings", "conversion-settings"].iter().enumerate() {
            let button = egui::Rect::from_min_size(
                egui::pos2(
                    header.right() - 20.0 - 36.0 - (1.0 - index as f32) * 48.0,
                    header.center().y - 18.0,
                ),
                Vec2::splat(36.0),
            );
            let response = ui.interact(
                button,
                ui.make_persistent_id(("conversion-action", id)),
                egui::Sense::click(),
            );
            let label = if *id == "settings" {
                "App settings"
            } else {
                "Conversion settings"
            };
            response
                .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
            let active = *id == "conversion-settings" && self.show_conversion_settings;
            painter.rect_filled(
                button,
                egui::CornerRadius::same(8),
                if active {
                    Color32::from_rgb(32, 42, 51)
                } else if response.hovered() {
                    SOFT
                } else {
                    CARD
                },
            );
            painter.rect_stroke(
                button,
                egui::CornerRadius::same(8),
                Stroke::new(
                    1.0_f32,
                    if active {
                        Color32::from_rgb(33, 62, 90)
                    } else {
                        BORDER
                    },
                ),
                egui::StrokeKind::Inside,
            );
            let icon_color = if active { PRIMARY } else { MUTED };
            match *id {
                "settings" => paint_settings_icon(painter, button.shrink(9.0), icon_color),
                "conversion-settings" => {
                    paint_sliders_icon(painter, button.shrink(9.0), icon_color);
                }
                _ => {}
            }
            if response.clicked() {
                match *id {
                    "settings" => self.show_settings = true,
                    "conversion-settings" => {
                        self.show_conversion_settings = !self.show_conversion_settings;
                    }
                    _ => {}
                }
            }
            response.on_hover_text(label);
        }
    }

    fn show_format_explorer(&mut self, ui: &mut egui::Ui, width: f32, height: f32) {
        ui.allocate_ui_with_layout(
            Vec2::new(width, height),
            Layout::top_down(Align::Min),
            |ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                let input_format = self.effective_input_format();
                let targets = available_targets(input_format);
                ui.add_space(20.0);

                let source_format = input_format.unwrap_or(InputFormat::Any);
                let source_label = if self.inputs.is_empty() {
                    "ANY FILE".to_owned()
                } else {
                    source_format.extension_label().to_owned()
                };
                let source_is_image = source_format.is_image();
                let row_height = 51.0;
                let source_width = 148.0;
                let output_width = 190.0;
                let gap = 12.0;
                let arrow_width = 24.0;
                let row_width = source_width + gap + arrow_width + gap + output_width;
                let (row, _) =
                    ui.allocate_exact_size(Vec2::new(width, row_height), egui::Sense::hover());
                let source = egui::Rect::from_min_size(
                    egui::pos2(row.center().x - row_width * 0.5, row.top()),
                    Vec2::new(source_width, row_height),
                );
                let output = egui::Rect::from_min_size(
                    egui::pos2(source.right() + gap + arrow_width + gap, row.top()),
                    Vec2::new(output_width, row_height),
                );
                let painter = ui.painter();
                painter.rect_filled(source, egui::CornerRadius::same(8), SOFT);
                let source_icon = egui::Rect::from_center_size(
                    egui::pos2(source.left() + 28.0, source.center().y),
                    Vec2::splat(18.0),
                );
                if source_is_image {
                    paint_image_icon(painter, source_icon, GREEN);
                } else {
                    paint_file_icon(painter, source_icon, GREEN);
                }
                painter.text(
                    egui::pos2(source.left() + 48.0, source.center().y),
                    egui::Align2::LEFT_CENTER,
                    &source_label,
                    bold_font(14.0),
                    TEXT,
                );
                if !self.inputs.is_empty() && !self.is_converting() {
                    let chevron = egui::pos2(source.right() - 14.0, source.center().y);
                    painter.add(egui::Shape::convex_polygon(
                        vec![
                            egui::pos2(chevron.x - 3.5, chevron.y - 2.0),
                            egui::pos2(chevron.x + 3.5, chevron.y - 2.0),
                            egui::pos2(chevron.x, chevron.y + 2.0),
                        ],
                        MUTED,
                        Stroke::NONE,
                    ));
                    let source_response = ui.interact(
                        source,
                        ui.make_persistent_id("input-format-source"),
                        egui::Sense::click(),
                    );
                    source_response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::ComboBox,
                            true,
                            format!("Input format: {source_label}"),
                        )
                    });
                    if source_response.hovered() {
                        painter.rect_stroke(
                            source,
                            egui::CornerRadius::same(8),
                            Stroke::new(1.0_f32, PRIMARY),
                            egui::StrokeKind::Inside,
                        );
                    }
                    if source_response.clicked() {
                        self.open_input_format_picker();
                    }
                    let _ = source_response.on_hover_text("Change input format");
                }
                paint_arrow_right(
                    painter,
                    egui::pos2(source.right() + gap + arrow_width * 0.5, row.center().y),
                    MUTED,
                );
                painter.rect_filled(output, egui::CornerRadius::same(8), SOFT);
                paint_dashed_border_with_radius(painter, output, BORDER, 8.0);
                painter.text(
                    output.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{} output formats", targets.len()),
                    FontId::proportional(14.0),
                    MUTED,
                );

                ui.add_space(20.0);
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    let search_margin = Vec2::new(38.4, 9.6);
                    let search_response = ui
                        .scope(|ui| {
                            let widgets = &mut ui.style_mut().visuals.widgets;
                            widgets.inactive.bg_fill = SOFT;
                            widgets.inactive.bg_stroke = Stroke::new(1.0_f32, Color32::TRANSPARENT);
                            widgets.inactive.corner_radius = egui::CornerRadius::same(6);
                            widgets.hovered.bg_fill = SOFT;
                            widgets.hovered.bg_stroke = Stroke::new(1.0_f32, Color32::TRANSPARENT);
                            widgets.hovered.corner_radius = egui::CornerRadius::same(6);
                            widgets.active.bg_fill = SOFT;
                            widgets.active.bg_stroke = Stroke::new(1.0_f32, PRIMARY);
                            widgets.active.corner_radius = egui::CornerRadius::same(6);

                            ui.add_sized(
                                Vec2::new((width - 40.0).max(0.0), 38.2),
                                egui::TextEdit::singleline(&mut self.search_filter)
                                    .font(FontId::proportional(14.0))
                                    .margin(search_margin)
                                    .hint_text(
                                        RichText::new("Search formats...")
                                            .size(14.0)
                                            .color(FOOTER_TEXT),
                                    ),
                            )
                        })
                        .inner;
                    let search_rect = search_response.rect.expand2(search_margin);
                    search_response.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::TextEdit,
                            true,
                            "Search formats",
                        )
                    });
                    paint_search_icon(ui.painter(), search_rect, FOOTER_TEXT);
                    if search_response.has_focus() {
                        ui.painter().rect_stroke(
                            search_rect.expand(1.0),
                            egui::CornerRadius::same(7),
                            Stroke::new(2.0_f32, Color32::from_rgba_unmultiplied(35, 131, 226, 38)),
                            egui::StrokeKind::Outside,
                        );
                    }
                });

                ui.add_space(12.0);
                let chip_gap = 6.4;
                let chip_height = 24.0;
                let available = (width - 40.0).max(0.0);
                let painter = ui.painter().clone();
                let mut chip_rows: Vec<Vec<(usize, FormatCategory, f32)>> = Vec::new();
                let mut row: Vec<(usize, FormatCategory, f32)> = Vec::new();
                let mut row_width = 0.0;
                for (index, category) in FormatCategory::ALL.into_iter().enumerate() {
                    let text_width = painter
                        .layout_no_wrap(
                            category.label().to_owned(),
                            FontId::proportional(12.0),
                            MUTED,
                        )
                        .size()
                        .x;
                    let chip_width = 2.0 * 9.6 + 14.0 + 5.6 + text_width + 2.0;
                    if !row.is_empty() && row_width + chip_gap + chip_width > available {
                        chip_rows.push(std::mem::take(&mut row));
                        row_width = 0.0;
                    }
                    row_width += if row.is_empty() {
                        chip_width
                    } else {
                        chip_gap + chip_width
                    };
                    row.push((index, category, chip_width));
                }
                if !row.is_empty() {
                    chip_rows.push(row);
                }
                let row_gap = 6.0;
                let filter_height = chip_rows.len() as f32 * chip_height
                    + chip_rows.len().saturating_sub(1) as f32 * row_gap;
                let (filter_area, _) =
                    ui.allocate_exact_size(Vec2::new(width, filter_height), egui::Sense::hover());
                for (row_index, row) in chip_rows.iter().enumerate() {
                    let top = filter_area.top() + row_index as f32 * (chip_height + row_gap);
                    let mut chip_x = filter_area.left() + 20.0;
                    for (index, category, chip_width) in row {
                        let selected = self.selected_category == Some(*category);
                        let label = category.label();
                        let text_color = if selected { Color32::WHITE } else { MUTED };
                        let chip = egui::Rect::from_min_size(
                            egui::pos2(chip_x, top),
                            Vec2::new(*chip_width, chip_height),
                        );
                        let response = ui.interact(
                            chip,
                            ui.make_persistent_id(("format-category-chip", *index)),
                            egui::Sense::click(),
                        );
                        response.widget_info(|| {
                            egui::WidgetInfo::selected(
                                egui::WidgetType::Checkbox,
                                true,
                                selected,
                                label,
                            )
                        });
                        painter.rect_filled(
                            chip,
                            egui::CornerRadius::same(6),
                            if selected {
                                PRIMARY
                            } else if response.hovered() {
                                Color32::from_rgb(45, 45, 45)
                            } else {
                                SOFT
                            },
                        );
                        if selected {
                            painter.rect_stroke(
                                chip,
                                egui::CornerRadius::same(6),
                                Stroke::new(1.0_f32, PRIMARY),
                                egui::StrokeKind::Inside,
                            );
                        }
                        paint_category_icon(
                            &painter,
                            egui::Rect::from_center_size(
                                egui::pos2(chip.left() + 16.6, chip.center().y),
                                Vec2::splat(14.0),
                            ),
                            *category,
                            if selected {
                                Color32::WHITE
                            } else {
                                FOOTER_TEXT
                            },
                        );
                        painter.text(
                            egui::pos2(chip.left() + 29.2, chip.center().y),
                            egui::Align2::LEFT_CENTER,
                            label,
                            FontId::proportional(12.0),
                            text_color,
                        );
                        if response.clicked() {
                            self.selected_category = if selected { None } else { Some(*category) };
                        }
                        chip_x = chip.right() + chip_gap;
                    }
                }
                ui.add_space(20.0);

                let search = self.search_filter.trim().to_ascii_lowercase();
                let filtered: Vec<_> = targets
                    .into_iter()
                    .filter(|format| {
                        self.selected_category
                            .map(|category| format_category(*format) == category)
                            .unwrap_or(true)
                            && (search.is_empty()
                                || format_extension(*format)
                                    .to_ascii_lowercase()
                                    .contains(&search)
                                || format.label().to_ascii_lowercase().contains(&search)
                                || format_description(*format)
                                    .to_ascii_lowercase()
                                    .contains(&search))
                    })
                    .collect();
                if let Some((message, is_error)) = &self.status {
                    let color = if *is_error { RED } else { GREEN };
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.add_space(20.0);
                        ui.label(RichText::new(message).size(12.0).color(color));
                    });
                }
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    ui.label(
                        RichText::new(format!("{} OUTPUT FORMATS", filtered.len()))
                            .size(11.0)
                            .strong()
                            .color(FOOTER_TEXT),
                    );
                });

                egui::ScrollArea::vertical()
                    .id_salt("native-format-list")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = Vec2::ZERO;
                        ui.add_space(10.4);
                        if filtered.is_empty() {
                            ui.add_space(36.0);
                            ui.vertical_centered(|ui| {
                                ui.label(RichText::new("No matching formats found").color(MUTED));
                            });
                        } else {
                            let columns = if width <= 600.0 { 2 } else { 3 };
                            let gap = 8.0;
                            let cell_width = ((width
                                - 40.0
                                - FORMAT_LIST_SCROLLBAR_GUTTER
                                - gap * (columns - 1) as f32)
                                / columns as f32)
                                .max(120.0);
                            for (row_index, row) in filtered.chunks(columns).enumerate() {
                                if row_index > 0 {
                                    ui.add_space(gap);
                                }
                                ui.horizontal(|ui| {
                                    ui.add_space(20.0);
                                    for (index, format) in row.iter().enumerate() {
                                        if index > 0 {
                                            ui.add_space(gap);
                                        }
                                        let (rect, response) = ui.allocate_exact_size(
                                            Vec2::new(cell_width, 54.0),
                                            egui::Sense::click(),
                                        );
                                        let selected = self.target == Some(*format);
                                        response.widget_info(|| {
                                            egui::WidgetInfo::selected(
                                                egui::WidgetType::RadioButton,
                                                true,
                                                selected,
                                                format!(
                                                    ".{}, {}",
                                                    format_extension(*format),
                                                    format_description(*format)
                                                ),
                                            )
                                        });
                                        ui.painter().rect_filled(
                                            rect,
                                            egui::CornerRadius::same(10),
                                            if selected {
                                                Color32::from_rgb(38, 55, 70)
                                            } else if response.hovered() {
                                                SOFT
                                            } else {
                                                CARD
                                            },
                                        );
                                        ui.painter().rect_stroke(
                                            rect,
                                            egui::CornerRadius::same(10),
                                            Stroke::new(
                                                1.5_f32,
                                                if selected { PRIMARY } else { BORDER },
                                            ),
                                            egui::StrokeKind::Inside,
                                        );
                                        paint_format_icon(
                                            ui.painter(),
                                            egui::Rect::from_min_size(
                                                egui::pos2(
                                                    rect.left() + 12.0,
                                                    rect.center().y - 8.0,
                                                ),
                                                Vec2::splat(18.0),
                                            ),
                                            *format,
                                        );
                                        ui.painter().text(
                                            egui::pos2(rect.left() + 40.0, rect.center().y - 7.0),
                                            egui::Align2::LEFT_CENTER,
                                            format!(".{}", format_extension(*format)),
                                            bold_font(14.0),
                                            TEXT,
                                        );
                                        ui.painter().text(
                                            egui::pos2(rect.left() + 40.0, rect.center().y + 11.0),
                                            egui::Align2::LEFT_CENTER,
                                            format_description(*format),
                                            FontId::proportional(11.0),
                                            FOOTER_TEXT,
                                        );
                                        if response.clicked() {
                                            self.target = Some(*format);
                                            self.status = None;
                                        }
                                    }
                                });
                            }
                        }
                        ui.add_space(20.0);
                    });
            },
        );
    }

    fn show_action_bar(&mut self, ui: &mut egui::Ui, width: f32) {
        let (bar, _) =
            ui.allocate_exact_size(Vec2::new(width, ACTION_BAR_HEIGHT), egui::Sense::hover());
        let painter = ui.painter().clone();
        painter.rect_filled(bar, 0.0, CARD);
        painter.line_segment(
            [bar.left_top(), bar.right_top()],
            Stroke::new(1.0_f32, BORDER),
        );

        let center_y = bar.center().y;
        let convert_rect = egui::Rect::from_center_size(
            egui::pos2(bar.right() - 20.0 - 41.0, center_y),
            Vec2::new(82.0, 40.0),
        );
        let back_rect = egui::Rect::from_center_size(
            egui::pos2(convert_rect.left() - 12.0 - 38.0, center_y),
            Vec2::new(76.0, 40.0),
        );
        let mut remove_index = None;
        let busy = self.is_converting();
        let file_area_right = back_rect.left() - 12.0;
        let file_area = egui::Rect::from_min_max(
            egui::pos2(bar.left() + 20.0, bar.top()),
            egui::pos2(file_area_right, bar.bottom()),
        );
        let content_width = self.inputs.len() as f32 * (187.0 + 8.0) + 45.0;
        let max_scroll = (content_width - file_area.width()).max(0.0);
        self.file_scroll_offset = self.file_scroll_offset.clamp(0.0, max_scroll);
        let scroll_delta = ui.input(|input| {
            if input
                .pointer
                .hover_pos()
                .is_some_and(|position| file_area.contains(position))
            {
                input.raw_scroll_delta.x + input.raw_scroll_delta.y
            } else {
                0.0
            }
        });
        self.file_scroll_offset = (self.file_scroll_offset - scroll_delta).clamp(0.0, max_scroll);
        let file_painter = painter.with_clip_rect(file_area);
        let mut file_x = bar.left() + 20.0 - self.file_scroll_offset;
        for (index, path) in self.inputs.iter().enumerate() {
            let badge = egui::Rect::from_min_size(
                egui::pos2(file_x, center_y - 22.5),
                Vec2::new(187.0, 45.0),
            );
            if badge.intersects(file_area) {
                let visible_badge = badge.intersect(file_area);
                let response = ui.interact(
                    visible_badge,
                    ui.make_persistent_id(("file-info", index)),
                    egui::Sense::hover(),
                );
                let chip_label = format!("{}, {}", file_name(path), file_size(path));
                response.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &chip_label)
                });
                file_painter.rect_filled(
                    badge,
                    egui::CornerRadius::same(8),
                    if response.hovered() {
                        Color32::from_rgb(43, 43, 43)
                    } else {
                        SOFT
                    },
                );
                file_painter.rect_stroke(
                    badge,
                    egui::CornerRadius::same(8),
                    Stroke::new(1.0_f32, BORDER),
                    egui::StrokeKind::Inside,
                );

                let extension = path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let icon_rect = egui::Rect::from_center_size(
                    egui::pos2(badge.left() + 19.0, badge.center().y),
                    Vec2::splat(16.0),
                );
                if matches!(
                    extension.as_str(),
                    "png" | "jpg" | "jpeg" | "webp" | "bmp" | "tif" | "tiff"
                ) {
                    paint_image_icon(&file_painter, icon_rect, GREEN);
                } else {
                    paint_file_icon(&file_painter, icon_rect, GREEN);
                }

                let name = truncate_file_name(file_name(path), 18);
                file_painter.text(
                    egui::pos2(badge.left() + 34.0, badge.center().y - 6.5),
                    egui::Align2::LEFT_CENTER,
                    name,
                    bold_font(13.0),
                    TEXT,
                );
                file_painter.text(
                    egui::pos2(badge.left() + 34.0, badge.center().y + 8.5),
                    egui::Align2::LEFT_CENTER,
                    file_size(path),
                    FontId::proportional(11.0),
                    FOOTER_TEXT,
                );

                let remove_rect = egui::Rect::from_center_size(
                    egui::pos2(badge.right() - 18.0, badge.center().y),
                    Vec2::splat(24.0),
                );
                if remove_rect.intersects(file_area) {
                    let visible_remove = remove_rect.intersect(file_area);
                    let remove = ui.interact(
                        visible_remove,
                        ui.make_persistent_id(("remove-file", index)),
                        egui::Sense::click(),
                    );
                    remove.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            !busy,
                            format!("Remove {}", file_name(path)),
                        )
                    });
                    if remove.hovered() {
                        file_painter.rect_filled(remove_rect, egui::CornerRadius::same(4), CARD);
                    }
                    file_painter.text(
                        remove_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "×",
                        FontId::proportional(16.0),
                        if remove.hovered() { RED } else { MUTED },
                    );
                    if remove.clicked() && !busy {
                        remove_index = Some(index);
                    }
                }
            }
            file_x = badge.right() + 8.0;
        }

        let add_rect =
            egui::Rect::from_min_size(egui::pos2(file_x, center_y - 22.5), Vec2::splat(45.0));
        if add_rect.intersects(file_area) {
            let visible_add = add_rect.intersect(file_area);
            let add = ui.interact(
                visible_add,
                ui.make_persistent_id("add-files"),
                egui::Sense::click(),
            );
            add.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, !busy, "Add files")
            });
            file_painter.rect_filled(
                add_rect,
                egui::CornerRadius::same(8),
                if add.hovered() { SOFT } else { CARD },
            );
            paint_dashed_border_with_radius(&file_painter, add_rect, BORDER, 8.0);
            file_painter.text(
                add_rect.center(),
                egui::Align2::CENTER_CENTER,
                "+",
                FontId::proportional(21.0),
                TEXT,
            );
            if add.clicked() && !busy {
                self.pick_files();
            }
        }

        let back = ui.interact(
            back_rect,
            ui.make_persistent_id("back-to-upload"),
            egui::Sense::click(),
        );
        back.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, !busy, "Back"));
        paint_action_button(&painter, back_rect, &back, "", false, !busy);
        paint_arrow_left(
            &painter,
            egui::pos2(back_rect.left() + 17.0, center_y),
            MUTED,
        );
        painter.text(
            egui::pos2(back_rect.center().x + 5.0, center_y),
            egui::Align2::CENTER_CENTER,
            "Back",
            FontId::proportional(12.0),
            if busy { FOOTER_TEXT } else { MUTED },
        );
        if back.clicked() && !busy {
            self.return_to_upload();
        }

        let can_convert = !busy
            && !self.inputs.is_empty()
            && self.target.is_some()
            && self.output_directory.is_some();
        let convert = ui.interact(
            convert_rect,
            ui.make_persistent_id("convert-files"),
            egui::Sense::click(),
        );
        convert.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                can_convert,
                if busy { "Converting..." } else { "Convert" },
            )
        });
        paint_action_button(
            &painter,
            convert_rect,
            &convert,
            if busy { "Converting..." } else { "Convert" },
            true,
            can_convert,
        );
        if convert.clicked() && can_convert {
            self.start_conversion();
        }

        if let Some(index) = remove_index {
            self.inputs.remove(index);
            self.input_formats.remove(index);
            self.input_format_override = None;
            self.input_format_search.clear();
            self.input_format_show_all = false;
            self.show_input_format_picker = false;
            self.target = None;
            if self.inputs.is_empty() {
                self.return_to_upload();
            } else {
                self.refresh_audio_tracks();
            }
        }
    }

    fn return_to_upload(&mut self) {
        self.page = AppPage::Upload;
        self.inputs.clear();
        self.input_formats.clear();
        self.audio_track_choices.clear();
        self.audio_track_error = None;
        self.audio_track_source = None;
        self.conversion_options.audio_track = None;
        self.audio_track_generation = self.audio_track_generation.wrapping_add(1);
        self.audio_track_request = None;
        self.audio_tracks_loading = false;
        self.input_format_override = None;
        self.input_format_search.clear();
        self.input_format_show_all = false;
        self.show_input_format_picker = false;
        self.file_scroll_offset = 0.0;
        self.target = None;
        self.status = None;
        self.search_filter.clear();
        self.selected_category = None;
        self.show_conversion_settings = false;
    }

    fn show_drop_overlay(&self, context: &egui::Context) {
        let hovered = context.input(|input| {
            input.raw.hovered_files.first().map(|file| {
                file.path
                    .as_deref()
                    .map(file_name)
                    .map(str::to_owned)
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| file.mime.clone())
            })
        });
        let viewport = context.screen_rect();
        egui::Area::new(egui::Id::new("full-page-drop-overlay"))
            .order(egui::Order::Foreground)
            .fixed_pos(viewport.min)
            .show(context, |ui| {
                let (screen, _) = ui.allocate_exact_size(viewport.size(), egui::Sense::hover());
                let painter = ui.painter();
                painter.rect_filled(screen, 0.0, Color32::from_rgba_unmultiplied(0, 0, 0, 150));

                let card_size = Vec2::new(screen.width() * 0.96, screen.height() * 0.88);
                let card = egui::Rect::from_center_size(screen.center(), card_size);
                painter.rect_filled(card, egui::CornerRadius::same(20), CARD);
                paint_dashed_border_with_radius(painter, card, PRIMARY, 20.0);

                let icon = egui::Rect::from_center_size(
                    egui::pos2(card.center().x, card.center().y - 68.0),
                    Vec2::splat(40.0),
                );
                paint_upload_icon(painter, icon, PRIMARY);
                painter.text(
                    egui::pos2(card.center().x, card.center().y - 20.0),
                    egui::Align2::CENTER_CENTER,
                    "Drop to upload",
                    FontId::proportional(24.0),
                    TEXT,
                );
                painter.text(
                    egui::pos2(card.center().x, card.center().y + 10.0),
                    egui::Align2::CENTER_CENTER,
                    "Release to convert your file",
                    FontId::proportional(15.0),
                    MUTED,
                );
                if let Some(name) = hovered {
                    let chip = egui::Rect::from_center_size(
                        egui::pos2(card.center().x, card.center().y + 52.0),
                        Vec2::new((name.len() as f32 * 7.0 + 44.0).clamp(150.0, 420.0), 34.0),
                    );
                    painter.rect_filled(chip, egui::CornerRadius::same(17), SOFT);
                    painter.rect_stroke(
                        chip,
                        egui::CornerRadius::same(17),
                        Stroke::new(1.0_f32, BORDER),
                        egui::StrokeKind::Inside,
                    );
                    painter.text(
                        chip.center(),
                        egui::Align2::CENTER_CENTER,
                        name,
                        FontId::proportional(12.0),
                        TEXT,
                    );
                }
            });
    }

    fn pick_files(&mut self) {
        let extensions = supported_input_extensions();
        if let Some(files) = FileDialog::new()
            .add_filter("Supported files", extensions.as_slice())
            .add_filter("All files", &["*"])
            .pick_files()
        {
            self.add_files(files);
        }
    }

    fn add_files(&mut self, files: impl IntoIterator<Item = PathBuf>) {
        let mut added = false;
        for file in files {
            if file.is_file() && !self.inputs.contains(&file) {
                self.input_formats.push(detect(&file));
                self.inputs.push(file);
                added = true;
            }
        }
        if added {
            self.refresh_audio_tracks();
            self.page = AppPage::Conversion;
            self.target = None;
            self.input_format_override = None;
            self.input_format_search.clear();
            self.input_format_show_all = false;
            self.show_input_format_picker = false;
            self.search_filter.clear();
            self.selected_category = None;
            self.status = None;
        }
    }

    fn collect_dropped_files(&mut self, context: &egui::Context) {
        let dropped_files = context.input(|input| input.raw.dropped_files.clone());
        if dropped_files.iter().any(|file| file.path.is_some()) {
            self.add_files(dropped_files.into_iter().filter_map(|file| file.path));
        }
    }

    fn is_converting(&self) -> bool {
        self.conversion_job.is_some()
    }

    fn cancel_conversion(&mut self) {
        let Some(job) = self.conversion_job.as_mut() else {
            return;
        };
        if !job.cancel.swap(true, Ordering::Relaxed) {
            job.message = "Cancelling after the current step...".to_owned();
        }
    }

    fn show_conversion_progress(&mut self, context: &egui::Context) {
        let Some(job) = self.conversion_job.as_ref() else {
            return;
        };
        let mut cancel_clicked = false;

        let _ = egui::Modal::new(egui::Id::new("conversion-progress-modal"))
            .frame(
                egui::Frame::new()
                    .fill(CARD)
                    .stroke(Stroke::new(1.0_f32, BORDER))
                    .corner_radius(egui::CornerRadius::same(14))
                    .inner_margin(egui::Margin::same(28)),
            )
            .show(context, |ui| {
                ui.set_max_width(512.0);
                ui.heading("Converting...");
                ui.label(
                    RichText::new(format!("{} → {}", job.source_label, job.target_label))
                        .size(13.0)
                        .color(MUTED),
                );
                ui.add_space(14.0);
                let step_fraction =
                    (job.completed as f32 / job.total.max(1) as f32).clamp(0.0, 1.0);
                ui.add(egui::ProgressBar::new(step_fraction).show_percentage());
                ui.add_space(8.0);
                ui.label(RichText::new(job.message.as_str()).size(12.0).color(TEXT));
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if job.cancel.load(Ordering::Relaxed) {
                        ui.add_enabled(false, egui::Button::new("Cancelling..."));
                    } else if ui.button("Cancel").clicked() {
                        cancel_clicked = true;
                    }
                });
            });

        if cancel_clicked {
            self.cancel_conversion();
        }
    }

    fn start_conversion(&mut self) {
        let Some(output_directory) = self.output_directory.clone() else {
            self.status = Some((
                "Choose an output folder before converting.".to_owned(),
                true,
            ));
            return;
        };
        if self.inputs.is_empty() {
            return;
        }

        let inputs = self.inputs.clone();
        let Some(target) = self.target else {
            return;
        };
        let options = self.conversion_options;
        let source_label = self
            .effective_input_format()
            .unwrap_or(InputFormat::Any)
            .extension_label()
            .to_owned();
        let target_label = format!(".{}", format_extension(target));
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        self.conversion_job = Some(ConversionJob {
            receiver,
            cancel,
            completed: 0,
            // The worker counts the real work before it reports the first
            // step, so the bar starts empty rather than at a guessed total.
            total: 0,
            message: "Preparing...".to_owned(),
            source_label,
            target_label,
        });
        self.show_input_format_picker = false;
        self.conversion_notice = None;
        self.status = None;

        let progress_sender = sender.clone();
        thread::spawn(move || {
            let result = convert_to_directory(
                &inputs,
                target,
                options,
                &output_directory,
                worker_cancel.as_ref(),
                |completed, total, message| {
                    let _ = progress_sender.send(ConversionEvent::Progress {
                        completed,
                        total,
                        message,
                    });
                },
            );
            let event = match result {
                Ok(message) => ConversionEvent::Finished(Ok(message)),
                Err(ConversionFailure::Cancelled) => ConversionEvent::Cancelled,
                Err(ConversionFailure::Message(message)) => ConversionEvent::Finished(Err(message)),
            };
            let _ = sender.send(event);
        });
    }

    fn finish_conversion(&mut self, title: &str, message: String, error: bool) {
        self.conversion_job = None;
        self.status = None;
        self.conversion_notice = Some((title.to_owned(), message, error));
    }

    fn poll_conversion(&mut self) {
        loop {
            let received = match self.conversion_job.as_ref() {
                Some(job) => job.receiver.try_recv(),
                None => return,
            };
            match received {
                Ok(ConversionEvent::Progress {
                    completed,
                    total,
                    message,
                }) => {
                    if let Some(job) = self.conversion_job.as_mut() {
                        job.completed = completed;
                        job.total = total;
                        if !job.cancel.load(Ordering::Relaxed) {
                            job.message = message;
                        }
                    }
                }
                Ok(ConversionEvent::Finished(Ok(message))) => {
                    self.finish_conversion("Conversion complete!", message, false);
                    return;
                }
                Ok(ConversionEvent::Finished(Err(message))) => {
                    self.finish_conversion("Conversion failed", message, true);
                    return;
                }
                Ok(ConversionEvent::Cancelled) => {
                    self.finish_conversion(
                        "Conversion cancelled",
                        "No files were written.".to_owned(),
                        false,
                    );
                    return;
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.finish_conversion(
                        "Conversion error",
                        "Conversion stopped unexpectedly.".to_owned(),
                        true,
                    );
                    return;
                }
            }
        }
    }
}

fn paint_search_icon(painter: &egui::Painter, field: egui::Rect, color: Color32) {
    let icon = egui::Rect::from_min_size(
        egui::pos2(field.left() + 13.6, field.center().y - 8.0),
        Vec2::splat(16.0),
    );
    let lens = egui::pos2(icon.left() + 6.2, icon.top() + 6.2);
    let stroke = Stroke::new(1.5_f32, color);
    painter.circle_stroke(lens, 4.0, stroke);
    painter.line_segment(
        [
            lens + Vec2::new(2.8, 2.8),
            egui::pos2(icon.right() - 1.0, icon.bottom() - 1.0),
        ],
        stroke,
    );
}

fn paint_refresh_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let scale = rect.width() / 24.0;
    let point = |x: f32, y: f32| egui::pos2(rect.left() + x * scale, rect.top() + y * scale);
    let stroke = Stroke::new(2.5 * scale, color);

    painter.line_segment([point(3.0, 2.0), point(3.0, 8.0)], stroke);
    painter.line_segment([point(3.0, 8.0), point(9.0, 8.0)], stroke);
    painter.line_segment([point(21.0, 22.0), point(21.0, 16.0)], stroke);
    painter.line_segment([point(21.0, 16.0), point(15.0, 16.0)], stroke);

    for (start, end) in [(0.0_f32, -132.0_f32), (180.0_f32, 48.0_f32)] {
        let steps = 18;
        let mut previous = point(
            12.0 + 9.0 * start.to_radians().cos(),
            12.0 + 9.0 * start.to_radians().sin(),
        );
        for step in 1..=steps {
            let angle = (start + (end - start) * step as f32 / steps as f32).to_radians();
            let next = point(12.0 + 9.0 * angle.cos(), 12.0 + 9.0 * angle.sin());
            painter.line_segment([previous, next], stroke);
            previous = next;
        }
    }
}

fn paint_upload_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let scale = rect.width() / 24.0;
    let point = |x: f32, y: f32| egui::pos2(rect.left() + x * scale, rect.top() + y * scale);
    let stroke = Stroke::new(2.0 * scale, color);
    painter.line_segment([point(12.0, 3.0), point(12.0, 15.0)], stroke);
    painter.line_segment([point(7.0, 8.0), point(12.0, 3.0)], stroke);
    painter.line_segment([point(12.0, 3.0), point(17.0, 8.0)], stroke);
    painter.line_segment([point(3.0, 15.0), point(3.0, 18.0)], stroke);
    painter.line_segment([point(3.0, 18.0), point(5.0, 20.0)], stroke);
    painter.line_segment([point(5.0, 20.0), point(19.0, 20.0)], stroke);
    painter.line_segment([point(19.0, 20.0), point(21.0, 18.0)], stroke);
    painter.line_segment([point(21.0, 18.0), point(21.0, 15.0)], stroke);
}

fn paint_settings_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let center = rect.center();
    painter.circle_stroke(center, rect.width() * 0.31, Stroke::new(1.5_f32, color));
    painter.circle_stroke(center, rect.width() * 0.1, Stroke::new(1.5_f32, color));
    for index in 0..8 {
        let angle = std::f32::consts::TAU * index as f32 / 8.0;
        let direction = Vec2::new(angle.cos(), angle.sin());
        painter.line_segment(
            [
                center + direction * rect.width() * 0.36,
                center + direction * rect.width() * 0.47,
            ],
            Stroke::new(1.5_f32, color),
        );
    }
}

fn paint_help_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    painter.circle_stroke(
        rect.center(),
        rect.width() * 0.46,
        Stroke::new(1.5_f32, color),
    );
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "?",
        FontId::proportional(rect.width() * 0.72),
        color,
    );
}

fn paint_github_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let scale = rect.width() / 12.0;
    let point = |x: f32, y: f32| egui::pos2(rect.left() + x * scale, rect.top() + y * scale);
    painter.add(egui::Shape::convex_polygon(
        vec![
            point(1.5, 1.0),
            point(5.2, 3.2),
            point(4.7, 6.3),
            point(2.0, 5.2),
        ],
        color,
        Stroke::NONE,
    ));
    painter.add(egui::Shape::convex_polygon(
        vec![
            point(6.8, 3.2),
            point(10.5, 1.0),
            point(10.0, 5.2),
            point(7.3, 6.3),
        ],
        color,
        Stroke::NONE,
    ));
    painter.circle_filled(point(6.0, 5.8), 4.7 * scale, color);
    painter.rect_filled(
        egui::Rect::from_min_max(point(2.8, 6.5), point(9.2, 10.2)),
        egui::CornerRadius::same(3),
        color,
    );
    painter.rect_filled(
        egui::Rect::from_min_max(point(3.4, 9.0), point(5.0, 12.0)),
        egui::CornerRadius::same(1),
        color,
    );
    painter.rect_filled(
        egui::Rect::from_min_max(point(7.0, 9.0), point(8.6, 12.0)),
        egui::CornerRadius::same(1),
        color,
    );
    painter.circle_filled(point(4.5, 5.3), 0.45 * scale, SOFT);
    painter.circle_filled(point(7.5, 5.3), 0.45 * scale, SOFT);
}

fn paint_formats_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let stroke = Stroke::new(1.4_f32, color);
    let back = egui::Rect::from_min_size(
        egui::pos2(rect.left() + 1.0, rect.top() + 1.0),
        Vec2::new(rect.width() * 0.57, rect.height() * 0.7),
    );
    painter.rect_stroke(
        back,
        egui::CornerRadius::same(2),
        stroke,
        egui::StrokeKind::Inside,
    );
    let front = egui::Rect::from_min_size(
        egui::pos2(
            rect.left() + rect.width() * 0.34,
            rect.top() + rect.height() * 0.28,
        ),
        Vec2::new(rect.width() * 0.57, rect.height() * 0.7),
    );
    painter.rect_stroke(
        front,
        egui::CornerRadius::same(2),
        stroke,
        egui::StrokeKind::Inside,
    );
    painter.line_segment(
        [
            egui::pos2(front.left() + 3.0, front.top() + 5.0),
            egui::pos2(front.right() - 3.0, front.top() + 5.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(front.left() + 3.0, front.top() + 8.0),
            egui::pos2(front.right() - 3.0, front.top() + 8.0),
        ],
        stroke,
    );
}

fn bold_font(size: f32) -> FontId {
    FontId::new(size, egui::FontFamily::Name("Inter Variable".into()))
}

fn paint_action_button(
    painter: &egui::Painter,
    rect: egui::Rect,
    response: &egui::Response,
    label: &str,
    primary: bool,
    enabled: bool,
) {
    let fill = if primary {
        if enabled {
            PRIMARY
        } else {
            Color32::from_rgb(67, 91, 115)
        }
    } else if response.hovered() && enabled {
        SOFT
    } else {
        CARD
    };
    painter.rect_filled(rect, egui::CornerRadius::same(8), fill);
    painter.rect_stroke(
        rect,
        egui::CornerRadius::same(8),
        Stroke::new(1.0_f32, if primary { fill } else { BORDER }),
        egui::StrokeKind::Inside,
    );
    if !label.is_empty() {
        let color = if primary {
            if enabled {
                Color32::WHITE
            } else {
                MUTED
            }
        } else if enabled {
            MUTED
        } else {
            FOOTER_TEXT
        };
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            if primary {
                bold_font(12.0)
            } else {
                FontId::proportional(12.0)
            },
            color,
        );
    }
}

fn paint_sliders_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let stroke = Stroke::new(1.5_f32, color);
    for (index, knob_x) in [0.3_f32, 0.7, 0.45].into_iter().enumerate() {
        let y = rect.top() + rect.height() * (0.2 + index as f32 * 0.3);
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            stroke,
        );
        painter.circle_filled(
            egui::pos2(rect.left() + rect.width() * knob_x, y),
            2.5,
            CARD,
        );
        painter.circle_stroke(
            egui::pos2(rect.left() + rect.width() * knob_x, y),
            2.5,
            stroke,
        );
    }
}

fn paint_arrow_right(painter: &egui::Painter, center: egui::Pos2, color: Color32) {
    let stroke = Stroke::new(1.8_f32, color);
    painter.line_segment(
        [center + Vec2::new(-8.0, 0.0), center + Vec2::new(8.0, 0.0)],
        stroke,
    );
    painter.line_segment(
        [center + Vec2::new(2.0, -6.0), center + Vec2::new(8.0, 0.0)],
        stroke,
    );
    painter.line_segment(
        [center + Vec2::new(2.0, 6.0), center + Vec2::new(8.0, 0.0)],
        stroke,
    );
}

fn paint_arrow_left(painter: &egui::Painter, center: egui::Pos2, color: Color32) {
    let stroke = Stroke::new(1.6_f32, color);
    painter.line_segment(
        [center + Vec2::new(-5.0, 0.0), center + Vec2::new(6.0, 0.0)],
        stroke,
    );
    painter.line_segment(
        [center + Vec2::new(-5.0, 0.0), center + Vec2::new(1.0, -6.0)],
        stroke,
    );
    painter.line_segment(
        [center + Vec2::new(-5.0, 0.0), center + Vec2::new(1.0, 6.0)],
        stroke,
    );
}

fn paint_image_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let stroke = Stroke::new(1.5_f32, color);
    painter.rect_stroke(
        rect.shrink(1.0),
        egui::CornerRadius::same(2),
        stroke,
        egui::StrokeKind::Inside,
    );
    painter.circle_filled(
        egui::pos2(
            rect.left() + rect.width() * 0.68,
            rect.top() + rect.height() * 0.32,
        ),
        rect.width() * 0.08,
        color,
    );
    painter.line_segment(
        [
            egui::pos2(rect.left() + 3.0, rect.bottom() - 4.0),
            egui::pos2(
                rect.left() + rect.width() * 0.43,
                rect.top() + rect.height() * 0.48,
            ),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(
                rect.left() + rect.width() * 0.43,
                rect.top() + rect.height() * 0.48,
            ),
            egui::pos2(rect.right() - 3.0, rect.bottom() - 4.0),
        ],
        stroke,
    );
}

fn paint_archive_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let stroke = Stroke::new(1.4_f32, color);
    painter.rect_stroke(
        egui::Rect::from_min_max(
            egui::pos2(rect.left() + 2.0, rect.top() + 4.0),
            egui::pos2(rect.right() - 2.0, rect.bottom() - 1.0),
        ),
        egui::CornerRadius::same(2),
        stroke,
        egui::StrokeKind::Inside,
    );
    painter.line_segment(
        [
            egui::pos2(rect.left() + 1.0, rect.top() + 4.0),
            egui::pos2(rect.right() - 1.0, rect.top() + 4.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(rect.center().x, rect.top() + 5.0),
            egui::pos2(rect.center().x, rect.top() + rect.height() * 0.65),
        ],
        stroke,
    );
}

fn paint_text_category_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let stroke = Stroke::new(1.2_f32, color);
    for y in [0.28_f32, 0.5, 0.72] {
        let line_y = rect.top() + rect.height() * y;
        painter.line_segment(
            [
                egui::pos2(rect.left() + 1.0, line_y),
                egui::pos2(rect.right() - 1.0, line_y),
            ],
            stroke,
        );
    }
}

fn paint_braces_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let scale = rect.width() / 24.0;
    let point = |x: f32, y: f32| egui::pos2(rect.left() + x * scale, rect.top() + y * scale);
    let stroke = Stroke::new(2.0 * scale, color);
    let segments = [
        (8.0, 3.0, 7.0, 3.0),
        (7.0, 3.0, 5.0, 5.0),
        (5.0, 5.0, 5.0, 10.0),
        (5.0, 10.0, 3.0, 12.0),
        (3.0, 12.0, 5.0, 14.0),
        (5.0, 14.0, 5.0, 19.0),
        (5.0, 19.0, 7.0, 21.0),
        (7.0, 21.0, 8.0, 21.0),
        (16.0, 3.0, 17.0, 3.0),
        (17.0, 3.0, 19.0, 5.0),
        (19.0, 5.0, 19.0, 10.0),
        (19.0, 10.0, 21.0, 12.0),
        (21.0, 12.0, 19.0, 14.0),
        (19.0, 14.0, 19.0, 19.0),
        (19.0, 19.0, 17.0, 21.0),
        (17.0, 21.0, 16.0, 21.0),
    ];
    for (x1, y1, x2, y2) in segments {
        painter.line_segment([point(x1, y1), point(x2, y2)], stroke);
    }
}

fn paint_category_icon(
    painter: &egui::Painter,
    rect: egui::Rect,
    category: FormatCategory,
    color: Color32,
) {
    match category {
        FormatCategory::Image => paint_image_icon(painter, rect, color),
        FormatCategory::Document => paint_file_icon(painter, rect, color),
        FormatCategory::Text => paint_text_category_icon(painter, rect, color),
        FormatCategory::Data => paint_braces_icon(painter, rect, color),
        FormatCategory::Audio => paint_audio_icon(painter, rect, color),
        FormatCategory::Video => paint_image_icon(painter, rect, color),
        FormatCategory::Archive => paint_archive_icon(painter, rect, color),
        FormatCategory::Font => paint_text_category_icon(painter, rect, color),
    }
}

fn paint_audio_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let heights = [0.32_f32, 0.62, 1.0, 0.48, 0.78, 0.38];
    let slot = rect.width() / heights.len() as f32;
    let bar_width = (slot * 0.45).max(1.0);
    for (index, height) in heights.iter().enumerate() {
        let center_x = rect.left() + slot * (index as f32 + 0.5);
        let bar_height = rect.height() * height;
        let bar = egui::Rect::from_center_size(
            egui::pos2(center_x, rect.center().y),
            Vec2::new(bar_width, bar_height),
        );
        painter.rect_filled(bar, egui::CornerRadius::same(1), color);
    }
}

fn paint_format_icon(painter: &egui::Painter, rect: egui::Rect, format: OutputFormat) {
    match format_category(format) {
        FormatCategory::Image => paint_image_icon(painter, rect, GREEN),
        FormatCategory::Document | FormatCategory::Text => paint_file_icon(painter, rect, GREEN),
        FormatCategory::Data => {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "{}",
                FontId::proportional(10.0),
                GREEN,
            );
        }
        FormatCategory::Audio => paint_audio_icon(painter, rect, GREEN),
        FormatCategory::Video => paint_image_icon(painter, rect, GREEN),
        FormatCategory::Archive => paint_archive_icon(painter, rect, GREEN),
        FormatCategory::Font => {
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "Aa",
                FontId::proportional(11.0),
                GREEN,
            );
        }
    }
}

fn paint_file_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let stroke = Stroke::new(1.5_f32, color);
    let page = egui::Rect::from_min_max(
        egui::pos2(rect.left() + rect.width() * 0.18, rect.top()),
        egui::pos2(rect.right() - rect.width() * 0.18, rect.bottom()),
    );
    painter.rect_stroke(
        page,
        egui::CornerRadius::same(2),
        stroke,
        egui::StrokeKind::Inside,
    );
    for offset in [0.38_f32, 0.58, 0.78] {
        let y = rect.top() + rect.height() * offset;
        painter.line_segment(
            [
                egui::pos2(page.left() + 3.0, y),
                egui::pos2(page.right() - 3.0, y),
            ],
            Stroke::new(1.0_f32, color),
        );
    }
}

fn truncate_file_name(name: &str, max_chars: usize) -> String {
    let mut characters = name.chars();
    let shortened: String = characters.by_ref().take(max_chars).collect();
    if characters.next().is_some() {
        format!("{shortened}…")
    } else {
        shortened
    }
}

fn file_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("File")
}

fn file_size(path: &Path) -> String {
    let bytes = fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

#[derive(Debug)]
enum ConversionFailure {
    Cancelled,
    Message(String),
}

/// The number of steps a directory conversion will take, so the progress bar
/// counts work that actually happens.
///
/// Every step the loop below reports is one unit: reading an input, converting
/// it, and saving each file it produced. A fixed multiplier per input cannot
/// express that, because a PDF rasterised to images writes one file per page
/// and the page count is only known by asking the converter. Archives build one
/// file from the whole batch, so they count differently.
fn planned_step_total(inputs: &[PathBuf], target: OutputFormat) -> usize {
    if matches!(
        target,
        OutputFormat::Zip | OutputFormat::Cbz | OutputFormat::Tar | OutputFormat::TarGz
    ) {
        return inputs.len().saturating_add(2);
    }
    if format_category(target) == FormatCategory::Audio {
        return inputs.len().saturating_mul(2);
    }
    inputs
        .iter()
        .map(|input| {
            // A count that cannot be taken falls back to the smallest possible
            // output count; the conversion itself then reports the real error.
            2usize.saturating_add(planned_output_count(input, target).unwrap_or(1))
        })
        .sum()
}

fn convert_to_directory(
    inputs: &[PathBuf],
    target: OutputFormat,
    options: ConversionOptions,
    output_directory: &Path,
    cancel: &AtomicBool,
    mut report: impl FnMut(usize, usize, String),
) -> Result<String, ConversionFailure> {
    // Count the work before the first step so every report carries the same
    // total. The bar then reaches 100% exactly when the last file is saved.
    let total = planned_step_total(inputs, target);
    let mut report = |completed: usize, message: String| report(completed, total, message);
    let mut completed = 0;
    let mut written = Vec::new();
    let video = VideoToolConfig::default();
    if matches!(
        target,
        OutputFormat::Zip | OutputFormat::Cbz | OutputFormat::Tar | OutputFormat::TarGz
    ) {
        let mut files = Vec::with_capacity(inputs.len());
        for path in inputs {
            if cancel.load(Ordering::Relaxed) {
                return Err(ConversionFailure::Cancelled);
            }
            report(completed, format!("Reading {}...", file_name(path)));
            let bytes = fs::read(path).map_err(|error| {
                ConversionFailure::Message(format!("{}: {error}", path.display()))
            })?;
            files.push((path.clone(), bytes));
            completed += 1;
            report(
                completed,
                format!("Read {} of {} files", completed, inputs.len()),
            );
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(ConversionFailure::Cancelled);
        }
        report(completed, "Creating archive...".to_owned());
        let converted = convert_batch_with_options(&files, target, options)
            .map_err(|error| ConversionFailure::Message(error.to_string()))?;
        completed += 1;
        report(completed, "Archive created".to_owned());

        for file in converted {
            if cancel.load(Ordering::Relaxed) {
                remove_written(&written);
                return Err(ConversionFailure::Cancelled);
            }
            report(completed, format!("Saving {}...", file.name.display()));
            let output =
                write_unique(output_directory, &file.name, &file.bytes).map_err(|error| {
                    ConversionFailure::Message(format!("{}: {error}", file.name.display()))
                })?;
            written.push(output);
            completed += 1;
            report(completed, format!("Saved {}", file.name.display()));
        }
        return Ok(conversion_summary(&written));
    }

    // Convert and write one file at a time. A batch no longer keeps every
    // input and every output in memory at once, which matters for long videos
    // and high-resolution images.
    for (index, path) in inputs.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            remove_written(&written);
            return Err(ConversionFailure::Cancelled);
        }
        if format_category(target) == FormatCategory::Audio {
            report(
                completed,
                format!(
                    "Converting {} ({}/{})...",
                    file_name(path),
                    index + 1,
                    inputs.len()
                ),
            );
            let output_name = audio_output_name(file_name(path), target);
            let output = match write_unique_with(output_directory, &output_name, |file| {
                convert_audio_file_to_writer_with_cancel(
                    path,
                    file_name(path),
                    target,
                    options,
                    file,
                    cancel,
                )
                .map_err(|error| match error {
                    ConvertError::Cancelled => {
                        std::io::Error::new(std::io::ErrorKind::Interrupted, "conversion cancelled")
                    }
                    error => std::io::Error::other(error.to_string()),
                })
            }) {
                Ok(output) => output,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                    remove_written(&written);
                    return Err(ConversionFailure::Cancelled);
                }
                Err(error) => {
                    return Err(ConversionFailure::Message(format!(
                        "{}: {error}",
                        output_name.display()
                    )))
                }
            };
            completed += 2;
            written.push(output);
            report(completed, format!("Saved {}", output_name.display()));
            continue;
        }

        report(completed, format!("Reading {}...", file_name(path)));
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                remove_written(&written);
                return Err(ConversionFailure::Message(format!(
                    "{}: {error}",
                    path.display()
                )));
            }
        };
        completed += 1;

        report(
            completed,
            format!(
                "Converting {} ({}/{})...",
                file_name(path),
                index + 1,
                inputs.len()
            ),
        );
        let outputs =
            match convert_bytes_with_video_config(file_name(path), &bytes, target, options, &video)
            {
                Ok(outputs) => outputs,
                Err(ConvertError::Cancelled) => {
                    remove_written(&written);
                    return Err(ConversionFailure::Cancelled);
                }
                Err(error) => {
                    remove_written(&written);
                    return Err(ConversionFailure::Message(error.to_string()));
                }
            };
        drop(bytes);
        completed += 1;

        for file in outputs {
            if cancel.load(Ordering::Relaxed) {
                remove_written(&written);
                return Err(ConversionFailure::Cancelled);
            }
            report(completed, format!("Saving {}...", file.name.display()));
            let output =
                write_unique(output_directory, &file.name, &file.bytes).map_err(|error| {
                    ConversionFailure::Message(format!("{}: {error}", file.name.display()))
                })?;
            written.push(output);
            completed += 1;
            report(completed, format!("Saved {}", file.name.display()));
        }
    }

    Ok(conversion_summary(&written))
}

fn conversion_summary(written: &[PathBuf]) -> String {
    let remaining = written.len().saturating_sub(3);
    let suffix = if remaining == 0 {
        String::new()
    } else {
        format!(", and {remaining} more")
    };
    format!(
        "Created {} file(s): {}{suffix}",
        written.len(),
        written
            .iter()
            .take(3)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn remove_written(written: &[PathBuf]) {
    for path in written {
        let _ = fs::remove_file(path);
    }
}

fn write_unique(directory: &Path, relative_path: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    write_unique_with(directory, relative_path, |file| file.write_all(bytes))
}

fn write_unique_with<F>(
    directory: &Path,
    relative_path: &Path,
    write: F,
) -> std::io::Result<PathBuf>
where
    F: FnOnce(&mut File) -> std::io::Result<()>,
{
    let relative_path = validate_relative_path(relative_path)?;
    let parent = relative_path.parent().unwrap_or_else(|| Path::new(""));
    let output_directory = directory.join(parent);
    fs::create_dir_all(&output_directory)?;
    let file_name = relative_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("converted"));
    let stem = Path::new(file_name)
        .file_stem()
        .filter(|stem| !stem.is_empty())
        .unwrap_or(file_name);
    let extension = Path::new(file_name).extension();

    for suffix in 1_u32.. {
        let candidate_name = if suffix == 1 {
            file_name.to_os_string()
        } else {
            let mut name = format!("{} ({})", stem.to_string_lossy(), suffix);
            if let Some(extension) = extension {
                name.push('.');
                name.push_str(&extension.to_string_lossy());
            }
            name.into()
        };
        let output = output_directory.join(candidate_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)
        {
            Ok(mut file) => {
                if let Err(error) = write(&mut file) {
                    drop(file);
                    return match fs::remove_file(&output) {
                        Ok(()) => Err(error),
                        Err(cleanup_error) => Err(std::io::Error::new(
                            error.kind(),
                            format!(
                                "{error}; also failed to remove partial output: {cleanup_error}"
                            ),
                        )),
                    };
                }
                return Ok(output);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    unreachable!("the suffix counter cannot exhaust every u32 value")
}

fn validate_relative_path(path: &Path) -> std::io::Result<PathBuf> {
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => safe.push(part),
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "output path must be relative and cannot contain parent components",
                ));
            }
        }
    }
    if safe.as_os_str().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "output path cannot be empty",
        ));
    }
    Ok(safe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use convert_to_it_native::{convert_audio_file_to_writer, VideoToolConfig};
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static VIDEO_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn ffmpeg_stub() -> &'static Path {
        static STUB: OnceLock<PathBuf> = OnceLock::new();
        STUB.get_or_init(|| {
            let directory = std::env::temp_dir()
                .join(format!("convert2it-gui-ffmpeg-stub-{}", std::process::id()));
            std::fs::create_dir_all(&directory).unwrap();
            let source = directory.join("ffmpeg_stub.rs");
            std::fs::write(
                &source,
                r#"
use std::{env, fs, process};
fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if let Ok(code) = env::var("STUB_EXIT") {
        process::exit(code.parse().unwrap_or(1));
    }
    if let Some(output) = args.last() {
        fs::copy(env::var("STUB_OUTPUT").unwrap(), output).unwrap();
    }
}
"#,
            )
            .unwrap();
            let executable = directory.join(if cfg!(windows) {
                "ffmpeg.exe"
            } else {
                "ffmpeg"
            });
            let status = std::process::Command::new("rustc")
                .arg("--edition=2021")
                .arg(&source)
                .arg("-o")
                .arg(&executable)
                .status()
                .expect("rustc must be available to build the ffmpeg test stub");
            assert!(status.success(), "could not build the ffmpeg test stub");
            executable
        })
    }

    struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl EnvGuard {
        fn set(&mut self, key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> &mut Self {
            self.0.push((key, std::env::var_os(key)));
            std::env::set_var(key, value);
            self
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.0.drain(..).rev() {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn real_ffmpeg() -> Option<PathBuf> {
        let path = VideoToolConfig::default().resolve()?;
        let output = std::process::Command::new(&path)
            .arg("-version")
            .output()
            .ok()?;
        let version = String::from_utf8_lossy(&output.stdout);
        (output.status.success() && version.contains("ffmpeg version")).then_some(path)
    }

    /// A test that cannot run has to say so where the log will show it.
    /// libtest captures the `print!` macros for a test that passes, so an
    /// `eprintln!` notice never leaves the harness and the skip reads as `ok`.
    /// Writing to the handle directly is not captured. Setting
    /// `CONVERT2IT_REQUIRE_FFMPEG=1` turns the skip into a failure, so a job
    /// that is supposed to have ffmpeg cannot go green without running these.
    fn ffmpeg_for_real_test(test: &str) -> Option<PathBuf> {
        if let Some(ffmpeg) = real_ffmpeg() {
            return Some(ffmpeg);
        }
        let notice = format!("skipped real-ffmpeg {test}: ffmpeg is not available");
        {
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(), "{notice}");
        }
        if std::env::var("CONVERT2IT_REQUIRE_FFMPEG").is_ok_and(|value| value == "1") {
            panic!("{notice} (CONVERT2IT_REQUIRE_FFMPEG is set)");
        }
        None
    }

    fn generate_real_video(ffmpeg: &Path, path: &Path) {
        let output = std::process::Command::new(ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=10:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-c:v",
                "libvpx-vp9",
                "-b:v",
                "300k",
                "-c:a",
                "libopus",
                "-shortest",
            ])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "ffmpeg could not generate the synthetic sample: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[derive(Default)]
    struct MemoryStorage(HashMap<String, String>);

    impl eframe::Storage for MemoryStorage {
        fn get_string(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }

        fn set_string(&mut self, key: &str, value: String) {
            self.0.insert(key.to_owned(), value);
        }

        fn flush(&mut self) {}
    }

    fn non_default_settings_options() -> ConversionOptions {
        ConversionOptions {
            jpeg_quality: Some(91),
            png_compression: Some(8),
            archive_compression: Some(2),
            audio_sample_rate: Some(48_000),
            audio_channels: Some(AudioChannels::Stereo),
            audio_track: Some(1),
            audio_bit_depth: AudioBitDepth::Bit24,
            mp3_bitrate: Mp3Bitrate::Kbps320,
            image_background: [12, 34, 56],
            image_apply_exif_orientation: false,
            pdf_page_size: PdfPageSize::Letter,
            pdf_orientation: PdfOrientation::Landscape,
            pdf_image_fit: PdfImageFit::Cover,
            pdf_font_size: 14,
            pdf_margin: 24,
            video_frame_time_ms: 1_250,
            video_thumbnail_width: 480,
        }
    }

    struct CloseAfterFirstFrame {
        app: ConvertApp,
    }

    impl eframe::App for CloseAfterFirstFrame {
        fn update(&mut self, context: &egui::Context, frame: &mut eframe::Frame) {
            self.app.update(context, frame);
            context.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        fn save(&mut self, storage: &mut dyn eframe::Storage) {
            self.app.save(storage);
        }
    }

    const DISK_TEST_APP_ID: &str = "Convert to it! disk storage test";

    fn run_hidden_eframe(
        persistence_path: &Path,
        app_creator: eframe::AppCreator<'static>,
    ) -> eframe::Result<()> {
        eframe::run_native(
            DISK_TEST_APP_ID,
            eframe::NativeOptions {
                viewport: egui::ViewportBuilder::default()
                    .with_inner_size([64.0, 64.0])
                    .with_position(egui::pos2(-10_000.0, -10_000.0))
                    .with_visible(false),
                persistence_path: Some(persistence_path.to_path_buf()),
                run_and_return: true,
                event_loop_builder: Some(Box::new(|builder| {
                    #[cfg(target_os = "windows")]
                    {
                        use winit::platform::windows::EventLoopBuilderExtWindows as _;
                        builder.with_any_thread(true);
                    }
                    #[cfg(not(target_os = "windows"))]
                    let _ = builder;
                })),
                ..Default::default()
            },
            app_creator,
        )
    }

    fn write_settings_via_real_eframe(
        persistence_path: &Path,
        output_directory: &Path,
        target: OutputFormat,
        conversion_options: ConversionOptions,
    ) {
        let output_directory = output_directory.to_path_buf();
        let result = run_hidden_eframe(
            persistence_path,
            Box::new(move |creation_context| {
                apply_theme(&creation_context.egui_ctx);
                let mut app = ConvertApp::from_storage(creation_context.storage);
                app.output_directory = Some(output_directory);
                app.target = Some(target);
                app.conversion_options = conversion_options;
                Ok(Box::new(CloseAfterFirstFrame { app }))
            }),
        );
        assert!(
            result.is_ok(),
            "eframe disk-storage save failed: {result:?}"
        );
    }

    type LoadedSettings = (Option<PathBuf>, Option<OutputFormat>, ConversionOptions);

    fn read_settings_via_real_eframe(persistence_path: &Path) -> LoadedSettings {
        let loaded = Arc::new(Mutex::new(None::<LoadedSettings>));
        let loaded_for_creator = Arc::clone(&loaded);
        let result = run_hidden_eframe(
            persistence_path,
            Box::new(move |creation_context| {
                apply_theme(&creation_context.egui_ctx);
                let app = ConvertApp::from_storage(creation_context.storage);
                *loaded_for_creator.lock().unwrap() = Some((
                    app.output_directory.clone(),
                    app.target,
                    app.conversion_options,
                ));
                Ok(Box::new(CloseAfterFirstFrame { app }))
            }),
        );
        assert!(
            result.is_ok(),
            "eframe disk-storage load failed: {result:?}"
        );
        let settings = loaded
            .lock()
            .unwrap()
            .take()
            .expect("eframe app creator did not run");
        settings
    }

    fn disk_test_directory(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "convert2it-eframe-disk-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn secondary_text_clears_the_body_contrast_target() {
        fn luminance(color: Color32) -> f64 {
            fn channel(value: u8) -> f64 {
                let value = f64::from(value) / 255.0;
                if value <= 0.039_28 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            }
            0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
        }

        fn contrast(foreground: Color32, background: Color32) -> f64 {
            let foreground = luminance(foreground);
            let background = luminance(background);
            let lighter = foreground.max(background);
            let darker = foreground.min(background);
            (lighter + 0.05) / (darker + 0.05)
        }

        for background in [BG, CARD, SOFT] {
            let ratio = contrast(MUTED, background);
            assert!(
                ratio >= 4.5,
                "MUTED on {background:?} is {ratio:.2}:1, under the 4.5:1 body-text target"
            );
        }
    }

    #[test]
    fn settings_round_trip_through_eframe_storage() {
        let directory = std::env::temp_dir().join(format!(
            "convert2it-settings-round-trip-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let options = non_default_settings_options();
        let mut app = ConvertApp {
            output_directory: Some(directory.clone()),
            target: Some(OutputFormat::Jpeg),
            conversion_options: options,
            ..Default::default()
        };
        let mut storage = MemoryStorage::default();
        eframe::App::save(&mut app, &mut storage);

        let restored = ConvertApp::from_storage(Some(&storage));
        assert_eq!(restored.output_directory, Some(directory.clone()));
        assert_eq!(restored.target, Some(OutputFormat::Jpeg));
        assert_eq!(restored.conversion_options, options);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    #[ignore = "opens a native eframe window; run explicitly to verify real disk persistence"]
    fn settings_round_trip_through_real_eframe_disk_storage() {
        let directory = disk_test_directory("round-trip");
        let output_directory = directory.join("output");
        std::fs::create_dir_all(&output_directory).unwrap();
        let persistence = directory.join("app.ron");
        let options = non_default_settings_options();

        write_settings_via_real_eframe(
            &persistence,
            &output_directory,
            OutputFormat::Jpeg,
            options,
        );

        let blob = std::fs::read_to_string(&persistence)
            .expect("eframe should have written its disk-backed app.ron");
        assert!(
            blob.contains(SETTINGS_KEY),
            "eframe's app.ron did not contain the persisted settings key: {blob}"
        );

        let (restored_output, restored_target, restored_options) =
            read_settings_via_real_eframe(&persistence);
        assert_eq!(restored_output, Some(output_directory));
        assert_eq!(restored_target, Some(OutputFormat::Jpeg));
        assert_eq!(restored_options, options);

        let cases: [(&str, Option<&str>); 3] = [
            ("absent", None),
            ("empty", Some("")),
            ("corrupt", Some("{ this is not a RON map")),
        ];
        for (label, contents) in cases {
            let _ = std::fs::remove_file(&persistence);
            if let Some(contents) = contents {
                std::fs::write(&persistence, contents).unwrap();
            }
            let (output, target, options) = read_settings_via_real_eframe(&persistence);
            assert_eq!(output, default_output_directory(), "{label} blob");
            assert_eq!(target, None, "{label} blob");
            assert_eq!(options, ConversionOptions::default(), "{label} blob");
        }

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn persistence_path_is_writable_in_supplied_directory() {
        let root = disk_test_directory("persistence-path");
        let directory = root.join("data");
        let path =
            persistence_path_in(&directory).expect("persistence directory should be creatable");
        assert_eq!(path, directory.join("app.ron"));
        assert!(directory.is_dir());

        std::fs::write(&path, b"probe").expect("persistence file should be writable");
        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn production_native_options_pass_persistence_path() {
        let path = std::env::temp_dir().join(format!(
            "convert2it-native-options-{}.ron",
            std::process::id()
        ));
        let options = native_options(Some(path.clone()));
        assert_eq!(options.persistence_path, Some(path));
    }

    #[test]
    fn absent_settings_storage_uses_defaults() {
        let app = ConvertApp::from_storage(None);
        assert_eq!(app.output_directory, default_output_directory());
        assert_eq!(app.target, None);
        assert_eq!(app.conversion_options, ConversionOptions::default());
    }

    #[test]
    fn corrupt_settings_storage_falls_back_to_defaults_without_panicking() {
        for blob in [
            "{not valid json",
            "{}",
            "{\"output_directory\":42,\"target\":\"Bogus\"}",
        ] {
            let mut storage = MemoryStorage::default();
            eframe::Storage::set_string(&mut storage, SETTINGS_KEY, blob.to_owned());
            let app = ConvertApp::from_storage(Some(&storage));
            assert_eq!(app.output_directory, default_output_directory());
            assert_eq!(app.target, None);
            assert_eq!(app.conversion_options, ConversionOptions::default());
        }
    }

    fn test_conversion_job(receiver: Receiver<ConversionEvent>) -> ConversionJob {
        ConversionJob {
            receiver,
            cancel: Arc::new(AtomicBool::new(false)),
            completed: 0,
            total: 3,
            message: "Preparing...".to_owned(),
            source_label: "TXT".to_owned(),
            target_label: ".JSON".to_owned(),
        }
    }

    #[test]
    fn dashed_border_outline_follows_all_rounded_corners() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(100.0, 50.0));
        let points = rounded_rect_outline(rect, 10.0);

        assert_eq!(points.first(), points.last());
        assert!(points.iter().any(|point| point.x > 90.0 && point.y < 10.0));
        assert!(points.iter().any(|point| point.x < 10.0 && point.y > 40.0));
    }

    #[test]
    fn image_files_only_offer_supported_image_and_archive_outputs() {
        assert_eq!(
            available_targets(Some(InputFormat::Webp)),
            [
                OutputFormat::Png,
                OutputFormat::Jpeg,
                OutputFormat::Webp,
                OutputFormat::Gif,
                OutputFormat::Bmp,
                OutputFormat::Tiff,
                OutputFormat::Ico,
                OutputFormat::Ppm,
                OutputFormat::Pbm,
                OutputFormat::Pgm,
                OutputFormat::Pam,
                OutputFormat::Tga,
                OutputFormat::Qoi,
                OutputFormat::Hdr,
                OutputFormat::Farbfeld,
                OutputFormat::Pdf,
                OutputFormat::Zip,
                OutputFormat::Tar,
                OutputFormat::TarGz,
                OutputFormat::Gzip
            ]
        );
    }

    #[test]
    fn text_files_offer_the_encodings_supported_by_the_web_handler() {
        assert_eq!(
            available_targets(Some(InputFormat::Text)),
            [
                OutputFormat::Pdf,
                OutputFormat::Text,
                OutputFormat::TextUtf8NoBom,
                OutputFormat::TextUtf8Bom,
                OutputFormat::TextUtf16Le,
                OutputFormat::TextUtf16Be,
                OutputFormat::TextUtf32Le,
                OutputFormat::TextUtf32Be,
                OutputFormat::Csv,
                OutputFormat::Json,
                OutputFormat::Yaml,
                OutputFormat::Toml,
                OutputFormat::Zip,
                OutputFormat::Tar,
                OutputFormat::TarGz,
                OutputFormat::Gzip,
                OutputFormat::Epub
            ]
        );
    }

    #[test]
    fn epub_files_offer_text_and_markup_outputs_only() {
        // An ebook becomes text or a document, never an image: there is no
        // layout path, and one conversion is one input format to one output.
        // The archive targets every input offers stay available, so an EPUB can
        // still be re-packed as one.
        assert_eq!(
            available_targets(Some(InputFormat::Epub)),
            [
                OutputFormat::Text,
                OutputFormat::TextUtf8NoBom,
                OutputFormat::TextUtf8Bom,
                OutputFormat::TextUtf16Le,
                OutputFormat::TextUtf16Be,
                OutputFormat::TextUtf32Le,
                OutputFormat::TextUtf32Be,
                OutputFormat::Zip,
                OutputFormat::Tar,
                OutputFormat::TarGz,
                OutputFormat::Gzip,
                OutputFormat::Html,
                OutputFormat::Markdown,
            ]
        );
    }

    #[test]
    fn pdf_files_offer_raster_and_text_outputs() {
        assert_eq!(
            available_targets(Some(InputFormat::Pdf)),
            [
                OutputFormat::Png,
                OutputFormat::Jpeg,
                OutputFormat::Webp,
                OutputFormat::Gif,
                OutputFormat::Bmp,
                OutputFormat::Tiff,
                OutputFormat::Ico,
                OutputFormat::Ppm,
                OutputFormat::Pbm,
                OutputFormat::Pgm,
                OutputFormat::Pam,
                OutputFormat::Tga,
                OutputFormat::Qoi,
                OutputFormat::Hdr,
                OutputFormat::Farbfeld,
                OutputFormat::Text,
                OutputFormat::TextUtf8NoBom,
                OutputFormat::TextUtf8Bom,
                OutputFormat::TextUtf16Le,
                OutputFormat::TextUtf16Be,
                OutputFormat::TextUtf32Le,
                OutputFormat::TextUtf32Be,
                OutputFormat::Zip,
                OutputFormat::Tar,
                OutputFormat::TarGz,
                OutputFormat::Gzip,
                OutputFormat::SvgPage
            ]
        );
    }

    #[test]
    fn bmp_and_tiff_files_offer_supported_image_outputs() {
        let expected = [
            OutputFormat::Png,
            OutputFormat::Jpeg,
            OutputFormat::Webp,
            OutputFormat::Gif,
            OutputFormat::Bmp,
            OutputFormat::Tiff,
            OutputFormat::Ico,
            OutputFormat::Ppm,
            OutputFormat::Pbm,
            OutputFormat::Pgm,
            OutputFormat::Pam,
            OutputFormat::Tga,
            OutputFormat::Qoi,
            OutputFormat::Hdr,
            OutputFormat::Farbfeld,
            OutputFormat::Pdf,
            OutputFormat::Zip,
            OutputFormat::Tar,
            OutputFormat::TarGz,
            OutputFormat::Gzip,
        ];
        for input in [InputFormat::Bmp, InputFormat::Tiff] {
            assert_eq!(available_targets(Some(input)), expected);
        }
    }

    #[test]
    fn conversion_page_lists_all_formats_when_no_file_is_selected() {
        assert_eq!(available_targets(None), OutputFormat::ALL);
    }

    #[test]
    fn unknown_files_only_offer_archive_outputs() {
        assert_eq!(
            available_targets(Some(InputFormat::Any)),
            [
                OutputFormat::Files,
                OutputFormat::Zip,
                OutputFormat::Tar,
                OutputFormat::TarGz,
                OutputFormat::Gzip
            ]
        );
    }

    #[test]
    fn mixed_inputs_only_offer_archive_outputs() {
        assert_eq!(
            available_targets(Some(InputFormat::Mixed)),
            [
                OutputFormat::Files,
                OutputFormat::Zip,
                OutputFormat::Tar,
                OutputFormat::TarGz,
                OutputFormat::Gzip
            ]
        );
    }

    #[test]
    fn zip_files_only_offer_archive_outputs() {
        assert_eq!(
            available_targets(Some(InputFormat::Archive)),
            [
                OutputFormat::Zip,
                OutputFormat::Tar,
                OutputFormat::TarGz,
                OutputFormat::Gzip,
                OutputFormat::Cbz,
                OutputFormat::Files
            ]
        );
    }

    #[test]
    fn tar_gz_files_only_offer_archive_outputs() {
        assert_eq!(
            available_targets(Some(InputFormat::Archive)),
            [
                OutputFormat::Zip,
                OutputFormat::Tar,
                OutputFormat::TarGz,
                OutputFormat::Gzip,
                OutputFormat::Cbz,
                OutputFormat::Files
            ]
        );
    }

    #[test]
    fn rar_and_seven_zip_inputs_offer_archive_outputs() {
        for input in [InputFormat::Rar, InputFormat::SevenZip] {
            assert_eq!(
                available_targets(Some(input)),
                [
                    OutputFormat::Zip,
                    OutputFormat::Tar,
                    OutputFormat::TarGz,
                    OutputFormat::Gzip,
                    OutputFormat::Cbz,
                    OutputFormat::Files
                ]
            );
        }
    }
    #[test]
    fn audio_track_picker_defaults_to_auto_and_uses_probed_metadata() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sample-interleaved-tracks.mkv");
        let choices = audio_tracks_from_path(&path).unwrap();

        assert!(!audio_track_picker_is_visible(&[]));
        assert!(!audio_track_picker_is_visible(&choices[..1]));
        assert!(audio_track_picker_is_visible(&choices));
        assert_eq!(
            audio_track_selection_label(&ConversionOptions::default(), &choices),
            "Auto (first track)"
        );
        let first = audio_track_selection_label(
            &ConversionOptions {
                audio_track: Some(0),
                ..Default::default()
            },
            &choices,
        );
        assert_eq!(first, "#1 · stream 3 · eng · AC-3 · 5.1 · 48.0 kHz");
        let second = audio_track_selection_label(
            &ConversionOptions {
                audio_track: Some(1),
                ..Default::default()
            },
            &choices,
        );
        assert_eq!(second, "#2 · stream 4 · fra · AC-3 · 5.1 · 48.0 kHz");

        let mut no_language = choices[0].clone();
        no_language.language = None;
        no_language.index = 0;
        no_language.id = 1;
        assert_eq!(no_language.label(), "#1 · AC-3 · 5.1 · 48.0 kHz");
    }

    #[test]
    fn mp3_bitrate_is_a_non_default_setting() {
        assert!(!target_has_applied_setting(
            OutputFormat::Mp3,
            &ConversionOptions::default()
        ));
        let high_bitrate = ConversionOptions {
            mp3_bitrate: Mp3Bitrate::Kbps320,
            ..Default::default()
        };
        assert!(target_has_applied_setting(OutputFormat::Mp3, &high_bitrate));
        assert!(!target_has_applied_setting(
            OutputFormat::Wav,
            &high_bitrate
        ));
        assert_eq!(Mp3Bitrate::ALL.len(), 4);
        assert_eq!(Mp3Bitrate::Kbps320.kbps(), 320);
    }

    #[test]
    fn image_background_setting_is_scoped_to_alpha_less_targets() {
        for target in [
            OutputFormat::Jpeg,
            OutputFormat::Bmp,
            OutputFormat::Hdr,
            OutputFormat::Ppm,
            OutputFormat::Pbm,
            OutputFormat::Pgm,
            OutputFormat::Pdf,
        ] {
            assert!(target.uses_image_background(), "{target:?}");
        }
        for target in [
            OutputFormat::Png,
            OutputFormat::Webp,
            OutputFormat::Gif,
            OutputFormat::Tiff,
            OutputFormat::Ico,
            OutputFormat::Pam,
            OutputFormat::Tga,
            OutputFormat::Qoi,
            OutputFormat::Farbfeld,
        ] {
            assert!(!target.uses_image_background(), "{target:?}");
        }

        let custom_background = ConversionOptions {
            image_background: [1, 2, 3],
            ..Default::default()
        };
        assert!(target_has_applied_setting(
            OutputFormat::Jpeg,
            &custom_background
        ));
        assert!(target_has_applied_setting(
            OutputFormat::Bmp,
            &custom_background
        ));
        assert!(target_has_applied_setting(
            OutputFormat::Pdf,
            &custom_background
        ));
        assert!(!target_has_applied_setting(
            OutputFormat::Png,
            &custom_background
        ));
        assert!(!target_has_applied_setting(
            OutputFormat::Webp,
            &custom_background
        ));
        assert!(target_has_applied_setting(
            OutputFormat::Png,
            &ConversionOptions {
                image_apply_exif_orientation: false,
                ..Default::default()
            }
        ));
    }

    #[test]
    fn audio_bit_depth_settings_reach_encoders_and_flac_resets_float() {
        assert!(audio_bit_depth_choices(OutputFormat::Wav).contains(&AudioBitDepth::Float32));
        assert!(audio_bit_depth_choices(OutputFormat::Wav).contains(&AudioBitDepth::Bit24));
        assert!(!audio_bit_depth_choices(OutputFormat::Flac).contains(&AudioBitDepth::Float32));

        fn wav_format_and_bits(bytes: &[u8]) -> (u16, u16) {
            assert_eq!(&bytes[0..4], b"RIFF");
            assert_eq!(&bytes[12..16], b"fmt ");
            (
                u16::from_le_bytes(bytes[20..22].try_into().unwrap()),
                u16::from_le_bytes(bytes[34..36].try_into().unwrap()),
            )
        }

        fn flac_bits_per_sample(bytes: &[u8]) -> u16 {
            assert_eq!(&bytes[0..4], b"fLaC");
            let streaminfo = &bytes[8..8 + 34];
            let packed = u64::from_be_bytes(streaminfo[10..18].try_into().unwrap());
            (((packed >> 36) & 0x1F) + 1) as u16
        }

        let directory = std::env::temp_dir().join(format!(
            "convert2it-depth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let input = directory.join("tone.wav");
        fs::write(&input, tiny_wav()).unwrap();

        let encode = |requested: AudioBitDepth, target: OutputFormat| {
            let mut options = ConversionOptions {
                audio_bit_depth: requested,
                ..Default::default()
            };
            normalize_audio_bit_depth(&mut options.audio_bit_depth, target);
            let mut output = std::io::Cursor::new(Vec::new());
            convert_audio_file_to_writer(&input, "tone.wav", target, options, &mut output).unwrap();
            (options.audio_bit_depth, output.into_inner())
        };

        let (depth, wav24) = encode(AudioBitDepth::Bit24, OutputFormat::Wav);
        assert_eq!(depth, AudioBitDepth::Bit24);
        assert_eq!(wav_format_and_bits(&wav24), (1, 24));

        let (depth, wav_float) = encode(AudioBitDepth::Float32, OutputFormat::Wav);
        assert_eq!(depth, AudioBitDepth::Float32);
        assert_eq!(wav_format_and_bits(&wav_float), (3, 32));

        let (depth, flac24) = encode(AudioBitDepth::Bit24, OutputFormat::Flac);
        assert_eq!(depth, AudioBitDepth::Bit24);
        assert_eq!(flac_bits_per_sample(&flac24), 24);

        // Selecting float for WAV and then switching to FLAC must reset to a
        // supported depth instead of encoding a hidden invalid value.
        let (depth, flac_reset) = encode(AudioBitDepth::Float32, OutputFormat::Flac);
        assert_eq!(depth, AudioBitDepth::Bit16);
        assert_eq!(flac_bits_per_sample(&flac_reset), 16);

        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn audio_files_offer_audio_and_archive_outputs() {
        let expected = [
            OutputFormat::Wav,
            OutputFormat::Flac,
            OutputFormat::Aiff,
            OutputFormat::Au,
            OutputFormat::Opus,
            OutputFormat::Mp3,
            OutputFormat::Zip,
            OutputFormat::Tar,
            OutputFormat::TarGz,
            OutputFormat::Gzip,
        ];
        for input in [
            InputFormat::Mp3,
            InputFormat::Flac,
            InputFormat::Ogg,
            InputFormat::Opus,
            InputFormat::M4a,
            InputFormat::Aiff,
        ] {
            assert_eq!(available_targets(Some(input)), expected);
        }
    }

    #[test]
    fn video_files_offer_thumbnails_remux_and_audio_outputs() {
        assert_eq!(
            available_targets(Some(InputFormat::Video)),
            [
                OutputFormat::Png,
                OutputFormat::Jpeg,
                OutputFormat::Webp,
                OutputFormat::Wav,
                OutputFormat::Flac,
                OutputFormat::Aiff,
                OutputFormat::Au,
                OutputFormat::Opus,
                OutputFormat::Mp3,
                OutputFormat::Mp4,
                OutputFormat::Webm,
                OutputFormat::Zip,
                OutputFormat::Tar,
                OutputFormat::TarGz,
                OutputFormat::Gzip
            ]
        );
    }

    #[test]
    fn video_directory_conversion_reports_progress_and_writes_remux() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let directory =
            std::env::temp_dir().join(format!("convert2it-gui-video-test-{}", std::process::id()));
        let output_directory = directory.join("out");
        std::fs::create_dir_all(&output_directory).unwrap();
        let input = directory.join("sample-vp9-opus.mp4");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("testdata")
                .join("sample-vp9-opus.mp4"),
            &input,
        )
        .unwrap();
        let prepared = directory.join("prepared.webm");
        std::fs::write(&prepared, b"\x1aE\xdf\xa3gui-webm").unwrap();
        let mut env = EnvGuard(Vec::new());
        env.set("CONVERT2IT_FFMPEG", ffmpeg_stub())
            .set("STUB_OUTPUT", &prepared);

        let mut messages = Vec::new();
        let summary = convert_to_directory(
            &[input],
            OutputFormat::Webm,
            ConversionOptions::default(),
            &output_directory,
            &AtomicBool::new(false),
            |completed, _total, message| messages.push((completed, message)),
        )
        .unwrap();
        let written = output_directory.join("sample-vp9-opus.webm");
        assert_eq!(std::fs::read(&written).unwrap(), b"\x1aE\xdf\xa3gui-webm");
        assert_eq!(summary, format!("Created 1 file(s): {}", written.display()));
        assert!(messages
            .iter()
            .any(|(_, message)| message.starts_with("Reading")));
        assert!(messages
            .iter()
            .any(|(_, message)| message.contains("Converting")));
        assert!(messages
            .iter()
            .any(|(_, message)| message.starts_with("Saving")));
        assert!(messages
            .iter()
            .any(|(_, message)| message.starts_with("Saved")));
        assert_eq!(messages.last().unwrap().0, 3);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// A PDF with one page per size, each carrying a filled rectangle. Built
    /// here so the progress test does not depend on a committed multi-page
    /// fixture.
    fn multi_page_pdf(pages: &[(f32, f32)]) -> Vec<u8> {
        use pdf_writer::{Content, Pdf, Rect, Ref};
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

    #[test]
    fn multi_page_pdf_reports_a_total_that_matches_the_work() {
        let directory =
            std::env::temp_dir().join(format!("convert2it-gui-pdf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let output_directory = directory.join("out");
        std::fs::create_dir_all(&output_directory).unwrap();
        let input = directory.join("report.pdf");
        std::fs::write(
            &input,
            multi_page_pdf(&[(200.0, 100.0), (100.0, 50.0), (80.0, 80.0)]),
        )
        .unwrap();

        let mut messages = Vec::new();
        let summary = convert_to_directory(
            &[input],
            OutputFormat::Png,
            ConversionOptions::default(),
            &output_directory,
            &AtomicBool::new(false),
            |completed, total, message| messages.push((completed, total, message)),
        )
        .unwrap();

        // Three pages means three output files: a read step and a convert step
        // on top of one save per page, so five steps and never three.
        let totals: std::collections::BTreeSet<usize> =
            messages.iter().map(|(_, total, _)| *total).collect();
        assert_eq!(totals, std::iter::once(5).collect());
        assert_eq!(messages.last().unwrap().0, 5);
        assert!(summary.contains("Created 3 file(s)"));
        for page in 0..3 {
            assert!(output_directory
                .join(format!("report_{page}.png"))
                .is_file());
        }
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn real_ffmpeg_gui_video_when_available() {
        let _lock = VIDEO_ENV_LOCK.lock().unwrap();
        let Some(ffmpeg) = ffmpeg_for_real_test("GUI test") else {
            return;
        };
        let directory =
            std::env::temp_dir().join(format!("convert2it-gui-real-video-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let input = directory.join("synthetic.mp4");
        generate_real_video(&ffmpeg, &input);
        let output_directory = directory.join("out");
        std::fs::create_dir_all(&output_directory).unwrap();
        let mut env = EnvGuard(Vec::new());
        env.set("CONVERT2IT_FFMPEG", &ffmpeg);

        let mut messages = Vec::new();
        let summary = convert_to_directory(
            std::slice::from_ref(&input),
            OutputFormat::Webm,
            ConversionOptions::default(),
            &output_directory,
            &AtomicBool::new(false),
            |completed, _total, message| messages.push((completed, message)),
        )
        .unwrap();
        let webm = std::fs::read(output_directory.join("synthetic.webm")).unwrap();
        assert!(!webm.is_empty());
        assert!(webm.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]));
        assert!(summary.contains("synthetic.webm"));
        assert!(messages
            .iter()
            .any(|(_, message)| message.contains("Converting")));

        let back_directory = directory.join("back");
        std::fs::create_dir_all(&back_directory).unwrap();
        let back_summary = convert_to_directory(
            &[output_directory.join("synthetic.webm")],
            OutputFormat::Mp4,
            ConversionOptions::default(),
            &back_directory,
            &AtomicBool::new(false),
            |_, _, _| {},
        )
        .unwrap();
        assert!(back_summary.contains("synthetic.mp4"));
        let back = std::fs::read(back_directory.join("synthetic.mp4")).unwrap();
        assert_eq!(back.get(4..8), Some(b"ftyp".as_slice()));

        for (target, format, extension) in [
            (OutputFormat::Png, image::ImageFormat::Png, "png"),
            (OutputFormat::Jpeg, image::ImageFormat::Jpeg, "jpg"),
            (OutputFormat::Webp, image::ImageFormat::WebP, "webp"),
        ] {
            let summary = convert_to_directory(
                std::slice::from_ref(&input),
                target,
                ConversionOptions {
                    video_frame_time_ms: 500,
                    video_thumbnail_width: 160,
                    ..Default::default()
                },
                &output_directory,
                &AtomicBool::new(false),
                |_, _, _| {},
            )
            .unwrap();
            assert!(summary.contains(&format!("synthetic.{extension}")));
            let bytes =
                std::fs::read(output_directory.join(format!("synthetic.{extension}"))).unwrap();
            assert_eq!(image::guess_format(&bytes).unwrap(), format);
            let decoded = image::load_from_memory(&bytes).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (160, 120));
        }

        let empty_output = directory.join("empty-out");
        std::fs::create_dir_all(&empty_output).unwrap();
        let error = convert_to_directory(
            std::slice::from_ref(&input),
            OutputFormat::Png,
            ConversionOptions {
                video_frame_time_ms: 10_000,
                video_thumbnail_width: 160,
                ..Default::default()
            },
            &empty_output,
            &AtomicBool::new(false),
            |_, _, _| {},
        )
        .unwrap_err();
        match error {
            ConversionFailure::Message(message) => {
                assert!(message.contains("ffmpeg produced no output"), "{message}");
            }
            other => panic!("expected the empty-output error, got {other:?}"),
        }
        assert!(std::fs::read_dir(&empty_output).unwrap().next().is_none());

        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn yaml_and_toml_files_offer_data_outputs() {
        let expected = [
            OutputFormat::Text,
            OutputFormat::Csv,
            OutputFormat::Json,
            OutputFormat::Yaml,
            OutputFormat::Toml,
            OutputFormat::Zip,
            OutputFormat::Tar,
            OutputFormat::TarGz,
            OutputFormat::Gzip,
        ];
        for input in [InputFormat::Yaml, InputFormat::Toml] {
            assert_eq!(available_targets(Some(input)), expected);
        }
    }

    #[test]
    fn manual_input_override_takes_precedence_over_detection() {
        let mut app = ConvertApp {
            inputs: vec![PathBuf::from("photo.png")],
            input_formats: vec![InputFormat::Png],
            ..Default::default()
        };
        assert_eq!(app.effective_input_format(), Some(InputFormat::Png));

        app.input_format_override = Some(InputFormat::Mp3);
        assert_eq!(app.effective_input_format(), Some(InputFormat::Mp3));
        assert_eq!(
            available_targets(app.effective_input_format()),
            [
                OutputFormat::Wav,
                OutputFormat::Flac,
                OutputFormat::Aiff,
                OutputFormat::Au,
                OutputFormat::Opus,
                OutputFormat::Mp3,
                OutputFormat::Zip,
                OutputFormat::Tar,
                OutputFormat::TarGz,
                OutputFormat::Gzip
            ]
        );
    }

    #[test]
    fn input_picker_starts_with_detected_formats_and_can_show_all() {
        let mut app = ConvertApp {
            inputs: vec![PathBuf::from("photo.png")],
            input_formats: vec![InputFormat::Png],
            ..Default::default()
        };
        assert_eq!(app.input_format_choices(), vec![InputFormat::Png]);

        app.input_format_show_all = true;
        let choices = app.input_format_choices();
        assert_eq!(choices.first(), Some(&InputFormat::Any));
        assert!(choices.len() > 1);
        assert!(choices.contains(&InputFormat::Mp3));
        assert!(choices.contains(&InputFormat::Aiff));
        assert!(choices.contains(&InputFormat::Video));
        assert!(!choices.contains(&InputFormat::Mixed));
    }

    #[test]
    fn choosing_the_effective_format_clears_the_override() {
        let mut app = ConvertApp {
            inputs: vec![PathBuf::from("photo.png")],
            input_formats: vec![InputFormat::Png],
            ..Default::default()
        };

        app.show_input_format_picker = true;
        app.choose_input_format(InputFormat::Png);
        assert_eq!(app.input_format_override, None);
        assert!(!app.show_input_format_picker);

        app.choose_input_format(InputFormat::Mp3);
        assert_eq!(app.input_format_override, Some(InputFormat::Mp3));
        assert!(!app.show_input_format_picker);
    }

    #[test]
    fn input_format_labels_describe_unknown_and_mixed_selections() {
        let mut app = ConvertApp {
            inputs: vec![PathBuf::from("mystery.bin")],
            input_formats: vec![InputFormat::Any],
            ..Default::default()
        };
        assert_eq!(app.detected_input_label(), "Unknown format");

        app.input_formats = vec![InputFormat::Png, InputFormat::Text];
        assert_eq!(app.detected_input_label(), "Mixed formats");

        app.input_formats = vec![InputFormat::Png];
        assert_eq!(app.detected_input_label(), "PNG image");
    }

    #[test]
    fn completed_conversions_open_a_notice_instead_of_inline_status() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(ConversionEvent::Finished(Ok(
                "Created 1 file(s): output.png".to_owned(),
            )))
            .unwrap();
        let mut app = ConvertApp {
            conversion_job: Some(test_conversion_job(receiver)),
            status: Some(("Converting your files...".to_owned(), false)),
            ..Default::default()
        };

        app.poll_conversion();

        assert_eq!(app.status, None);
        assert_eq!(
            app.conversion_notice,
            Some((
                "Conversion complete!".to_owned(),
                "Created 1 file(s): output.png".to_owned(),
                false
            ))
        );
        assert!(app.conversion_job.is_none());
    }

    #[test]
    fn progress_events_update_the_active_job() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(ConversionEvent::Progress {
                completed: 2,
                total: 5,
                message: "Saving output.png...".to_owned(),
            })
            .unwrap();
        let mut app = ConvertApp {
            conversion_job: Some(test_conversion_job(receiver)),
            ..Default::default()
        };

        app.poll_conversion();

        let job = app.conversion_job.as_ref().unwrap();
        assert_eq!(job.completed, 2);
        assert_eq!(job.total, 5);
        assert_eq!(job.message, "Saving output.png...");
    }

    #[test]
    fn cancellation_clears_the_job_and_shows_a_notice() {
        let (sender, receiver) = mpsc::channel();
        sender.send(ConversionEvent::Cancelled).unwrap();
        let mut app = ConvertApp {
            conversion_job: Some(test_conversion_job(receiver)),
            ..Default::default()
        };

        app.poll_conversion();

        assert!(app.conversion_job.is_none());
        assert_eq!(
            app.conversion_notice,
            Some((
                "Conversion cancelled".to_owned(),
                "No files were written.".to_owned(),
                false
            ))
        );
    }

    #[test]
    fn requesting_cancel_sets_the_worker_flag() {
        let (_sender, receiver) = mpsc::channel();
        let mut app = ConvertApp {
            conversion_job: Some(test_conversion_job(receiver)),
            ..Default::default()
        };

        app.cancel_conversion();

        let job = app.conversion_job.as_ref().unwrap();
        assert!(job.cancel.load(Ordering::Relaxed));
        assert_eq!(job.message, "Cancelling after the current step...");
    }

    #[test]
    fn pdf_image_layout_options_trigger_reset_state() {
        assert!(!target_has_applied_setting(
            OutputFormat::Pdf,
            &ConversionOptions::default()
        ));

        let landscape = ConversionOptions {
            pdf_orientation: PdfOrientation::Landscape,
            ..Default::default()
        };
        assert!(target_has_applied_setting(OutputFormat::Pdf, &landscape));

        let cover = ConversionOptions {
            pdf_image_fit: PdfImageFit::Cover,
            ..Default::default()
        };
        assert!(target_has_applied_setting(OutputFormat::Pdf, &cover));
    }

    fn tiny_wav() -> Vec<u8> {
        let frames = 800_u32;
        let data_size = frames * 4;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_size).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&8_000_u32.to_le_bytes());
        wav.extend_from_slice(&32_000_u32.to_le_bytes());
        wav.extend_from_slice(&4_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_size.to_le_bytes());
        wav.resize(wav.len() + data_size as usize, 0);
        wav
    }

    #[test]
    fn wav_conversion_streams_to_the_output_file() {
        let directory = std::env::temp_dir().join(format!(
            "convert2it-wav-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let input = directory.join("tone.wav");
        fs::write(&input, tiny_wav()).unwrap();
        let output_directory = directory.join("out");
        let mut last_step = 0;
        let summary = convert_to_directory(
            std::slice::from_ref(&input),
            OutputFormat::Wav,
            ConversionOptions::default(),
            &output_directory,
            &AtomicBool::new(false),
            |completed, _total, _message| last_step = completed,
        )
        .unwrap();

        assert!(summary.contains("tone.wav"));
        let output = fs::read(output_directory.join("tone.wav")).unwrap();
        assert_eq!(&output[..4], b"RIFF");
        assert!(output.len() > 44);
        assert_eq!(last_step, 2);
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn cancellation_before_start_writes_nothing() {
        let cancel = AtomicBool::new(true);
        let result = convert_to_directory(
            &[PathBuf::from("input.txt")],
            OutputFormat::Text,
            ConversionOptions::default(),
            Path::new("unused"),
            &cancel,
            |_, _, _| {},
        );

        assert!(matches!(result, Err(ConversionFailure::Cancelled)));
    }
}
