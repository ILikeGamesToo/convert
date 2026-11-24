// SPDX-License-Identifier: GPL-2.0-or-later

//! Text encoding conversions.

use crate::data::{json_to_csv, json_to_toml, parse_data_to_json, text_to_csv};
use crate::error::ConvertError;
use crate::format::OutputFormat;
use crate::shared::{file_extension, is_data_extension, output_name, unsupported};
use crate::ConvertedFile;

pub(crate) fn convert_text(
    input_name: &str,
    input: &[u8],
    target: OutputFormat,
) -> Result<Vec<ConvertedFile>, ConvertError> {
    let extension = file_extension(input_name);
    if !is_data_extension(&extension) {
        return Err(unsupported(&extension, target));
    }

    if matches!(
        target,
        OutputFormat::TextUtf8NoBom
            | OutputFormat::TextUtf8Bom
            | OutputFormat::TextUtf16Le
            | OutputFormat::TextUtf16Be
            | OutputFormat::TextUtf32Le
            | OutputFormat::TextUtf32Be
    ) {
        if extension != "txt" {
            return Err(unsupported(&extension, target));
        }
        let text = decode_text(input)?;
        return Ok(vec![ConvertedFile {
            name: output_name(input_name, target.extension()),
            bytes: encode_text(&text, target)?,
        }]);
    }

    let text = if extension == "txt" {
        decode_text(input)?
    } else {
        String::from_utf8(input.to_vec())?
    };
    let output = match target {
        OutputFormat::Text => match extension.as_str() {
            "json" => {
                let value: serde_json::Value = serde_json::from_str(&text)?;
                match value {
                    serde_json::Value::String(value) => value.into_bytes(),
                    value => serde_json::to_vec_pretty(&value)?,
                }
            }
            "yaml" | "yml" | "toml" | "txt" => text.into_bytes(),
            _ => input.to_vec(),
        },
        OutputFormat::Json => {
            let value = parse_data_to_json(&extension, &text)?;
            serde_json::to_vec_pretty(&value)?
        }
        OutputFormat::Csv => match extension.as_str() {
            "csv" => input.to_vec(),
            "txt" => text_to_csv(&text)?,
            _ => {
                let value = parse_data_to_json(&extension, &text)?;
                json_to_csv(&value)?
            }
        },
        OutputFormat::Yaml => {
            let value = parse_data_to_json(&extension, &text)?;
            serde_yaml_ng::to_string(&value)?.into_bytes()
        }
        OutputFormat::Toml => {
            let value = parse_data_to_json(&extension, &text)?;
            json_to_toml(&value)?.into_bytes()
        }
        _ => return Err(unsupported(&extension, target)),
    };

    let output_extension = target.extension();
    Ok(vec![ConvertedFile {
        name: output_name(input_name, output_extension),
        bytes: output,
    }])
}

pub(crate) fn decode_text(input: &[u8]) -> Result<String, ConvertError> {
    if let Some(bytes) = input.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        return Ok(String::from_utf8_lossy(bytes).into_owned());
    }
    if let Some(bytes) = input.strip_prefix(&[0xff, 0xfe, 0x00, 0x00]) {
        return decode_utf32(bytes, true);
    }
    if let Some(bytes) = input.strip_prefix(&[0x00, 0x00, 0xfe, 0xff]) {
        return decode_utf32(bytes, false);
    }
    if let Some(bytes) = input.strip_prefix(&[0xff, 0xfe]) {
        return Ok(decode_utf16(bytes, true));
    }
    if let Some(bytes) = input.strip_prefix(&[0xfe, 0xff]) {
        return Ok(decode_utf16(bytes, false));
    }
    Ok(String::from_utf8_lossy(input).into_owned())
}

pub(crate) fn decode_utf16(input: &[u8], little_endian: bool) -> String {
    let words = input
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes(*pair)
            } else {
                u16::from_be_bytes(*pair)
            }
        })
        .collect::<Vec<_>>();
    String::from_utf16_lossy(&words)
}

pub(crate) fn decode_utf32(input: &[u8], little_endian: bool) -> Result<String, ConvertError> {
    input
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| {
            let bytes = *chunk;
            let code_point = if little_endian {
                u32::from_le_bytes(bytes)
            } else {
                u32::from_be_bytes(bytes)
            };
            char::from_u32(code_point).ok_or(ConvertError::InvalidUnicodeCodePoint(code_point))
        })
        .collect()
}

pub(crate) fn encode_text(text: &str, target: OutputFormat) -> Result<Vec<u8>, ConvertError> {
    match target {
        OutputFormat::TextUtf8Bom => {
            let mut bytes = vec![0xef, 0xbb, 0xbf];
            bytes.extend_from_slice(text.as_bytes());
            Ok(bytes)
        }
        OutputFormat::TextUtf16Le | OutputFormat::TextUtf16Be => {
            let little_endian = target == OutputFormat::TextUtf16Le;
            let mut bytes = if little_endian {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            };
            for word in text.encode_utf16() {
                let encoded = if little_endian {
                    word.to_le_bytes()
                } else {
                    word.to_be_bytes()
                };
                bytes.extend_from_slice(&encoded);
            }
            Ok(bytes)
        }
        OutputFormat::TextUtf32Le | OutputFormat::TextUtf32Be => {
            let little_endian = target == OutputFormat::TextUtf32Le;
            let mut bytes = if little_endian {
                vec![0xff, 0xfe, 0x00, 0x00]
            } else {
                vec![0x00, 0x00, 0xfe, 0xff]
            };
            for code_point in text.chars().map(u32::from) {
                let encoded = if little_endian {
                    code_point.to_le_bytes()
                } else {
                    code_point.to_be_bytes()
                };
                bytes.extend_from_slice(&encoded);
            }
            Ok(bytes)
        }
        OutputFormat::Text | OutputFormat::TextUtf8NoBom => Ok(text.as_bytes().to_vec()),
        _ => Err(unsupported("txt", target)),
    }
}
