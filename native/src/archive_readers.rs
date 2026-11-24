// SPDX-License-Identifier: GPL-2.0-or-later

//! RAR and 7z reading.
//!
//! The native app writes ZIP, TAR, TAR.GZ and GZ and reads RAR and 7z as
//! inputs, so a comic that ships as CBR or CB7 can be re-packed as a CBZ. Both
//! decoders are pure Rust: nothing here shells out and nothing needs a
//! toolchain on the user's machine.
//!
//! Every limit the ZIP and TAR reader enforces is enforced here too, through
//! the same [`push_archive_entry`] helper: absolute paths, `..` segments,
//! drive prefixes, duplicate names, oversized entries and excessive entry
//! counts. 7z compresses solidly, so a small file can decode to a large one;
//! the byte budget and the decode deadline are checked while the payload is
//! read rather than only after it lands.

use std::{
    collections::HashSet,
    io::{Cursor, Read, Write},
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

use crate::archive::{
    push_archive_entry, safe_archive_path, MAX_ARCHIVE_BYTES, MAX_ARCHIVE_ENTRIES,
    MAX_ARCHIVE_ENTRY_BYTES,
};
use crate::convert::ConvertedFile;
use crate::error::ConvertError;

/// The six bytes every RAR archive starts with. RAR 4 follows with an archive
/// type byte and RAR 5 with a different one, so the marker stops before them.
const RAR_MARKER: &[u8] = b"Rar!\x1a\x07";

/// The six bytes every 7z archive starts with, including the format version.
const SEVEN_ZIP_MARKER: &[u8] = b"7z\xbc\xaf\x27\x1c";

/// How long one RAR or 7z decode may run before it is abandoned. A solid
/// archive turns a few kilobytes into a lot of work, so the budget is a wall
/// clock rather than only a size.
const MAX_DECODE_TIME: Duration = Duration::from_secs(120);

/// The memory a 7z decoder may use for its dictionaries. The same order as the
/// extraction byte budget, so a legitimate archive fits and a bomb does not.
const MAX_SEVEN_ZIP_DICTIONARY_KIB: usize = 512 * 1024;

/// Whether the bytes begin with the RAR marker.
pub(crate) fn is_rar(bytes: &[u8]) -> bool {
    bytes.starts_with(RAR_MARKER)
}

/// Whether the bytes begin with the 7z marker.
pub(crate) fn is_seven_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(SEVEN_ZIP_MARKER)
}

/// Whether the bytes are a RAR or 7z archive this module reads.
pub(crate) fn is_archive(bytes: &[u8]) -> bool {
    is_rar(bytes) || is_seven_zip(bytes)
}

/// Collects decoded members with the same safety checks the ZIP and TAR reader
/// applies, so the three formats cannot drift apart.
#[derive(Default)]
struct MemberSink {
    outputs: Vec<ConvertedFile>,
    names: HashSet<PathBuf>,
    total_bytes: u64,
}

impl MemberSink {
    fn push(&mut self, name: &str, bytes: Vec<u8>) -> Result<(), ConvertError> {
        push_archive_entry(
            &mut self.outputs,
            &mut self.names,
            &mut self.total_bytes,
            name,
            bytes,
        )
    }

    fn finish(self) -> Result<Vec<ConvertedFile>, ConvertError> {
        if self.outputs.is_empty() {
            return Err(ConvertError::NoArchiveEntries);
        }
        Ok(self.outputs)
    }
}

/// A deadline for one RAR decode. `rars` polls its cancellation token between
/// decoder steps, so a watchdog cancels it when the budget runs out. `Drop`
/// always stops and joins the watchdog, so a fast archive leaves no thread
/// behind and an early return cannot leak one.
struct RarDeadline {
    cancellation: rars::ReadCancellation,
    stop: mpsc::Sender<()>,
    watchdog: Option<std::thread::JoinHandle<()>>,
}

impl RarDeadline {
    fn start(limit: Duration) -> Self {
        let cancellation = rars::ReadCancellation::new();
        let watched = cancellation.clone();
        let (stop, stopped) = mpsc::channel();
        let watchdog = std::thread::spawn(move || {
            if stopped.recv_timeout(limit).is_err() {
                watched.cancel();
            }
        });
        Self {
            cancellation,
            stop,
            watchdog: Some(watchdog),
        }
    }

    fn cancellation(&self) -> &rars::ReadCancellation {
        &self.cancellation
    }
}

impl Drop for RarDeadline {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }
}

/// One member the RAR walk wrote into a capture buffer, kept until the whole
/// archive has been decoded so a failure part-way through leaves nothing behind.
struct CapturedMember {
    name: String,
    skip: bool,
    buffer: Arc<Mutex<Vec<u8>>>,
}

/// A writer that keeps one member's bytes so the caller can inspect them after
/// `rars` has finished handing the writer around.
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut captured = self
            .0
            .lock()
            .map_err(|_| std::io::Error::other("member capture is poisoned"))?;
        captured.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Extracts a RAR archive into independent files.
pub(crate) fn extract_rar(input: &[u8]) -> Result<Vec<ConvertedFile>, ConvertError> {
    let deadline = RarDeadline::start(MAX_DECODE_TIME);
    let options = rars::ArchiveReadOptions::new()
        .with_cancellation(deadline.cancellation())
        .with_max_header_count(MAX_ARCHIVE_ENTRIES as u64 * 2)
        .with_max_member_output_bytes(MAX_ARCHIVE_ENTRY_BYTES)
        .with_max_total_output_bytes(MAX_ARCHIVE_BYTES);
    let archive = rars::ArchiveReader::read_with_options(input, options).map_err(rar_error)?;
    if archive.is_volume() {
        return Err(ConvertError::ArchiveDecoder {
            format: "RAR",
            message: "multi-volume sets are not supported".to_owned(),
        });
    }
    check_rar_members(&archive)?;

    // Directories and links are captured and dropped afterwards instead of
    // being skipped, because rars refuses to skip a member that shares a solid
    // block with the next one and neither kind carries a payload worth reading.
    let mut captured: Vec<CapturedMember> = Vec::new();
    archive
        .extract_with_control(options, |member| {
            let buffer = Arc::new(Mutex::new(Vec::new()));
            captured.push(CapturedMember {
                name: member.meta.name_lossy(),
                skip: member.meta.is_directory || member.meta.is_redirection,
                buffer: Arc::clone(&buffer),
            });
            Ok(rars::ExtractionDecision::Extract(Box::new(CaptureWriter(
                buffer,
            ))))
        })
        .map_err(rar_error)?;

    let mut sink = MemberSink::default();
    for CapturedMember { name, skip, buffer } in captured {
        if skip {
            continue;
        }
        let bytes = Arc::try_unwrap(buffer)
            .map(|cell| cell.into_inner().unwrap_or_default())
            .unwrap_or_else(|shared| shared.lock().map(|guard| guard.clone()).unwrap_or_default());
        sink.push(&name, bytes)?;
    }
    sink.finish()
}

/// Counts the files a RAR archive holds without decoding them.
pub(crate) fn rar_entry_count(input: &[u8]) -> Result<usize, ConvertError> {
    let archive = rars::ArchiveReader::read(input).map_err(rar_error)?;
    Ok(archive
        .members()
        .filter(|member| !member.meta.is_directory && !member.meta.is_redirection)
        .count())
}

/// Rejects a RAR whose metadata already breaks a limit, so the decoder never
/// runs on an archive that cannot be extracted.
fn check_rar_members(archive: &rars::Archive) -> Result<(), ConvertError> {
    let mut count = 0_usize;
    let mut total = 0_u64;
    for member in archive.members() {
        if member.meta.is_encrypted {
            return Err(ConvertError::ArchiveEncrypted);
        }
        if member.meta.is_directory || member.meta.is_redirection {
            continue;
        }
        count += 1;
        if count > MAX_ARCHIVE_ENTRIES {
            return Err(ConvertError::ArchiveTooManyEntries {
                count,
                max: MAX_ARCHIVE_ENTRIES,
            });
        }
        if member.meta.unpacked_size > MAX_ARCHIVE_ENTRY_BYTES {
            return Err(ConvertError::ArchiveTooLarge {
                max: MAX_ARCHIVE_ENTRY_BYTES,
            });
        }
        total = total.saturating_add(member.meta.unpacked_size);
        if total > MAX_ARCHIVE_BYTES {
            return Err(ConvertError::ArchiveTooLarge {
                max: MAX_ARCHIVE_BYTES,
            });
        }
        safe_archive_path(&member.meta.name_lossy())?;
    }
    Ok(())
}

/// Extracts a 7z archive into independent files.
pub(crate) fn extract_seven_zip(input: &[u8]) -> Result<Vec<ConvertedFile>, ConvertError> {
    let deadline = Instant::now() + MAX_DECODE_TIME;
    let mut reader = sevenz_rust2::ArchiveReader::new_with_memory_limit(
        Cursor::new(input),
        sevenz_rust2::Password::empty(),
        MAX_SEVEN_ZIP_DICTIONARY_KIB,
    )
    .map_err(seven_zip_error)?;
    check_seven_zip_entries(reader.archive())?;

    let mut sink = MemberSink::default();
    let mut failure: Option<ConvertError> = None;
    let result = reader.for_each_entries(|entry, data| {
        if entry.is_directory() {
            return Ok(true);
        }
        match read_seven_zip_entry(entry, data, deadline) {
            Ok(bytes) => match sink.push(&entry.name, bytes) {
                Ok(()) => Ok(true),
                Err(error) => {
                    failure = Some(error);
                    Err(sevenz_rust2::Error::Other("extraction stopped".into()))
                }
            },
            Err(error) => {
                failure = Some(error);
                Err(sevenz_rust2::Error::Other("extraction stopped".into()))
            }
        }
    });
    if let Some(error) = failure {
        return Err(error);
    }
    result.map_err(seven_zip_error)?;
    sink.finish()
}

/// Counts the files a 7z archive holds without decoding them.
pub(crate) fn seven_zip_entry_count(input: &[u8]) -> Result<usize, ConvertError> {
    let mut cursor = Cursor::new(input);
    let archive = sevenz_rust2::Archive::read(&mut cursor, &sevenz_rust2::Password::empty())
        .map_err(seven_zip_error)?;
    Ok(archive
        .files
        .iter()
        .filter(|entry| !entry.is_directory())
        .count())
}

/// Rejects a 7z whose metadata already breaks a limit, so a solid block is
/// never decoded for an archive that cannot be extracted.
fn check_seven_zip_entries(archive: &sevenz_rust2::Archive) -> Result<(), ConvertError> {
    let mut count = 0_usize;
    let mut total = 0_u64;
    for (index, entry) in archive.files.iter().enumerate() {
        if archive.is_file_encrypted(index) {
            return Err(ConvertError::ArchiveEncrypted);
        }
        if entry.is_directory() {
            continue;
        }
        count += 1;
        if count > MAX_ARCHIVE_ENTRIES {
            return Err(ConvertError::ArchiveTooManyEntries {
                count,
                max: MAX_ARCHIVE_ENTRIES,
            });
        }
        if entry.size > MAX_ARCHIVE_ENTRY_BYTES {
            return Err(ConvertError::ArchiveTooLarge {
                max: MAX_ARCHIVE_ENTRY_BYTES,
            });
        }
        total = total.saturating_add(entry.size);
        if total > MAX_ARCHIVE_BYTES {
            return Err(ConvertError::ArchiveTooLarge {
                max: MAX_ARCHIVE_BYTES,
            });
        }
        safe_archive_path(&entry.name)?;
    }
    Ok(())
}

/// Reads one 7z member, checking the byte budget and the deadline as it goes so
/// a solid block cannot run away before either limit is noticed.
fn read_seven_zip_entry(
    entry: &sevenz_rust2::ArchiveEntry,
    data: &mut dyn Read,
    deadline: Instant,
) -> Result<Vec<u8>, ConvertError> {
    if entry.size > MAX_ARCHIVE_ENTRY_BYTES {
        return Err(ConvertError::ArchiveTooLarge {
            max: MAX_ARCHIVE_ENTRY_BYTES,
        });
    }
    let mut bytes = Vec::with_capacity(entry.size.min(64 * 1024) as usize);
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        if Instant::now() > deadline {
            return Err(ConvertError::ArchiveDecodeTimeout {
                seconds: MAX_DECODE_TIME.as_secs(),
            });
        }
        let read = data.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() as u64 > MAX_ARCHIVE_ENTRY_BYTES {
            return Err(ConvertError::ArchiveTooLarge {
                max: MAX_ARCHIVE_ENTRY_BYTES,
            });
        }
    }
    Ok(bytes)
}

/// Turns a `rars` failure into the typed error the rest of the crate reports.
fn rar_error(error: rars::Error) -> ConvertError {
    match error {
        rars::Error::NeedPassword
        | rars::Error::WrongPasswordOrCorruptData
        | rars::Error::UnsupportedEncryption { .. } => ConvertError::ArchiveEncrypted,
        rars::Error::Cancelled => ConvertError::ArchiveDecodeTimeout {
            seconds: MAX_DECODE_TIME.as_secs(),
        },
        rars::Error::MemberOutputLimitExceeded { .. }
        | rars::Error::TotalOutputLimitExceeded { .. } => ConvertError::ArchiveTooLarge {
            max: MAX_ARCHIVE_BYTES,
        },
        rars::Error::HeaderCountLimitExceeded { .. } => ConvertError::ArchiveTooManyEntries {
            count: MAX_ARCHIVE_ENTRIES,
            max: MAX_ARCHIVE_ENTRIES,
        },
        other => ConvertError::ArchiveDecoder {
            format: "RAR",
            message: other.to_string(),
        },
    }
}

/// Turns a `sevenz_rust2` failure into the typed error the rest of the crate
/// reports.
fn seven_zip_error(error: sevenz_rust2::Error) -> ConvertError {
    match error {
        sevenz_rust2::Error::PasswordRequired | sevenz_rust2::Error::MaybeBadPassword(_) => {
            ConvertError::ArchiveEncrypted
        }
        sevenz_rust2::Error::MaxMemLimited { .. } => ConvertError::ArchiveTooLarge {
            max: MAX_ARCHIVE_BYTES,
        },
        other => ConvertError::ArchiveDecoder {
            format: "7z",
            message: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::archive_entry_count;
    use crate::convert::{convert_batch, convert_bytes};
    use crate::format::OutputFormat;
    use std::io::Read;

    const RAR4_COMIC: &[u8] = include_bytes!("../testdata/sample-comic.cbr");
    const RAR5_COMIC: &[u8] = include_bytes!("../testdata/sample-rar5.cbr");
    const ENCRYPTED_RAR: &[u8] = include_bytes!("../testdata/sample-encrypted.rar");
    const SEVEN_ZIP_COMIC: &[u8] = include_bytes!("../testdata/sample-comic.cb7");
    const BZIP2_COMIC: &[u8] = include_bytes!("../testdata/sample-bzip2.7z");
    const ENCRYPTED_HEADER_SEVEN_ZIP: &[u8] =
        include_bytes!("../testdata/sample-encrypted-header.7z");
    const ENCRYPTED_SEVEN_ZIP: &[u8] = include_bytes!("../testdata/sample-encrypted.7z");

    fn member<'a>(files: &'a [ConvertedFile], name: &str) -> &'a [u8] {
        &files
            .iter()
            .find(|file| file.name.to_string_lossy() == name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .bytes
    }

    fn assert_comic(files: &[ConvertedFile]) {
        assert_eq!(files.len(), 3);
        assert!(member(files, "page-1.png").starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(member(files, "page-2.png").starts_with(b"\x89PNG\r\n\x1a\n"));
        let info = String::from_utf8(member(files, "ComicInfo.xml").to_vec()).unwrap();
        assert!(info.contains("<PageCount>2</PageCount>"));
    }

    #[test]
    fn detects_rar_and_seven_zip_by_magic() {
        assert!(is_rar(RAR4_COMIC));
        assert!(is_rar(ENCRYPTED_RAR));
        assert!(!is_rar(SEVEN_ZIP_COMIC));
        assert!(is_seven_zip(SEVEN_ZIP_COMIC));
        assert!(is_seven_zip(BZIP2_COMIC));
        assert!(!is_seven_zip(RAR5_COMIC));
        assert!(is_archive(RAR5_COMIC));
        assert!(!is_archive(b"PK\x03\x04"));
    }
    #[test]
    fn extracts_rar4_and_rar5_comics() {
        assert_comic(&extract_rar(RAR4_COMIC).unwrap());
        assert_comic(&extract_rar(RAR5_COMIC).unwrap());
    }
    #[test]
    fn extracts_seven_zip_comics_with_lzma2_and_bzip2() {
        assert_comic(&extract_seven_zip(SEVEN_ZIP_COMIC).unwrap());
        assert_comic(&extract_seven_zip(BZIP2_COMIC).unwrap());
    }
    #[test]
    fn counts_members_without_decoding_them() {
        for (name, bytes) in [
            ("sample-comic.cbr", RAR4_COMIC),
            ("sample-rar5.cbr", RAR5_COMIC),
            ("sample-comic.cb7", SEVEN_ZIP_COMIC),
            ("sample-bzip2.7z", BZIP2_COMIC),
        ] {
            assert_eq!(archive_entry_count(name, bytes).unwrap(), 3, "{name}");
        }
    }
    #[test]
    fn refuses_encrypted_archives() {
        for (name, bytes) in [
            ("sample-encrypted.rar", ENCRYPTED_RAR),
            ("sample-encrypted.7z", ENCRYPTED_SEVEN_ZIP),
            ("sample-encrypted-header.7z", ENCRYPTED_HEADER_SEVEN_ZIP),
        ] {
            let error = convert_bytes(name, bytes, OutputFormat::Files).unwrap_err();
            assert!(
                matches!(error, ConvertError::ArchiveEncrypted),
                "{name} produced {error}"
            );
        }
    }
    #[test]
    fn reports_malformed_archives_as_decoder_errors() {
        for (name, bytes) in [
            ("broken.rar", &b"Rar!\x1a\x07\x00not a header"[..]),
            ("broken.7z", &b"7z\xbc\xaf\x27\x1c\x00\x04not a header"[..]),
        ] {
            let error = convert_bytes(name, bytes, OutputFormat::Files).unwrap_err();
            assert!(
                matches!(error, ConvertError::ArchiveDecoder { .. }),
                "{name} produced {error}"
            );
        }
    }
    #[test]
    fn maps_decoder_failures_to_typed_errors() {
        assert!(matches!(
            rar_error(rars::Error::NeedPassword),
            ConvertError::ArchiveEncrypted
        ));
        assert!(matches!(
            rar_error(rars::Error::Cancelled),
            ConvertError::ArchiveDecodeTimeout { .. }
        ));
        assert!(matches!(
            seven_zip_error(sevenz_rust2::Error::PasswordRequired),
            ConvertError::ArchiveEncrypted
        ));
    }
    #[test]
    fn the_rar_deadline_cancels_a_decode_that_overruns_it() {
        let deadline = RarDeadline::start(Duration::from_millis(20));
        assert!(!deadline.cancellation().is_cancelled());
        std::thread::sleep(Duration::from_millis(120));
        assert!(deadline.cancellation().is_cancelled());
    }
    #[test]
    fn dropping_the_rar_deadline_stops_its_watchdog_early() {
        let started = Instant::now();
        let deadline = RarDeadline::start(Duration::from_secs(30));
        assert!(!deadline.cancellation().is_cancelled());
        drop(deadline);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "dropping the deadline waited for the whole budget"
        );
    }
    #[test]
    fn a_seven_zip_member_past_its_deadline_is_abandoned() {
        let entry = sevenz_rust2::ArchiveEntry::default();
        let error = read_seven_zip_entry(
            &entry,
            &mut &b"data"[..],
            Instant::now() - Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ConvertError::ArchiveDecodeTimeout { seconds } if seconds == MAX_DECODE_TIME.as_secs()
        ));
    }
    #[test]
    fn repacks_a_cbr_into_a_cbz() {
        let converted = convert_bytes("sample-comic.cbr", RAR4_COMIC, OutputFormat::Cbz).unwrap();
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].name, PathBuf::from("sample-comic.cbz"));

        let mut archive = zip::ZipArchive::new(Cursor::new(&converted[0].bytes)).unwrap();
        assert_eq!(archive.len(), 3);
        let mut page = Vec::new();
        archive
            .by_name("page-1.png")
            .unwrap()
            .read_to_end(&mut page)
            .unwrap();
        assert_eq!(
            page,
            member(&extract_rar(RAR4_COMIC).unwrap(), "page-1.png")
        );
        assert!(crate::images::load_image(&page, Default::default()).is_ok());
    }
    #[test]
    fn repacks_an_archive_input_instead_of_nesting_it() {
        let inputs = vec![(PathBuf::from("sample-comic.cb7"), SEVEN_ZIP_COMIC.to_vec())];
        let packed = convert_batch(&inputs, OutputFormat::Zip).unwrap();
        assert_eq!(packed[0].name, PathBuf::from("converted.zip"));

        let mut archive = zip::ZipArchive::new(Cursor::new(&packed[0].bytes)).unwrap();
        assert_eq!(archive.len(), 3);
        assert!(archive.by_name("page-1.png").is_ok());
        assert!(archive.by_name("sample-comic.cb7").is_err());
    }
}
