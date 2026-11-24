// SPDX-License-Identifier: GPL-2.0-or-later

//! ZIP, TAR, TAR.GZ and GZIP creation and extraction.

use crate::convert::ConvertedFile;
use crate::error::ConvertError;
use crate::format::OutputFormat;
use crate::options::ConversionOptions;
use crate::shared::{file_extension, unsupported};
use std::{
    collections::HashSet,
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
};

pub(crate) const MAX_ARCHIVE_ENTRIES: usize = 10_000;

pub(crate) const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

pub(crate) const MAX_ARCHIVE_ENTRY_BYTES: u64 = 256 * 1024 * 1024;

pub(crate) fn create_zip(
    entries: &[(PathBuf, Vec<u8>)],
    compression: Option<u8>,
) -> Result<Vec<u8>, ConvertError> {
    let cursor = Cursor::new(Vec::new());
    let mut archive = zip::ZipWriter::new(cursor);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(compression.map(|level| i64::from(level.clamp(1, 9))));

    for (name, bytes) in entries {
        let name = name.to_string_lossy().replace('\\', "/");
        archive.start_file(name, options)?;
        archive.write_all(bytes)?;
    }

    Ok(archive.finish()?.into_inner())
}

pub(crate) fn create_tar(
    entries: &[(PathBuf, Vec<u8>)],
    gzip: bool,
    compression: Option<u8>,
) -> Result<Vec<u8>, ConvertError> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, bytes) in entries {
        let name = name.to_string_lossy().replace('\\', "/");
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder.append_data(&mut header, name, bytes.as_slice())?;
    }
    let bytes = builder.into_inner()?;
    if gzip {
        gzip_bytes(&bytes, compression)
    } else {
        Ok(bytes)
    }
}

pub(crate) fn gzip_bytes(input: &[u8], compression: Option<u8>) -> Result<Vec<u8>, ConvertError> {
    let level = compression
        .unwrap_or(ConversionOptions::DEFAULT_COMPRESSION_LEVEL)
        .clamp(1, 9);
    let mut encoder =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level.into()));
    encoder.write_all(input)?;
    Ok(encoder.finish()?)
}

/// Extracts a ZIP, TAR, TAR.GZ or GZIP archive into independent files.
/// Entry names are normalized to safe relative paths; absolute paths and
/// parent-directory components are rejected instead of being written outside
/// the output directory.
pub fn extract_archive(input_name: &str, input: &[u8]) -> Result<Vec<ConvertedFile>, ConvertError> {
    let lower_name = input_name.to_ascii_lowercase();
    let mut outputs = Vec::new();
    let mut names = HashSet::new();
    let mut total_bytes = 0_u64;

    if lower_name.ends_with(".tar.gz") || lower_name.ends_with(".tgz") {
        let decoder = flate2::read::GzDecoder::new(Cursor::new(input));
        extract_tar_entries(decoder, &mut outputs, &mut names, &mut total_bytes)?;
    } else if lower_name.ends_with(".tar") {
        extract_tar_entries(
            Cursor::new(input),
            &mut outputs,
            &mut names,
            &mut total_bytes,
        )?;
    } else if lower_name.ends_with(".zip") {
        extract_zip_entries(input, &mut outputs, &mut names, &mut total_bytes)?;
    } else if lower_name.ends_with(".gz") {
        let name = Path::new(input_name)
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".gz"))
            .filter(|name| !name.is_empty())
            .unwrap_or("extracted");
        let mut decoder = flate2::read::GzDecoder::new(Cursor::new(input));
        let data = read_archive_entry(&mut decoder, MAX_ARCHIVE_ENTRY_BYTES)?;
        push_archive_entry(&mut outputs, &mut names, &mut total_bytes, name, data)?;
    } else {
        return Err(unsupported(
            &file_extension(input_name),
            OutputFormat::Files,
        ));
    }

    if outputs.is_empty() {
        return Err(ConvertError::NoArchiveEntries);
    }
    Ok(outputs)
}

/// How many files an archive holds, without extracting them. Directory entries
/// are skipped, matching what `extract_archive` returns. Counting a compressed
/// tarball decompresses it, so this costs about as much as listing it.
pub fn archive_entry_count(input_name: &str, input: &[u8]) -> Result<usize, ConvertError> {
    let lower_name = input_name.to_ascii_lowercase();
    if lower_name.ends_with(".tar.gz") || lower_name.ends_with(".tgz") {
        return count_tar_entries(flate2::read::GzDecoder::new(Cursor::new(input)));
    }
    if lower_name.ends_with(".tar") {
        return count_tar_entries(Cursor::new(input));
    }
    if lower_name.ends_with(".zip") {
        let mut archive = zip::ZipArchive::new(Cursor::new(input))?;
        let mut count = 0;
        for index in 0..archive.len() {
            if !archive.by_index(index)?.is_dir() {
                count += 1;
            }
        }
        return Ok(count);
    }
    if lower_name.ends_with(".gz") {
        return Ok(1);
    }
    Err(unsupported(
        &file_extension(input_name),
        OutputFormat::Files,
    ))
}

fn count_tar_entries<R: Read>(reader: R) -> Result<usize, ConvertError> {
    let mut archive = tar::Archive::new(reader);
    let mut count = 0;
    for entry in archive.entries()? {
        if entry?.header().entry_type().is_file() {
            count += 1;
        }
    }
    Ok(count)
}

pub(crate) fn extract_zip_entries(
    input: &[u8],
    outputs: &mut Vec<ConvertedFile>,
    names: &mut HashSet<PathBuf>,
    total_bytes: &mut u64,
) -> Result<(), ConvertError> {
    let mut archive = zip::ZipArchive::new(Cursor::new(input))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        return Err(ConvertError::ArchiveTooManyEntries {
            count: archive.len(),
            max: MAX_ARCHIVE_ENTRIES,
        });
    }
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        let data = read_archive_entry(&mut entry, MAX_ARCHIVE_ENTRY_BYTES)?;
        push_archive_entry(outputs, names, total_bytes, &name, data)?;
    }
    Ok(())
}

pub(crate) fn extract_tar_entries<R: Read>(
    reader: R,
    outputs: &mut Vec<ConvertedFile>,
    names: &mut HashSet<PathBuf>,
    total_bytes: &mut u64,
) -> Result<(), ConvertError> {
    let mut archive = tar::Archive::new(reader);
    let mut count = 0_usize;
    for entry in archive.entries()? {
        let mut entry = entry?;
        count += 1;
        if count > MAX_ARCHIVE_ENTRIES {
            return Err(ConvertError::ArchiveTooManyEntries {
                count,
                max: MAX_ARCHIVE_ENTRIES,
            });
        }
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.to_string_lossy().into_owned();
        let data = read_archive_entry(&mut entry, MAX_ARCHIVE_ENTRY_BYTES)?;
        push_archive_entry(outputs, names, total_bytes, &path, data)?;
    }
    Ok(())
}

pub(crate) fn read_archive_entry<R: Read>(
    reader: &mut R,
    limit: u64,
) -> Result<Vec<u8>, ConvertError> {
    let mut data = Vec::new();
    reader.take(limit + 1).read_to_end(&mut data)?;
    if data.len() as u64 > limit {
        return Err(ConvertError::ArchiveTooLarge {
            max: MAX_ARCHIVE_BYTES,
        });
    }
    Ok(data)
}

pub(crate) fn push_archive_entry(
    outputs: &mut Vec<ConvertedFile>,
    names: &mut HashSet<PathBuf>,
    total_bytes: &mut u64,
    name: &str,
    data: Vec<u8>,
) -> Result<(), ConvertError> {
    let path = safe_archive_path(name)?;
    if !names.insert(path.clone()) {
        return Err(ConvertError::DuplicateArchiveEntryName(
            path.display().to_string(),
        ));
    }
    *total_bytes = total_bytes.saturating_add(data.len() as u64);
    if *total_bytes > MAX_ARCHIVE_BYTES {
        return Err(ConvertError::ArchiveTooLarge {
            max: MAX_ARCHIVE_BYTES,
        });
    }
    outputs.push(ConvertedFile {
        name: path,
        bytes: data,
    });
    Ok(())
}

pub(crate) fn safe_archive_path(name: &str) -> Result<PathBuf, ConvertError> {
    let normalized = name.replace('\\', "/");
    if normalized.is_empty() || normalized.contains('\0') || normalized.starts_with('/') {
        return Err(ConvertError::UnsafeArchivePath(name.to_owned()));
    }
    let mut safe = PathBuf::new();
    for part in normalized.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains(':') {
            return Err(ConvertError::UnsafeArchivePath(name.to_owned()));
        }
        safe.push(part);
    }
    if safe.as_os_str().is_empty() {
        return Err(ConvertError::UnsafeArchivePath(name.to_owned()));
    }
    Ok(safe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::{convert_batch, convert_batch_with_options, convert_bytes};
    use crate::error::ConvertError;
    use crate::format::OutputFormat;
    use crate::options::ConversionOptions;
    use std::{
        io::{Cursor, Read, Write},
        path::PathBuf,
    };
    #[test]
    fn applies_archive_compression_options() {
        let inputs = vec![(
            PathBuf::from("note.txt"),
            b"the same line repeated many times\n".repeat(2_000),
        )];

        let fast_zip = convert_batch_with_options(
            &inputs,
            OutputFormat::Zip,
            ConversionOptions {
                archive_compression: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let best_zip = convert_batch_with_options(
            &inputs,
            OutputFormat::Zip,
            ConversionOptions {
                archive_compression: Some(9),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            best_zip[0].bytes.len() <= fast_zip[0].bytes.len(),
            "higher ZIP compression should not produce a larger archive"
        );
        assert!(zip::ZipArchive::new(Cursor::new(&best_zip[0].bytes)).is_ok());

        let fast_gzip = convert_batch_with_options(
            &inputs,
            OutputFormat::Gzip,
            ConversionOptions {
                archive_compression: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let best_gzip = convert_batch_with_options(
            &inputs,
            OutputFormat::Gzip,
            ConversionOptions {
                archive_compression: Some(9),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            best_gzip[0].bytes.len() <= fast_gzip[0].bytes.len(),
            "higher GZIP compression should not produce a larger file"
        );
        let mut decoder = flate2::read::GzDecoder::new(Cursor::new(&best_gzip[0].bytes));
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, inputs[0].1);
    }
    #[test]
    fn archives_multiple_files() {
        let inputs = vec![
            (PathBuf::from("one.txt"), b"one".to_vec()),
            (PathBuf::from("two.txt"), b"two".to_vec()),
        ];
        let archive = convert_batch(&inputs, OutputFormat::Zip).unwrap();
        assert_eq!(archive[0].name, PathBuf::from("converted.zip"));

        let mut zip = zip::ZipArchive::new(Cursor::new(&archive[0].bytes)).unwrap();
        assert_eq!(zip.len(), 2);
        let mut one = String::new();
        zip.by_name("one.txt")
            .unwrap()
            .read_to_string(&mut one)
            .unwrap();
        assert_eq!(one, "one");
        let mut two = String::new();
        zip.by_name("two.txt")
            .unwrap()
            .read_to_string(&mut two)
            .unwrap();
        assert_eq!(two, "two");
    }
    #[test]
    fn extracts_zip_tar_and_gzip_archives() {
        let zip_bytes = {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            writer.start_file("nested/one.txt", options).unwrap();
            writer.write_all(b"one").unwrap();
            writer.start_file("two.txt", options).unwrap();
            writer.write_all(b"two").unwrap();
            writer.finish().unwrap().into_inner()
        };
        let files = convert_bytes("bundle.zip", &zip_bytes, OutputFormat::Files).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, PathBuf::from("nested/one.txt"));
        assert_eq!(files[0].bytes, b"one");
        assert_eq!(files[1].name, PathBuf::from("two.txt"));
        assert_eq!(files[1].bytes, b"two");

        let tar_bytes = {
            let mut builder = tar::Builder::new(Vec::new());
            for (name, content) in [("nested/one.txt", &b"one"[..]), ("two.txt", &b"two"[..])] {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, name, content).unwrap();
            }
            builder.into_inner().unwrap()
        };
        let files = convert_bytes("bundle.tar", &tar_bytes, OutputFormat::Files).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, PathBuf::from("nested/one.txt"));
        assert_eq!(files[0].bytes, b"one");

        let gzip = convert_bytes("note.txt", b"hello", OutputFormat::Gzip).unwrap();
        let files = convert_bytes("note.txt.gz", &gzip[0].bytes, OutputFormat::Files).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, PathBuf::from("note.txt"));
        assert_eq!(files[0].bytes, b"hello");
    }
    #[test]
    fn rejects_archive_paths_that_escape_the_output_directory() {
        let tar_bytes = {
            let mut builder = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_path("safe.txt").unwrap();
            header.as_mut_bytes()[..11].copy_from_slice(b"../evil.txt");
            header.set_size(3);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, &b"bad"[..]).unwrap();
            builder.into_inner().unwrap()
        };
        assert!(matches!(
            convert_bytes("bundle.tar", &tar_bytes, OutputFormat::Files),
            Err(ConvertError::UnsafeArchivePath(_))
        ));

        for name in [
            "../evil.txt",
            "nested/../../evil.txt",
            "/absolute.txt",
            "C:\\evil.txt",
        ] {
            let zip_bytes = {
                let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
                let options = zip::write::SimpleFileOptions::default();
                writer.start_file(name, options).unwrap();
                writer.write_all(b"bad").unwrap();
                writer.finish().unwrap().into_inner()
            };
            let error = convert_bytes("bundle.zip", &zip_bytes, OutputFormat::Files).unwrap_err();
            assert!(
                matches!(error, ConvertError::UnsafeArchivePath(_)),
                "{name} produced {error}"
            );
        }
    }
    #[test]
    fn rejects_duplicate_archive_entry_names() {
        let inputs = vec![
            (PathBuf::from("first/one.txt"), b"one".to_vec()),
            (PathBuf::from("second/one.txt"), b"two".to_vec()),
        ];
        assert!(matches!(
            convert_batch(&inputs, OutputFormat::Zip),
            Err(ConvertError::DuplicateArchiveEntryName(name)) if name == "one.txt"
        ));
    }
    #[test]
    fn rejects_archives_with_too_many_entries() {
        let inputs: Vec<(PathBuf, Vec<u8>)> = (0..=MAX_ARCHIVE_ENTRIES)
            .map(|index| (PathBuf::from(format!("{index}.txt")), Vec::new()))
            .collect();
        assert!(matches!(
            convert_batch(&inputs, OutputFormat::Zip),
            Err(ConvertError::ArchiveTooManyEntries { max, .. }) if max == MAX_ARCHIVE_ENTRIES
        ));
    }
    #[test]
    fn packs_tar_archives() {
        let inputs = vec![
            (PathBuf::from("one.txt"), b"one".to_vec()),
            (PathBuf::from("two.txt"), b"two".to_vec()),
        ];

        for (target, name) in [
            (OutputFormat::Tar, "converted.tar"),
            (OutputFormat::TarGz, "converted.tar.gz"),
        ] {
            let archive = convert_batch(&inputs, target).unwrap();
            assert_eq!(archive[0].name, PathBuf::from(name));

            let reader: Box<dyn Read> = if target == OutputFormat::TarGz {
                Box::new(flate2::read::GzDecoder::new(Cursor::new(&archive[0].bytes)))
            } else {
                Box::new(Cursor::new(&archive[0].bytes))
            };
            let mut tar = tar::Archive::new(reader);
            let mut entries = Vec::new();
            for entry in tar.entries().unwrap() {
                let mut entry = entry.unwrap();
                let path = entry.path().unwrap().to_string_lossy().into_owned();
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                entries.push((path, bytes));
            }
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].0, "one.txt");
            assert_eq!(entries[0].1, b"one");
            assert_eq!(entries[1].0, "two.txt");
            assert_eq!(entries[1].1, b"two");
        }
    }
    #[test]
    fn compresses_gzip_files() {
        let gzip = convert_bytes("note.txt", b"hello", OutputFormat::Gzip).unwrap();
        assert_eq!(gzip[0].name, PathBuf::from("note.txt.gz"));
        assert_eq!(&gzip[0].bytes[0..2], &[0x1f, 0x8b]);

        let mut decoder = flate2::read::GzDecoder::new(Cursor::new(&gzip[0].bytes));
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, b"hello");
    }
}
