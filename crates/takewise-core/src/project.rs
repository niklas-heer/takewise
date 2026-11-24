//! A project directory: `project.toml`, `script.toml` and the takes.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::edit::ScriptVersion;
use crate::error::{Error, Result, io};
use crate::script::Script;

pub const PROJECT_FILE: &str = "project.toml";
pub const SCRIPT_FILE: &str = "script.toml";
pub const TAKES_DIR: &str = "takes";

const SCRIPT_HEADER: &str = "\
# Takewise script: one [[line]] table per line to speak.
#
# [[line]]
# id = \"hook\"          # stable slug [a-z0-9-], unique
# start = 0.0          # film seconds where the line should begin
# end = 5.0            # optional; defaults to the next line's start or the film's end
# text = \"The words to speak.\"
# note = \"warm\"        # optional direction for the narrator
";

/// An opened project directory.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub dir: PathBuf,
    pub config: ProjectConfig,
    pub script: Script,
}

/// `project.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub format: u32,
    /// The film, relative to the project directory or absolute.
    pub film: PathBuf,
    pub sample_rate: u32,
    pub film_info: FilmInfo,
    #[serde(default)]
    pub input: InputConfig,
    #[serde(default)]
    pub record: RecordConfig,
    #[serde(default)]
    pub check: CheckConfig,
    #[serde(default, skip_serializing_if = "OutputConfig::is_unset")]
    pub output: OutputConfig,
    #[serde(default, skip_serializing_if = "MonitorConfig::is_default")]
    pub monitor: MonitorConfig,
    /// The device pair and buffer `input.latency_offset_ms` was measured on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<CalibrationInfo>,
    /// The last setup check (written from milestone 4b on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<SetupInfo>,
    /// How `export` writes the narration; without it, 48 kHz 24-bit WAV,
    /// `raw`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export: Option<ExportConfig>,
}

/// `[export]`: the narration file's format and processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportConfig {
    pub format: ExportFormat,
    /// 16 or 24.
    pub bit_depth: u16,
    /// 48000 or 44100.
    pub sample_rate: u32,
    pub processing: Processing,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            format: ExportFormat::Wav,
            bit_depth: 24,
            sample_rate: 48_000,
            processing: Processing::Raw,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    #[default]
    Wav,
    Flac,
}

impl ExportFormat {
    /// The narration file's extension.
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Wav => "wav",
            ExportFormat::Flac => "flac",
        }
    }
}

/// `raw` places the takes as recorded; `clean` filters them and sets the
/// loudness (see `docs/design.md`, "Export cleanup and formats").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Processing {
    #[default]
    Raw,
    Clean,
}

impl Processing {
    pub fn as_str(self) -> &'static str {
        match self {
            Processing::Raw => "raw",
            Processing::Clean => "clean",
        }
    }
}

/// `[output]`: where film audio (and monitoring) plays.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    /// The output device, matched by name; unset means the system default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

impl OutputConfig {
    fn is_unset(&self) -> bool {
        self.device.is_none()
    }
}

/// `[monitor]`: whether the narrator hears their own voice, and how loud.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    pub enabled: bool,
    pub volume_db: f64,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            volume_db: -6.0,
        }
    }
}

impl MonitorConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// `[calibration]`: what the latency offset was measured on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationInfo {
    pub input: String,
    pub output: String,
    /// The capture buffer in frames; missing in calibrations that did not
    /// record it, which count as stale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_frames: Option<u32>,
    /// `YYYY-MM-DD`: the day of the measurement, also when copied.
    pub date: String,
    /// [`REMEMBERED`] when the offset was copied from an earlier
    /// measurement on these devices instead of measured for this project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// The `from` of a `[calibration]` or `[setup]` copied from the app's
/// remembered setup (see [`crate::remembered`]).
pub const REMEMBERED: &str = "remembered";

impl CalibrationInfo {
    /// Whether the offset came from an earlier measurement on these devices.
    pub fn is_remembered(&self) -> bool {
        self.from.as_deref() == Some(REMEMBERED)
    }
}

/// `[setup]`: the result of the setup check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetupInfo {
    pub date: String,
    pub input: String,
    pub channel: u16,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_floor_dbfs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_to_noise_db: Option<f64>,
    /// [`REMEMBERED`] when the result and its room tone were copied from
    /// a setup check of another project on the same day.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

impl SetupInfo {
    /// Whether the result was copied from another project's setup check.
    pub fn is_remembered(&self) -> bool {
        self.from.as_deref() == Some(REMEMBERED)
    }
}

/// What the film looked like when the project last measured it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilmInfo {
    pub duration_s: f64,
    pub size: u64,
    /// RFC 3339 modification time.
    pub modified: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputConfig {
    /// The input device, matched by name; unset until chosen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// 1-based input channel, recorded as mono.
    pub channel: u16,
    pub latency_offset_ms: i32,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            device: None,
            channel: 1,
            latency_offset_ms: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordConfig {
    pub preroll_s: f64,
    pub tail_s: f64,
    pub film_gain_db: f64,
    /// The level takes and passages play at when the narrator listens to
    /// them in the window (P, Shift+P), in dB; 0 at most, since the player
    /// cannot amplify. Export never uses it.
    pub voice_gain_db: f64,
    /// Consecutive lines belong to one passage while each line's slot ends
    /// less than this many seconds before the next line starts (see
    /// [`Script::passages`]); 0 makes every line a passage of its own.
    pub passage_gap_s: f64,
}

impl Default for RecordConfig {
    fn default() -> Self {
        Self {
            // A second for each digit of the 3-2-1 count-in.
            preroll_s: 3.0,
            tail_s: 1.5,
            film_gain_db: -12.0,
            voice_gain_db: 0.0,
            passage_gap_s: 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CheckConfig {
    pub whisper_model: String,
    pub language: String,
}

impl Default for CheckConfig {
    fn default() -> Self {
        Self {
            whisper_model: "ggml-large-v3-turbo.bin".to_owned(),
            language: "en".to_owned(),
        }
    }
}

impl Project {
    /// Creates a project with an empty script; fails if `project.toml` exists.
    pub fn init(dir: &Path, film: &Path, film_info: FilmInfo) -> Result<Project> {
        let project_file = dir.join(PROJECT_FILE);
        if project_file.exists() {
            return Err(Error::AlreadyExists(dir.to_path_buf()));
        }
        fs::create_dir_all(dir.join(TAKES_DIR)).map_err(io(dir))?;
        let config = ProjectConfig {
            format: 1,
            film: film.to_path_buf(),
            sample_rate: 48_000,
            film_info,
            input: InputConfig::default(),
            record: RecordConfig::default(),
            check: CheckConfig::default(),
            output: OutputConfig::default(),
            monitor: MonitorConfig::default(),
            calibration: None,
            setup: None,
            export: None,
        };
        let text = toml::to_string(&config).map_err(|err| Error::Parse {
            path: project_file.clone(),
            message: err.to_string(),
        })?;
        write_new(&project_file, text.as_bytes()).map_err(|err| match err {
            Error::Io { source, .. } if source.kind() == std::io::ErrorKind::AlreadyExists => {
                Error::AlreadyExists(dir.to_path_buf())
            }
            other => other,
        })?;
        let script_file = dir.join(SCRIPT_FILE);
        if !script_file.exists() {
            write_new(&script_file, SCRIPT_HEADER.as_bytes())?;
        }
        Ok(Project {
            dir: dir.to_path_buf(),
            config,
            script: Script::default(),
        })
    }

    /// Reads `project.toml` and `script.toml`.
    pub fn open(dir: &Path) -> Result<Project> {
        let config = read_config(dir)?;
        let script = read_script(dir)?;
        Ok(Project {
            dir: dir.to_path_buf(),
            config,
            script,
        })
    }

    /// Reads `project.toml` and `script.toml`, and returns the version of the
    /// exact script bytes that were parsed, so the two always agree.
    pub fn open_versioned(dir: &Path) -> Result<(Project, ScriptVersion)> {
        let config = read_config(dir)?;
        let bytes = read_script_bytes(dir)?;
        let version = ScriptVersion::of(&bytes);
        let path = dir.join(SCRIPT_FILE);
        let text = String::from_utf8(bytes).map_err(|err| Error::Parse {
            path: path.clone(),
            message: err.to_string(),
        })?;
        let script = parse_script(&path, &text)?;
        let project = Project {
            dir: dir.to_path_buf(),
            config,
            script,
        };
        Ok((project, version))
    }

    /// The film's path, with a relative `film` resolved against the project.
    pub fn film_path(&self) -> PathBuf {
        if self.config.film.is_absolute() {
            self.config.film.clone()
        } else {
            self.dir.join(&self.config.film)
        }
    }

    pub fn film_duration(&self) -> f64 {
        self.config.film_info.duration_s
    }
}

/// Writes a file that must not exist yet.
fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io(path))?;
    file.write_all(bytes).map_err(io(path))?;
    file.sync_all().map_err(io(path))
}

pub fn read_config(dir: &Path) -> Result<ProjectConfig> {
    let path = dir.join(PROJECT_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::NotAProject(dir.to_path_buf()));
        }
        Err(err) => return Err(io(&path)(err)),
    };
    toml::from_str(&text).map_err(|err| Error::Parse {
        path,
        message: err.to_string(),
    })
}

pub fn read_script(dir: &Path) -> Result<Script> {
    let path = dir.join(SCRIPT_FILE);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Script::default()),
        Err(err) => return Err(io(&path)(err)),
    };
    parse_script(&path, &text)
}

/// The bytes of `script.toml`; an absent file is empty.
pub(crate) fn read_script_bytes(dir: &Path) -> Result<Vec<u8>> {
    let path = dir.join(SCRIPT_FILE);
    match fs::read(&path) {
        Ok(bytes) => Ok(bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(io(&path)(err)),
    }
}

pub(crate) fn parse_script(path: &Path, text: &str) -> Result<Script> {
    toml::from_str(text).map_err(|err| Error::Parse {
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> FilmInfo {
        FilmInfo {
            duration_s: 51.5,
            size: 12_345_678,
            modified: "2026-11-22T20:05:11Z".to_owned(),
        }
    }

    #[test]
    fn init_then_open_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("promo.takewise");
        let created = Project::init(&dir, Path::new("../film.mp4"), info()).unwrap();
        assert!(dir.join("takes").is_dir());
        assert!(dir.join("script.toml").is_file());

        let opened = Project::open(&dir).unwrap();
        assert_eq!(opened.config, created.config);
        assert_eq!(opened.config.format, 1);
        assert_eq!(opened.config.sample_rate, 48_000);
        assert_eq!(opened.config.film_info, info());
        assert_eq!(opened.config.input.channel, 1);
        assert_eq!(opened.config.input.latency_offset_ms, 0);
        assert!((opened.config.record.preroll_s - 3.0).abs() < f64::EPSILON);
        assert!((opened.config.record.tail_s - 1.5).abs() < f64::EPSILON);
        assert!((opened.config.record.film_gain_db + 12.0).abs() < f64::EPSILON);
        assert!(opened.config.record.voice_gain_db.abs() < f64::EPSILON);
        assert!((opened.config.record.passage_gap_s - 1.0).abs() < f64::EPSILON);
        assert_eq!(opened.config.check.language, "en");
        assert_eq!(opened.config.check.whisper_model, "ggml-large-v3-turbo.bin");
        assert_eq!(opened.script.lines, []);
        assert_eq!(opened.film_path(), dir.join("../film.mp4"));
    }

    #[test]
    fn old_project_without_new_tables_opens() {
        let tmp = tempfile::tempdir().unwrap();
        let text = "format = 1\nfilm = \"film.mp4\"\nsample_rate = 48000\n\n\
                    [film_info]\nduration_s = 51.5\nsize = 1\nmodified = \"2026-11-22T20:05:11Z\"\n\n\
                    [input]\ndevice = \"Mic\"\nchannel = 2\nlatency_offset_ms = 12\n";
        fs::write(tmp.path().join(PROJECT_FILE), text).unwrap();
        let config = read_config(tmp.path()).unwrap();
        assert_eq!(config.output, OutputConfig::default());
        assert_eq!(config.monitor, MonitorConfig::default());
        assert!(!config.monitor.enabled);
        assert!((config.monitor.volume_db + 6.0).abs() < f64::EPSILON);
        assert_eq!(config.calibration, None);
        assert_eq!(config.setup, None);
        assert_eq!(config.input.latency_offset_ms, 12);
        assert!((config.record.passage_gap_s - 1.0).abs() < f64::EPSILON);
        assert!(
            config.record.voice_gain_db.abs() < f64::EPSILON,
            "takes play at full level in projects made before 0.2.1"
        );

        // A new project's file has none of the new tables either.
        let dir = tmp.path().join("new");
        Project::init(&dir, Path::new("film.mp4"), info()).unwrap();
        let written = fs::read_to_string(dir.join(PROJECT_FILE)).unwrap();
        for table in ["[output]", "[monitor]", "[calibration]", "[setup]"] {
            assert!(!written.contains(table), "{table} in {written}");
        }
    }

    #[test]
    fn absolute_film_path_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("p");
        let project = Project::init(&dir, Path::new("/films/a.mp4"), info()).unwrap();
        assert_eq!(project.film_path(), PathBuf::from("/films/a.mp4"));
    }

    #[test]
    fn init_refuses_existing_project() {
        let tmp = tempfile::tempdir().unwrap();
        Project::init(tmp.path(), Path::new("film.mp4"), info()).unwrap();
        let err = Project::init(tmp.path(), Path::new("film.mp4"), info()).unwrap_err();
        assert!(matches!(err, Error::AlreadyExists(_)), "{err:?}");
    }

    #[test]
    fn open_versioned_hashes_the_bytes_it_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        Project::init(tmp.path(), Path::new("film.mp4"), info()).unwrap();
        let text = "[[line]]\nid = \"hook\"\nstart = 1.0\ntext = \"Hi.\"\n";
        fs::write(tmp.path().join(SCRIPT_FILE), text).unwrap();
        let (project, version) = Project::open_versioned(tmp.path()).unwrap();
        assert_eq!(project.script.lines[0].id, "hook");
        assert_eq!(version, ScriptVersion::of(text.as_bytes()));
        assert_eq!(project, Project::open(tmp.path()).unwrap());
    }

    #[test]
    fn open_missing_project_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let err = Project::open(&tmp.path().join("nope")).unwrap_err();
        assert!(matches!(err, Error::NotAProject(_)), "{err:?}");
    }
}
