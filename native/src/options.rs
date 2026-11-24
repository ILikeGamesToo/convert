// SPDX-License-Identifier: GPL-2.0-or-later

//! User-selected encoding options.

use crate::{AudioBitDepth, AudioChannels, Mp3Bitrate};
use crate::{PdfImageFit, PdfOrientation, PdfPageSize};
use serde::{Deserialize, Serialize};

/// User-selected encoding options. `None` keeps the encoder's default.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConversionOptions {
    pub jpeg_quality: Option<u8>,
    pub png_compression: Option<u8>,
    pub archive_compression: Option<u8>,
    pub audio_sample_rate: Option<u32>,
    pub audio_channels: Option<AudioChannels>,
    /// Zero-based index of the audio track to decode. `None` uses the first.
    pub audio_track: Option<usize>,
    pub audio_bit_depth: AudioBitDepth,
    /// Bitrate for MP3 output. Other audio formats ignore this.
    pub mp3_bitrate: Mp3Bitrate,
    /// Background used when an image with transparency is saved to a format
    /// without an alpha channel.
    pub image_background: [u8; 3],
    /// Apply the EXIF orientation tag when decoding photos.
    pub image_apply_exif_orientation: bool,
    pub pdf_page_size: PdfPageSize,
    pub pdf_orientation: PdfOrientation,
    pub pdf_image_fit: PdfImageFit,
    pub pdf_font_size: u32,
    pub pdf_margin: u32,
    /// Frame timestamp for video thumbnails. `0` starts at the beginning.
    pub video_frame_time_ms: u32,
    /// Optional width for video thumbnails. `0` keeps the source width.
    pub video_thumbnail_width: u32,
}

impl Default for ConversionOptions {
    fn default() -> Self {
        Self {
            jpeg_quality: None,
            png_compression: None,
            archive_compression: None,
            audio_sample_rate: None,
            audio_channels: None,
            audio_track: None,
            audio_bit_depth: AudioBitDepth::Bit16,
            mp3_bitrate: Self::DEFAULT_MP3_BITRATE,
            image_background: Self::DEFAULT_IMAGE_BACKGROUND,
            image_apply_exif_orientation: true,
            pdf_page_size: Self::DEFAULT_PDF_PAGE_SIZE,
            pdf_orientation: Self::DEFAULT_PDF_ORIENTATION,
            pdf_image_fit: Self::DEFAULT_PDF_IMAGE_FIT,
            pdf_font_size: Self::DEFAULT_PDF_FONT_SIZE,
            pdf_margin: Self::DEFAULT_PDF_MARGIN,
            video_frame_time_ms: 0,
            video_thumbnail_width: 0,
        }
    }
}

impl ConversionOptions {
    pub const DEFAULT_JPEG_QUALITY: u8 = 75;
    pub const DEFAULT_COMPRESSION_LEVEL: u8 = 6;
    pub const DEFAULT_AUDIO_SAMPLE_RATE: u32 = 44_100;
    pub const DEFAULT_MP3_BITRATE: Mp3Bitrate = Mp3Bitrate::Kbps192;
    pub const MIN_AUDIO_SAMPLE_RATE: u32 = 1_000;
    /// Opaque white, so transparent areas remain visible on common backgrounds.
    pub const DEFAULT_IMAGE_BACKGROUND: [u8; 3] = [255, 255, 255];
    /// FLAC's encoder accepts at most 96 kHz, so the shared audio control stays within that range.
    pub const MAX_AUDIO_SAMPLE_RATE: u32 = 96_000;
    pub const DEFAULT_PDF_PAGE_SIZE: PdfPageSize = PdfPageSize::A4;
    pub const DEFAULT_PDF_ORIENTATION: PdfOrientation = PdfOrientation::Portrait;
    pub const DEFAULT_PDF_IMAGE_FIT: PdfImageFit = PdfImageFit::Contain;
    pub const DEFAULT_PDF_FONT_SIZE: u32 = 11;
    pub const MIN_PDF_FONT_SIZE: u32 = 1;
    pub const MAX_PDF_FONT_SIZE: u32 = 200;
    pub const DEFAULT_PDF_MARGIN: u32 = 72;
    pub const MIN_PDF_MARGIN: u32 = 0;
    pub const MAX_PDF_MARGIN: u32 = 144;
    pub const MAX_VIDEO_FRAME_TIME_MS: u32 = 86_400_000;
    pub const MAX_VIDEO_THUMBNAIL_WIDTH: u32 = 4_096;
}
