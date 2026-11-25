// SPDX-License-Identifier: GPL-2.0-or-later

//! Locating and running the external ffmpeg binary.

use crate::error::ConvertError;
use crate::video::{VideoProgress, VideoToolConfig};
use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
};

pub(crate) const FFMPEG_STDERR_LIMIT: usize = 8_192;

pub(crate) static VIDEO_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        #[cfg(windows)]
        for extension in ["exe", "cmd", "bat", "com"] {
            let candidate = directory.join(format!("{name}.{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        #[cfg(not(windows))]
        {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

pub(crate) fn ffmpeg_command(path: &Path) -> Command {
    #[cfg(windows)]
    if matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase())
            .as_deref(),
        Some("cmd" | "bat")
    ) {
        let mut command = Command::new("cmd");
        command.arg("/C").arg(path);
        return command;
    }
    Command::new(path)
}

pub(crate) fn ffmpeg_stderr(bytes: &[u8]) -> String {
    let text: String = String::from_utf8_lossy(bytes)
        .trim()
        .chars()
        .take(FFMPEG_STDERR_LIMIT)
        .collect();
    if text.is_empty() {
        "<no stderr>".to_owned()
    } else {
        text
    }
}

fn spawn_error(error: std::io::Error, ffmpeg: &Path) -> ConvertError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ConvertError::FfmpegNotFound {
            path: ffmpeg.display().to_string(),
        }
    } else {
        ConvertError::FfmpegSpawn(error.to_string())
    }
}

fn status_text(status: ExitStatus) -> String {
    status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "terminated by signal".to_owned())
}

pub(crate) fn run_ffmpeg(command: &mut Command, ffmpeg: &Path) -> Result<Output, ConvertError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command
        .output()
        .map_err(|error| spawn_error(error, ffmpeg))?;
    if !output.status.success() {
        return Err(ConvertError::FfmpegFailed {
            status: status_text(output.status),
            stderr: ffmpeg_stderr(&output.stderr),
        });
    }
    Ok(output)
}

/// How often a tracked encode looks at the cancellation flag while ffmpeg is
/// quiet. ffmpeg writes a progress block about twice a second, so this only
/// bounds the latency of a cancel between two blocks.
const CANCEL_POLL: Duration = Duration::from_millis(200);

/// Runs an encoding ffmpeg while forwarding the `out_time_us` values it writes
/// for `-progress pipe:1` and stopping it when `cancel` is set.
///
/// The caller must have added `-progress pipe:1 -nostats` to the command and
/// must have given it a file output: stdout is the progress stream, so nothing
/// this function reads is conversion output. Cancellation kills the child and
/// returns [`ConvertError::Cancelled`]; ffmpeg's exit status and stderr become
/// [`ConvertError::FfmpegFailed`] exactly as in [`run_ffmpeg`].
pub(crate) fn run_ffmpeg_tracked(
    command: &mut Command,
    ffmpeg: &Path,
    cancel: &AtomicBool,
    mut on_progress: Option<&mut dyn FnMut(VideoProgress)>,
) -> Result<(), ConvertError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| spawn_error(error, ffmpeg))?;
    let stdout = child.stdout.take().expect("ffmpeg stdout is piped");
    let stderr = child.stderr.take().expect("ffmpeg stderr is piped");

    // Each pipe is drained on its own thread so a full stderr buffer can never
    // block the child while this thread is waiting for the next progress line.
    let (sender, receiver) = mpsc::channel();
    let progress_reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let update = line
                .strip_prefix("out_time_us=")
                .and_then(|value| value.parse::<u64>().ok())
                .map(|microseconds| VideoProgress {
                    out_time: Duration::from_micros(microseconds),
                    total: None,
                });
            if sender.send(update).is_err() {
                break;
            }
        }
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = BufReader::new(stderr).read_to_end(&mut bytes);
        bytes
    });

    let mut cancelled = false;
    loop {
        match receiver.recv_timeout(CANCEL_POLL) {
            Ok(Some(progress)) => {
                if let Some(callback) = on_progress.as_mut() {
                    callback(progress);
                }
            }
            Ok(None) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if cancel.load(Ordering::Relaxed) {
                    cancelled = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    if cancelled {
        let _ = child.kill();
        let _ = child.wait();
        let _ = progress_reader.join();
        let _ = stderr_reader.join();
        return Err(ConvertError::Cancelled);
    }
    let status = child.wait().map_err(|error| spawn_error(error, ffmpeg))?;
    let _ = progress_reader.join();
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        return Err(ConvertError::FfmpegFailed {
            status: status_text(status),
            stderr: ffmpeg_stderr(&stderr),
        });
    }
    Ok(())
}

pub(crate) struct TempVideoPath {
    pub(crate) path: PathBuf,
}

impl TempVideoPath {
    pub(crate) fn new(extension: &str) -> Self {
        let sequence = VIDEO_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "convert2it-{}-{sequence}.{extension}",
            std::process::id()
        ));
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempVideoPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(crate) fn resolve_ffmpeg(video: &VideoToolConfig) -> Result<PathBuf, ConvertError> {
    video.resolve().ok_or_else(|| ConvertError::FfmpegNotFound {
        path: "ffmpeg".to_owned(),
    })
}
