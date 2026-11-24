// SPDX-License-Identifier: GPL-2.0-or-later

//! Locating and running the external ffmpeg binary.

use crate::error::ConvertError;
use crate::VideoToolConfig;
use std::{
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
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

pub(crate) fn run_ffmpeg(command: &mut Command, ffmpeg: &Path) -> Result<Output, ConvertError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command.output().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ConvertError::FfmpegNotFound {
                path: ffmpeg.display().to_string(),
            }
        } else {
            ConvertError::FfmpegSpawn(error.to_string())
        }
    })?;
    if !output.status.success() {
        let status = output
            .status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "terminated by signal".to_owned());
        return Err(ConvertError::FfmpegFailed {
            status,
            stderr: ffmpeg_stderr(&output.stderr),
        });
    }
    Ok(output)
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
