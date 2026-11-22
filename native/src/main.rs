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

mod input_format;

use convert_to_it_native::{
    audio_output_name, audio_tracks_from_path, convert_audio_file_to_writer_with_cancel,
    convert_batch_with_options, convert_bytes_with_options, supported_input_extensions,
    AudioBitDepth, AudioChannels, AudioTrackInfo, ConversionOptions, ConvertError, OutputFormat,
    PdfImageFit, PdfOrientation, PdfPageSize,
};
use eframe::egui::{self, Align, Color32, FontId, Layout, RichText, Stroke, Vec2};
use input_format::{common_format, detect, InputFormat};
use rfd::FileDialog;

const BG: Color32 = Color32::from_rgb(25, 25, 25);
const CARD: Color32 = Color32::from_rgb(32, 32, 32);
const SOFT: Color32 = Color32::from_rgb(37, 37, 37);
const BORDER: Color32 = Color32::from_rgb(55, 55, 55);
const PRIMARY: Color32 = Color32::from_rgb(35, 131, 226);
const TEXT: Color32 = Color32::from_rgb(209, 209, 209);
const MUTED: Color32 = Color32::from_rgb(128, 128, 128);
const FOOTER_TEXT: Color32 = Color32::from_rgb(94, 94, 94);
const GREEN: Color32 = Color32::from_rgb(83, 190, 132);
const RED: Color32 = Color32::from_rgb(235, 93, 93);
const ACTION_BAR_HEIGHT: f32 = 70.0;
const FORMAT_LIST_SCROLLBAR_GUTTER: f32 = 15.0;
const HOME_CARD_HEIGHT: f32 = 354.36;
const HOME_FOOTER_HEIGHT: f32 = 27.78;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([767.0, 675.0])
            .with_min_inner_size([600.0, 600.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "Convert to it!",
        options,
        Box::new(|creation_context| {
            apply_theme(&creation_context.egui_ctx);
            let app = ConvertApp {
                output_directory: default_output_directory(),
                ..Default::default()
            };
            Ok(Box::new(app))
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
    Progress { completed: usize, message: String },
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
    Archive,
}

impl FormatCategory {
    const ALL: [Self; 6] = [
        Self::Image,
        Self::Document,
        Self::Text,
        Self::Data,
        Self::Audio,
        Self::Archive,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Image => "Image",
            Self::Document => "Document",
            Self::Text => "Text",
            Self::Data => "Data",
            Self::Audio => "Audio",
            Self::Archive => "Archive",
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
        OutputFormat::Zip | OutputFormat::Tar | OutputFormat::TarGz | OutputFormat::Gzip => {
            FormatCategory::Archive
        }
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
    }
}

const ARCHIVE_TARGETS: [OutputFormat; 4] = [
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
    let image_changed = options.image_background != ConversionOptions::DEFAULT_IMAGE_BACKGROUND
        || !options.image_apply_exif_orientation;
    match target {
        OutputFormat::Jpeg => options.jpeg_quality.is_some() || image_changed,
        OutputFormat::Png => options.png_compression.is_some() || image_changed,
        OutputFormat::Zip | OutputFormat::TarGz | OutputFormat::Gzip => {
            options.archive_compression.is_some()
        }
        OutputFormat::Wav | OutputFormat::Flac => {
            options.audio_sample_rate.is_some()
                || options.audio_channels.is_some()
                || options.audio_track.is_some()
                || options.audio_bit_depth != AudioBitDepth::Bit16
        }
        OutputFormat::Aiff | OutputFormat::Au | OutputFormat::Opus | OutputFormat::Mp3 => {
            options.audio_sample_rate.is_some()
                || options.audio_channels.is_some()
                || options.audio_track.is_some()
        }
        OutputFormat::Pdf => {
            options.pdf_page_size != ConversionOptions::DEFAULT_PDF_PAGE_SIZE
                || options.pdf_orientation != ConversionOptions::DEFAULT_PDF_ORIENTATION
                || options.pdf_image_fit != ConversionOptions::DEFAULT_PDF_IMAGE_FIT
                || options.pdf_font_size != ConversionOptions::DEFAULT_PDF_FONT_SIZE
                || options.pdf_margin != ConversionOptions::DEFAULT_PDF_MARGIN
                || image_changed
        }
        target if format_category(target) == FormatCategory::Image => image_changed,
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

fn audio_bit_depth_setting(ui: &mut egui::Ui, depth: &mut AudioBitDepth, target: OutputFormat) {
    let choices: &[AudioBitDepth] = if target == OutputFormat::Flac {
        &[AudioBitDepth::Bit16, AudioBitDepth::Bit24]
    } else {
        &AudioBitDepth::ALL
    };
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

fn image_settings(ui: &mut egui::Ui, options: &mut ConversionOptions) {
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

fn audio_track_selection_label(options: &ConversionOptions, choices: &[AudioTrackInfo]) -> String {
    options
        .audio_track
        .and_then(|index| choices.get(index))
        .map(AudioTrackInfo::label)
        .unwrap_or_else(|| "Auto (first track)".to_owned())
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
}

impl ConvertApp {
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
            let response = ui.interact(
                button,
                ui.make_persistent_id(("upload-action", id)),
                egui::Sense::click(),
            );
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
            let tooltip = match *id {
                "settings" => "App settings",
                "help" => "Help & Info",
                _ => "View supported formats",
            };
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
                                    .color(FOOTER_TEXT),
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
                image_settings(ui, &mut self.conversion_options);
            }
            OutputFormat::Png => {
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
                image_settings(ui, &mut self.conversion_options);
            }
            OutputFormat::Zip | OutputFormat::TarGz | OutputFormat::Gzip => {
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
                if matches!(target, OutputFormat::Wav | OutputFormat::Flac) {
                    ui.add_space(14.0);
                    audio_bit_depth_setting(
                        ui,
                        &mut self.conversion_options.audio_bit_depth,
                        target,
                    );
                }
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
                    image_settings(ui, &mut self.conversion_options);
                }
            }
            _ if format_category(target) == FormatCategory::Image => {
                settings_section_heading(ui, "Image", None);
                ui.add_space(10.0);
                image_settings(ui, &mut self.conversion_options);
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
            response.on_hover_text(if *id == "settings" {
                "App settings"
            } else {
                "Conversion settings"
            });
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
                    source_label,
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
                let fraction = (job.completed as f32 / job.total.max(1) as f32).clamp(0.0, 1.0);
                ui.add(egui::ProgressBar::new(fraction).show_percentage());
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
        let total = conversion_step_total(inputs.len(), target);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        self.conversion_job = Some(ConversionJob {
            receiver,
            cancel,
            completed: 0,
            total,
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
                |completed, message| {
                    let _ = progress_sender.send(ConversionEvent::Progress { completed, message });
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
                Ok(ConversionEvent::Progress { completed, message }) => {
                    if let Some(job) = self.conversion_job.as_mut() {
                        job.completed = completed;
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
                FOOTER_TEXT
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
        FormatCategory::Archive => paint_archive_icon(painter, rect, color),
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
        FormatCategory::Archive => paint_archive_icon(painter, rect, GREEN),
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

fn conversion_step_total(input_count: usize, target: OutputFormat) -> usize {
    if matches!(
        target,
        OutputFormat::Zip | OutputFormat::Tar | OutputFormat::TarGz
    ) {
        input_count.saturating_add(2)
    } else if format_category(target) == FormatCategory::Audio {
        input_count.saturating_mul(2)
    } else {
        input_count.saturating_mul(3)
    }
}

fn convert_to_directory(
    inputs: &[PathBuf],
    target: OutputFormat,
    options: ConversionOptions,
    output_directory: &Path,
    cancel: &AtomicBool,
    mut report: impl FnMut(usize, String),
) -> Result<String, ConversionFailure> {
    let mut completed = 0;
    let mut written = Vec::new();
    if matches!(
        target,
        OutputFormat::Zip | OutputFormat::Tar | OutputFormat::TarGz
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
        let outputs = match convert_bytes_with_options(file_name(path), &bytes, target, options) {
            Ok(outputs) => outputs,
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
                OutputFormat::Gzip
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
                OutputFormat::Gzip
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
                OutputFormat::Gzip
            ]
        );
    }

    #[test]
    fn audio_track_picker_defaults_to_auto_and_uses_probed_metadata() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sample-interleaved-tracks.mkv");
        let choices = audio_tracks_from_path(&path).unwrap();

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
            InputFormat::Video,
        ] {
            assert_eq!(available_targets(Some(input)), expected);
        }
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
            |completed, _| last_step = completed,
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
            |_, _| {},
        );

        assert!(matches!(result, Err(ConversionFailure::Cancelled)));
    }
}
