// SPDX-License-Identifier: GPL-2.0-or-later

//! Conversion entry points that route an input to the per-medium handlers.

use crate::archive::{create_tar, create_zip, extract_archive, gzip_bytes, MAX_ARCHIVE_ENTRIES};
use crate::audio::{
    apply_audio_options, decode_audio, encode_aiff, encode_au, encode_flac, encode_mp3,
    encode_opus, encode_wav, mp3_sample_rate, opus_sample_rate, preserve_flac_tags,
    preserve_mp3_id3v2,
};
use crate::document::{extract_pdf_text, image_to_pdf, text_to_pdf};
use crate::error::ConvertError;
use crate::fonts::{convert_font, sniff as sniff_font};
use crate::format::OutputFormat;
use crate::images::{encode_image, load_image, preserve_jpeg_exif, rasterize_svg};
use crate::options::ConversionOptions;
use crate::pdf;
use crate::shared::{
    file_extension, is_audio_extension, is_font_extension, is_image_input_extension,
    is_pdf_extension, is_video_input_extension, output_name, page_output_name, safe_file_name,
    unsupported,
};
use crate::text::{convert_text, encode_text};
use crate::video::{convert_video, extract_video_frame, VideoControl, VideoToolConfig};
use std::{collections::HashSet, path::PathBuf};

#[derive(Debug)]
pub struct ConvertedFile {
    pub name: PathBuf,
    pub bytes: Vec<u8>,
}

pub fn convert_bytes(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    convert_bytes_with_options(input_name, input, target, ConversionOptions::default())
}

pub fn convert_bytes_with_options(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    options: ConversionOptions,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    convert_bytes_with_video_config(
        input_name,
        input,
        target,
        options,
        &VideoToolConfig::default(),
    )
}

/// Conversion entry point with an explicit ffmpeg configuration. The normal
/// entry points resolve `CONVERT2IT_FFMPEG`/`FFMPEG` and PATH themselves.
pub fn convert_bytes_with_video_config(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    options: ConversionOptions,
    video: &VideoToolConfig,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    convert_bytes_with_video_control(input_name, input, target, options, video, None)
}

/// Conversion entry point that accepts the reserved transcode progress and
/// cancellation hooks. This slice's remux and thumbnail paths are synchronous,
/// so the hooks are ignored; non-video conversions ignore them as well.
pub fn convert_bytes_with_video_control(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    options: ConversionOptions,
    video: &VideoToolConfig,
    _control: Option<&mut VideoControl<'_>>,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    let extension = file_extension(input_name);
    if target == OutputFormat::Files {
        return extract_archive(input_name, input);
    }
    if target == OutputFormat::Zip {
        let name = safe_file_name(input_name);
        let entries = [(PathBuf::from(name), input.to_vec())];
        let bytes = create_zip(&entries, options.archive_compression)?;
        return Ok(vec![ConvertedFile {
            name: output_name(input_name, target.extension()),
            bytes,
        }]);
    }
    if target == OutputFormat::Tar || target == OutputFormat::TarGz {
        let name = safe_file_name(input_name);
        let entries = [(PathBuf::from(name), input.to_vec())];
        let gzip = target == OutputFormat::TarGz;
        let bytes = create_tar(&entries, gzip, options.archive_compression)?;
        return Ok(vec![ConvertedFile {
            name: output_name(input_name, target.extension()),
            bytes,
        }]);
    }
    if target == OutputFormat::Gzip {
        return Ok(vec![ConvertedFile {
            name: PathBuf::from(format!("{}.gz", safe_file_name(input_name))),
            bytes: gzip_bytes(input, options.archive_compression)?,
        }]);
    }
    if target.is_font_target() {
        if !is_font_extension(&extension) && sniff_font(input).is_none() {
            return Err(unsupported(&extension, target));
        }
        return Ok(vec![ConvertedFile {
            name: output_name(input_name, target.extension()),
            bytes: convert_font(input, target)?,
        }]);
    }

    if is_pdf_extension(&extension) {
        if target.image_format().is_some() {
            return render_pdf_pages(input_name, input, target, options);
        }
        let text = extract_pdf_text(input)?;
        let bytes = match target {
            OutputFormat::Text
            | OutputFormat::TextUtf8NoBom
            | OutputFormat::TextUtf8Bom
            | OutputFormat::TextUtf16Le
            | OutputFormat::TextUtf16Be
            | OutputFormat::TextUtf32Le
            | OutputFormat::TextUtf32Be => encode_text(&text, target)?,
            _ => return Err(unsupported(&extension, target)),
        };
        return Ok(vec![ConvertedFile {
            name: output_name(input_name, target.extension()),
            bytes,
        }]);
    }

    if target.is_video_target() {
        if !is_video_input_extension(&extension) {
            return Err(unsupported(&extension, target));
        }
        return convert_video(input_name, input, target, video);
    }

    if target.image_format().is_some() && is_video_input_extension(&extension) {
        return extract_video_frame(input_name, input, target, options, video);
    }

    if target == OutputFormat::Pdf {
        let bytes = if extension == "txt" {
            text_to_pdf(input, options)?
        } else if is_image_input_extension(&extension) {
            image_to_pdf(input, &extension, options)?
        } else {
            return Err(unsupported(&extension, target));
        };
        return Ok(vec![ConvertedFile {
            name: output_name(input_name, target.extension()),
            bytes,
        }]);
    }

    if target.image_format().is_some() {
        if !is_image_input_extension(&extension) {
            return Err(unsupported(&extension, target));
        }
        let image = if extension == "svg" {
            rasterize_svg(input)?
        } else {
            load_image(input, options)?
        };
        let bytes = encode_image(image, target, options)?;
        let bytes = preserve_jpeg_exif(input, bytes, options);
        return Ok(vec![ConvertedFile {
            name: output_name(input_name, target.extension()),
            bytes,
        }]);
    }

    if matches!(
        target,
        OutputFormat::Wav
            | OutputFormat::Flac
            | OutputFormat::Aiff
            | OutputFormat::Au
            | OutputFormat::Opus
            | OutputFormat::Mp3
    ) {
        if !is_audio_extension(&extension) {
            return Err(unsupported(&extension, target));
        }
        // Opus only accepts 8/12/16/24/48 kHz. Map the requested rate before
        // the shared audio options resample so it goes straight to the rate the
        // encoder uses instead of resampling twice.
        let audio_options = match target {
            OutputFormat::Opus => ConversionOptions {
                audio_sample_rate: options.audio_sample_rate.map(opus_sample_rate),
                ..options
            },
            OutputFormat::Mp3 => ConversionOptions {
                audio_sample_rate: options.audio_sample_rate.map(mp3_sample_rate),
                ..options
            },
            _ => options,
        };
        let audio = apply_audio_options(
            decode_audio(input, &extension, options.audio_track)?,
            audio_options,
        )?;
        let bytes = match target {
            OutputFormat::Wav => encode_wav(&audio, options.audio_bit_depth)?,
            OutputFormat::Flac => encode_flac(&audio, options.audio_bit_depth)?,
            OutputFormat::Aiff => encode_aiff(&audio)?,
            OutputFormat::Au => encode_au(&audio)?,
            OutputFormat::Opus => encode_opus(audio)?,
            OutputFormat::Mp3 => encode_mp3(audio, options.mp3_bitrate)?,
            _ => unreachable!("only audio targets reach this branch"),
        };
        let bytes = match target {
            OutputFormat::Mp3 => preserve_mp3_id3v2(input, bytes),
            OutputFormat::Flac => preserve_flac_tags(input, bytes),
            _ => bytes,
        };
        return Ok(vec![ConvertedFile {
            name: output_name(input_name, target.extension()),
            bytes,
        }]);
    }

    convert_text(input_name, input, target)
}

/// Renders every page of a PDF and encodes each one, giving one output file per
/// page with a zero-based page suffix, which is what the web app's pdf handler
/// does. Encoding happens inside the render callback so a long document is
/// encoded and released page by page rather than held in memory as a whole.
fn render_pdf_pages(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
    options: ConversionOptions,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    let mut pages = Vec::new();
    pdf::render_pages(input, |index, image| {
        pages.push(ConvertedFile {
            name: page_output_name(input_name, target.extension(), index),
            bytes: encode_image(image, target, options)?,
        });
        Ok(())
    })?;
    Ok(pages)
}

pub fn convert_batch(
    inputs: &[(PathBuf, Vec<u8>)],
    target: OutputFormat,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    convert_batch_with_options(inputs, target, ConversionOptions::default())
}

pub fn convert_batch_with_options(
    inputs: &[(PathBuf, Vec<u8>)],
    target: OutputFormat,
    options: ConversionOptions,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    convert_batch_with_video_config(inputs, target, options, &VideoToolConfig::default())
}

pub fn convert_batch_with_video_config(
    inputs: &[(PathBuf, Vec<u8>)],
    target: OutputFormat,
    options: ConversionOptions,
    video: &VideoToolConfig,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    if inputs.is_empty() {
        return Err(ConvertError::NoInputFiles);
    }

    if matches!(
        target,
        OutputFormat::Zip | OutputFormat::Tar | OutputFormat::TarGz
    ) {
        if inputs.len() > MAX_ARCHIVE_ENTRIES {
            return Err(ConvertError::ArchiveTooManyEntries {
                count: inputs.len(),
                max: MAX_ARCHIVE_ENTRIES,
            });
        }
        let mut seen_names = HashSet::new();
        let mut entries = Vec::with_capacity(inputs.len());

        for (path, bytes) in inputs {
            let name = safe_file_name(path.to_string_lossy().as_ref());
            if !seen_names.insert(name.clone()) {
                return Err(ConvertError::DuplicateArchiveEntryName(name));
            }
            entries.push((PathBuf::from(name), bytes.clone()));
        }

        let (name, bytes) = match target {
            OutputFormat::Zip => (
                "converted.zip",
                create_zip(&entries, options.archive_compression)?,
            ),
            OutputFormat::Tar => (
                "converted.tar",
                create_tar(&entries, false, options.archive_compression)?,
            ),
            OutputFormat::TarGz => (
                "converted.tar.gz",
                create_tar(&entries, true, options.archive_compression)?,
            ),
            _ => unreachable!("only archive targets reach this branch"),
        };
        return Ok(vec![ConvertedFile {
            name: PathBuf::from(name),
            bytes,
        }]);
    }

    if target == OutputFormat::Gzip {
        let mut converted = Vec::with_capacity(inputs.len());
        for (path, bytes) in inputs {
            converted.push(ConvertedFile {
                name: PathBuf::from(format!(
                    "{}.gz",
                    safe_file_name(path.to_string_lossy().as_ref())
                )),
                bytes: gzip_bytes(bytes, options.archive_compression)?,
            });
        }
        return Ok(converted);
    }

    let mut converted = Vec::new();
    for (path, bytes) in inputs {
        converted.extend(convert_bytes_with_video_config(
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .as_ref(),
            bytes,
            target,
            options,
            video,
        )?);
    }
    Ok(converted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ConvertError;
    use crate::format::OutputFormat;
    #[test]
    fn rejects_empty_batches() {
        assert!(matches!(
            convert_batch(&[], OutputFormat::Json),
            Err(ConvertError::NoInputFiles)
        ));
    }
    #[test]
    fn reports_unsupported_format_pairs() {
        let error = convert_bytes("movie.mp4", b"not a movie", OutputFormat::Pdf).unwrap_err();
        assert!(matches!(error, ConvertError::Unsupported { .. }));
    }
}
