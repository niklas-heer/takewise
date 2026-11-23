//! Reading the film's duration, size and modification time.

use std::path::Path;
use std::process::Command;

use takewise_core::FilmInfo;

use crate::cli::CliError;

/// Measures the film with `ffprobe` and its file metadata.
pub fn probe(path: &Path) -> Result<FilmInfo, CliError> {
    let (size, modified) = stamp(path)?;
    let ffprobe = takewise_core::whisper::find_tool("ffprobe").unwrap_or_else(|| "ffprobe".into());
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .map_err(|err| CliError::Setup(format!("cannot run ffprobe (install ffmpeg): {err}")))?;
    if !output.status.success() {
        return Err(CliError::Setup(format!(
            "ffprobe cannot read {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|err| CliError::Setup(format!("unexpected ffprobe output: {err}")))?;
    let duration_s = value["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
        .ok_or_else(|| {
            CliError::Setup(format!(
                "ffprobe reports no duration for {}",
                path.display()
            ))
        })?;
    Ok(FilmInfo {
        duration_s,
        size,
        modified,
    })
}

/// The file's size and modification time (RFC 3339, whole seconds).
pub fn stamp(path: &Path) -> Result<(u64, String), CliError> {
    let meta = std::fs::metadata(path).map_err(|err| {
        CliError::Setup(format!("cannot read the film {}: {err}", path.display()))
    })?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| jiff::Timestamp::try_from(time).ok())
        .and_then(|ts| jiff::Timestamp::from_second(ts.as_second()).ok())
        .map(|ts| ts.to_string())
        .unwrap_or_default();
    Ok((meta.len(), modified))
}

/// Whether the film on disk still matches what the project measured.
pub fn matches(path: &Path, info: &FilmInfo) -> Result<bool, CliError> {
    let (size, modified) = stamp(path)?;
    Ok(size == info.size && modified == info.modified)
}
