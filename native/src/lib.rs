// SPDX-License-Identifier: GPL-2.0-or-later

mod archive;
mod audio;
mod audio_tags;
mod container;
mod convert;
mod data;
mod document;
mod error;
mod ffmpeg;
mod fonts;
mod format;
mod image_metadata;
mod images;
pub mod input_extensions;
pub mod input_format;
mod options;
mod pdf;
mod shared;
mod streaming;
#[cfg(test)]
mod test_support;
mod text;
mod video;

pub use archive::extract_archive;
pub use audio::{
    audio_output_name, audio_tracks, audio_tracks_from_path, convert_audio_to_wav_writer,
    wav_output_name, AudioBitDepth, AudioChannels, AudioTrackInfo, Mp3Bitrate,
};
pub use convert::{
    convert_batch, convert_batch_with_options, convert_batch_with_video_config, convert_bytes,
    convert_bytes_with_options, convert_bytes_with_video_config, convert_bytes_with_video_control,
    planned_output_count, ConvertedFile,
};
pub use document::{PdfImageFit, PdfOrientation, PdfPageSize};
pub use error::ConvertError;
pub use format::{native_format_registry, FormatHandler, OutputFormat};
pub use input_extensions::supported_input_extensions;
pub use options::ConversionOptions;
pub use streaming::{convert_audio_file_to_writer, convert_audio_file_to_writer_with_cancel};
pub use video::{
    native_video_routes, VideoControl, VideoProgress, VideoRoute, VideoRouteInfo, VideoToolConfig,
};
