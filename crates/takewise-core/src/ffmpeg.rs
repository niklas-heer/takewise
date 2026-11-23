//! Running `ffmpeg` for the `clean` export and format conversion: found as
//! the checks find it, run at lower priority with a timeout, and fed and
//! read through 32-bit float WAV files.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::whisper::{ToolError, find_tool, run_tool_captured, run_tool_stderr, tool_command};

/// How long one export step may run before `ffmpeg` is stopped.
pub const EXPORT_TIMEOUT: Duration = Duration::from_secs(300);

/// How long `ffprobe` may take to read the film's duration; export probes
/// the film again under the project lock, which a stalled probe must not
/// hold.
pub const FILM_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// `ffprobe` ran longer than its timeout and was stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeTimedOut;

/// The film's duration in seconds as `ffprobe` (run at lower priority)
/// reads it; `None` when it cannot (it fails, or reports no duration);
/// [`ProbeTimedOut`] when it runs longer than `timeout`.
pub fn probe_duration(
    ffprobe: &Path,
    film: &Path,
    timeout: Duration,
) -> Result<Option<f64>, ProbeTimedOut> {
    let mut command = tool_command(ffprobe);
    command
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "json",
        ])
        .arg(film);
    let stdout = match run_tool_captured(&mut command, ffprobe, timeout, true) {
        Ok((stdout, _)) => stdout,
        Err(ToolError::TimedOut(_)) => return Err(ProbeTimedOut),
        Err(ToolError::Failed(_)) => return Ok(None),
    };
    Ok(serde_json::from_slice::<serde_json::Value>(&stdout)
        .ok()
        .and_then(|value| value["format"]["duration"].as_str()?.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0))
}

/// The `ffmpeg` executable and how long a run may take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ffmpeg {
    pub path: PathBuf,
    pub timeout: Duration,
}

impl Ffmpeg {
    /// `ffmpeg` on `$PATH`, else in `/opt/homebrew/bin` or `/usr/local/bin`.
    pub fn find() -> Option<Ffmpeg> {
        find_tool("ffmpeg").map(|path| Ffmpeg {
            path,
            timeout: EXPORT_TIMEOUT,
        })
    }

    /// Runs `ffmpeg -nostdin -y -hide_banner <args>` and returns what it
    /// wrote to stderr; a failure names the tool and quotes its error.
    pub fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<String, String> {
        let mut command = tool_command(&self.path);
        command.args(["-nostdin", "-y", "-hide_banner"]).args(args);
        run_tool_stderr(&mut command, &self.path, self.timeout)
            .map(|stderr| String::from_utf8_lossy(&stderr).into_owned())
    }

    /// Converts `src` with the filter chain `filters` (none when empty) to
    /// 48 kHz mono 32-bit float at `out`.
    pub fn filter_to_float(&self, src: &Path, filters: &str, out: &Path) -> Result<(), String> {
        let mut args: Vec<&OsStr> = vec![
            OsStr::new("-loglevel"),
            OsStr::new("error"),
            OsStr::new("-i"),
            src.as_os_str(),
        ];
        if !filters.is_empty() {
            args.extend([OsStr::new("-af"), OsStr::new(filters)]);
        }
        args.extend(
            ["-ar", "48000", "-ac", "1", "-c:a", "pcm_f32le"]
                .iter()
                .map(OsStr::new),
        );
        args.push(out.as_os_str());
        self.run(&args).map(drop)
    }
}

/// Reads a mono WAV of any sample format hound reads as samples in
/// −1..1, with its rate.
pub fn read_float_wav(path: &Path) -> Result<(Vec<f64>, u32), String> {
    let error = |err: hound::Error| format!("could not read {}: {err}", path.display());
    let reader = hound::WavReader::open(path).map_err(error)?;
    let spec = reader.spec();
    if spec.channels != 1 {
        return Err(format!(
            "{} has {} channels, not one",
            path.display(),
            spec.channels
        ));
    }
    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .into_samples::<f32>()
            .map(|s| s.map(f64::from))
            .collect::<Result<Vec<_>, _>>(),
        hound::SampleFormat::Int => {
            let scale = f64::from(1_u32 << (spec.bits_per_sample.clamp(1, 32) - 1));
            reader
                .into_samples::<i32>()
                .map(|s| s.map(|v| f64::from(v) / scale))
                .collect::<Result<Vec<_>, _>>()
        }
    }
    .map_err(error)?;
    Ok((samples, spec.sample_rate))
}

/// Writes mono 32-bit float samples at `rate`.
pub fn write_float_wav(path: &Path, samples: &[f64], rate: u32) -> Result<(), String> {
    let error = |err: hound::Error| format!("could not write {}: {err}", path.display());
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec).map_err(error)?;
    for &v in samples {
        #[allow(clippy::cast_possible_truncation)]
        writer.write_sample(v as f32).map_err(error)?;
    }
    writer.finalize().map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_wav_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.wav");
        write_float_wav(&path, &[0.0, 0.5, -0.25], 44_100).unwrap();
        assert_eq!(
            read_float_wav(&path).unwrap(),
            (vec![0.0, 0.5, -0.25], 44_100)
        );
        // 24-bit PCM reads scaled to −1..1.
        crate::takes::write_take_wav(&path, &[1 << 22], 48_000).unwrap();
        assert_eq!(read_float_wav(&path).unwrap(), (vec![0.5], 48_000));
    }

    /// A script at `dir/name` running `body`.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn a_film_probe_reads_the_duration_and_stops_when_it_stalls() {
        let tmp = tempfile::tempdir().unwrap();
        let film = tmp.path().join("film.mp4");
        std::fs::write(&film, b"").unwrap();
        let reads = script(
            tmp.path(),
            "ffprobe",
            r#"echo '{"format": {"duration": "52.250000"}}'"#,
        );
        assert_eq!(
            probe_duration(&reads, &film, Duration::from_secs(10)),
            Ok(Some(52.25))
        );
        let fails = script(tmp.path(), "broken", "exit 1");
        assert_eq!(
            probe_duration(&fails, &film, Duration::from_secs(10)),
            Ok(None)
        );
        let stalls = script(tmp.path(), "stalls", "exec sleep 30");
        let started = std::time::Instant::now();
        assert_eq!(
            probe_duration(&stalls, &film, Duration::from_millis(300)),
            Err(ProbeTimedOut)
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "stopped in time"
        );
    }
}
