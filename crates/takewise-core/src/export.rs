//! Export: one narration file (WAV or FLAC) aligned to the film's first
//! frame, and a manifest. `raw` places the takes as recorded; `clean`
//! filters each take and sets one static gain for the loudness (see
//! `docs/design.md`, "Export and Resolve handoff" and "Export cleanup and
//! formats").

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::check::speech_onset;
use crate::clean::{Cleaner, NOISE_REDUCTION_DB, find_room_tone};
use crate::error::{Error, Result, io};
use crate::ffmpeg::{Ffmpeg, read_float_wav};
use crate::fsutil::{lock, rename_temp, write_temp};
use crate::ledger::{Checked, EventKind, Ledger, Verdict, read};
use crate::project::{ExportConfig, ExportFormat, Processing, Project};
use crate::render::{self, Loudness, MAX_TRUE_PEAK_DBTP, SOURCE_RATE, TARGET_LUFS, Target, UNIT};
use crate::script::{Line, validate};
use crate::speech::{Analysis, analyze_take};
use crate::state::{State, TakeState, fold};
use crate::takes::read_take_wav;

pub const EXPORT_DIR: &str = "export";
/// The narration file as a 48 kHz WAV; see [`narration_file`].
pub const NARRATION_FILE: &str = "narration.wav";
pub const MANIFEST_FILE: &str = "manifest.json";
/// Audio kept on either side of the detected speech.
const MARGIN_S: f64 = 0.150;
const FADE_S: f64 = 0.010;
/// Speech of two takes closer than this gets a `tight-gap` warning: there
/// is no room for two full fades between them.
const TIGHT_GAP_S: f64 = 0.020;
/// `clean` reached its loudness target when the final file lies this close
/// to [`TARGET_LUFS`].
const LOUDNESS_TOLERANCE_LU: f64 = 0.5;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportOptions {
    /// Use the proposed take for lines without a chosen one.
    pub use_proposed: bool,
    /// Override `[export]` (or its defaults) for this export.
    pub format: Option<ExportFormat>,
    pub bit_depth: Option<u16>,
    pub sample_rate: Option<u32>,
    pub processing: Option<Processing>,
}

impl ExportOptions {
    /// The settings in effect: these options over `[export]`, over the
    /// defaults (48 kHz 24-bit WAV, `raw`).
    pub fn settings(&self, config: Option<&ExportConfig>) -> Result<ExportConfig> {
        let base = config.copied().unwrap_or_default();
        let settings = ExportConfig {
            format: self.format.unwrap_or(base.format),
            bit_depth: self.bit_depth.unwrap_or(base.bit_depth),
            sample_rate: self.sample_rate.unwrap_or(base.sample_rate),
            processing: self.processing.unwrap_or(base.processing),
        };
        if ![16, 24].contains(&settings.bit_depth) {
            return Err(Error::ExportSetting(format!(
                "bit depth {} is not supported; use 16 or 24",
                settings.bit_depth
            )));
        }
        if ![48_000, 44_100].contains(&settings.sample_rate) {
            return Err(Error::ExportSetting(format!(
                "sample rate {} is not supported; use 48000 or 44100",
                settings.sample_rate
            )));
        }
        Ok(settings)
    }
}

/// The narration file's name for a format: `narration.wav` or
/// `narration.flac`.
pub fn narration_file(format: ExportFormat) -> String {
    format!("narration.{}", format.extension())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ExportReport {
    /// The narration file written, relative to the export directory;
    /// empty when refused.
    pub narration: String,
    /// The settings it was written with; `None` when refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<ExportSettings>,
    pub lines: Vec<ManifestLine>,
    /// Why nothing was written; empty on success.
    pub refused: Vec<Refusal>,
    /// Exported, but worth a look; also listed in the manifest.
    pub warnings: Vec<ExportWarning>,
}

/// How the narration file was written, as the manifest states it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ExportSettings {
    pub processing: Processing,
    pub format: ExportFormat,
    pub sample_rate: u32,
    /// The takes' rate, which source ranges count in.
    pub source_sample_rate: u32,
    pub bit_depth: u16,
    /// Integrated loudness and true peak of the final file (`clean`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loudness_lufs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub true_peak_dbtp: Option<f64>,
    /// The static gain `clean` applied before its limiter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<f64>,
    /// The most gain reduction `clean`'s peak limiter applied, in dB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limiter_max_reduction_db: Option<f64>,
    /// Whether the final file's integrated loudness lies within 0.5 LU of
    /// −16 LUFS (`clean`); false when the limiter's 6 dB cap stopped it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loudness_target_reached: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportWarning {
    pub code: &'static str,
    /// The line it is about; absent for the whole narration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Refusal {
    pub code: &'static str,
    pub lines: Vec<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ManifestLine {
    pub id: String,
    pub text: String,
    pub take_file: String,
    /// Who chose the take, or `proposed`.
    pub chosen_by: String,
    /// Half-open sample range taken from the take, at the take's rate.
    pub source_samples: [u64; 2],
    /// Half-open sample range in the narration file, at its rate
    /// (`round(n × rate / 48000)` of the range at 48 kHz).
    pub dest_samples: [u64; 2],
    /// Detected speech start and end in film seconds, clamped to the film;
    /// a `speech-cropped` warning gives the unclamped values. The start is
    /// where the check's early-start rule measures it, after a breath set
    /// apart from the words.
    pub speech_film_s: [f64; 2],
    /// The transcribed words in film seconds, from the take's latest check
    /// against the line as it is now; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub words_film_s: Option<Vec<ManifestWord>>,
    /// `clean` only: the noise reduction the take got, or why it was
    /// skipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noise_reduction: Option<String>,
}

/// A transcribed word placed in the film.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ManifestWord {
    pub text: String,
    /// Film seconds, as heard: not clamped to the film.
    pub start: f64,
    pub end: f64,
}

#[derive(Serialize)]
struct Manifest<'a> {
    v: u32,
    film: ManifestFilm<'a>,
    /// The file this manifest describes; a mismatch means the pair is torn.
    narration: ManifestNarration,
    #[serde(flatten)]
    settings: ExportSettings,
    lines: &'a [ManifestLine],
    warnings: &'a [ExportWarning],
}

#[derive(Serialize)]
struct ManifestNarration {
    /// Relative to the manifest.
    file: String,
    size: u64,
    sha256: String,
}

#[derive(Serialize)]
struct ManifestFilm<'a> {
    path: String,
    duration_s: f64,
    size: u64,
    modified: &'a str,
}

/// A take ready to be mixed in.
struct Placed {
    line: ManifestLine,
    /// The take (or its cleaned copy) at 48 kHz, in 24-bit units.
    samples: Vec<f64>,
    take: u32,
    /// The take's pre-roll noise floor, for matching a room tone.
    floor_dbfs: f64,
    /// Source start and destination start and end, after cropping.
    src: usize,
    dest: (usize, usize),
    /// The speech without its margins in destination samples, cropped.
    speech: (usize, usize),
    /// The most samples the fade in and the fade out may take: a margin
    /// shortened by a split gap leaves less room, so the fades never cut
    /// into the speech.
    fade_room: (usize, usize),
    /// The take was recorded against different text than the line has now.
    old_text: bool,
    /// The unclamped speech in film seconds, when part of it lies outside
    /// the film.
    cropped_speech: Option<(f64, f64)>,
    /// `clean` only: why noise reduction was skipped for this take.
    nr_skipped: Option<String>,
}

#[allow(clippy::cast_possible_truncation)]
fn seconds_to_samples(seconds: f64, rate: u32) -> i64 {
    (seconds * f64::from(rate)).round() as i64
}

fn to_usize(n: i64) -> usize {
    usize::try_from(n).unwrap_or(0)
}

fn to_u64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Collects refusals by code, keeping the order lines were found in.
#[derive(Default)]
struct Refusals(BTreeMap<&'static str, (Vec<String>, String)>);

impl Refusals {
    fn add(&mut self, code: &'static str, line: &str, message: &'static str) {
        let entry = self
            .0
            .entry(code)
            .or_insert_with(|| (Vec::new(), message.to_owned()));
        entry.0.push(line.to_owned());
    }
}

/// The film on disk as the caller found it just before export, to compare
/// with the `[film_info]` the timings were made against.
#[derive(Debug, Clone, PartialEq)]
pub enum FilmOnDisk {
    Missing,
    Found {
        size: u64,
        /// RFC 3339, whole seconds, as `[film_info] modified`.
        modified: String,
        /// `None` when it could not be measured (no `ffprobe`): the size
        /// and modification time decide alone.
        duration_s: Option<f64>,
    },
    /// `ffprobe` stalled and was stopped: whether the film is the one
    /// measured is unknown, so export refuses with `film-probe-timeout`.
    ProbeTimedOut,
}

/// The refusal for a film probe that stalled.
fn probe_timeout_refusal() -> Refusal {
    Refusal {
        code: "film-probe-timeout",
        lines: Vec::new(),
        message: format!(
            "ffprobe did not read the film within {} s and was stopped; export again",
            crate::ffmpeg::FILM_PROBE_TIMEOUT.as_secs()
        ),
    }
}

/// Durations closer than this are the same film.
const FILM_DURATION_TOLERANCE_S: f64 = 1e-3;

/// Why the film no longer matches what the project measured, if it does
/// not: `film-missing` or `film-changed`.
fn film_refusal(project: &Project, film: &FilmOnDisk) -> Option<Refusal> {
    let info = &project.config.film_info;
    let refresh = "check the timings, then run `takewise status --refresh-film`";
    match film {
        FilmOnDisk::ProbeTimedOut => Some(probe_timeout_refusal()),
        FilmOnDisk::Missing => Some(Refusal {
            code: "film-missing",
            lines: Vec::new(),
            message: format!("the film {} is missing", project.film_path().display()),
        }),
        FilmOnDisk::Found {
            size,
            modified,
            duration_s,
        } => {
            let same = *size == info.size
                && *modified == info.modified
                && duration_s
                    .is_none_or(|d| (d - info.duration_s).abs() <= FILM_DURATION_TOLERANCE_S);
            (!same).then(|| Refusal {
                code: "film-changed",
                lines: Vec::new(),
                message: format!("the film changed since it was measured; {refresh}"),
            })
        }
    }
}

/// What an export read: a hash of `project.toml`, `script.toml` and
/// `takes.jsonl`. The export publishes only when it is unchanged after
/// rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StateVersion([u8; 32]);

fn state_version(dir: &Path) -> Result<StateVersion> {
    let mut hash = Sha256::new();
    for file in [
        crate::project::PROJECT_FILE,
        crate::project::SCRIPT_FILE,
        crate::ledger::LEDGER_FILE,
    ] {
        let path = dir.join(file);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(io(&path)(err)),
        };
        hash.update(to_u64(bytes.len()).to_le_bytes());
        hash.update(&bytes);
    }
    Ok(StateVersion(hash.finalize().into()))
}

/// Writes `export/narration.wav` (film length, mono 24-bit) and
/// `export/manifest.json` from each line's chosen take, after comparing the
/// film on disk (`film` probes it) with the film the project measured.
///
/// The project, script and ledger are read under the project lock, which is
/// then released while the takes are loaded and analyzed, cleaned and the
/// narration rendered (`ffmpeg` can take a while). The lock is taken again
/// to publish: only when the project, script and ledger are unchanged, so
/// the pair reflects the state at write time, and only when the film probes
/// as it did at the start (`film-missing`, `film-changed` otherwise). When
/// the state changed, the export renders once more from the new state, then
/// refuses with `state-changed`.
///
/// On refusal it writes nothing and removes an earlier manifest, so no stale
/// pair looks valid; the export is regenerable and takes are untouched.
pub fn export(
    dir: &Path,
    opts: &ExportOptions,
    film: &dyn Fn() -> Result<FilmOnDisk>,
) -> Result<ExportReport> {
    export_observed(dir, opts, film, &mut |_| {})
}

/// How many times export renders before refusing with `state-changed`.
const RENDER_ATTEMPTS: usize = 2;

/// Where an export is when [`export_observed`] reports to its observer;
/// both stages run without the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// The takes are loaded, analyzed and placed.
    Planned,
    /// The narration is rendered to its temp file.
    Rendered,
}

/// [`export`], reporting each [`Stage`] to `observe`.
fn export_observed(
    dir: &Path,
    opts: &ExportOptions,
    film: &dyn Fn() -> Result<FilmOnDisk>,
    observe: &mut dyn FnMut(Stage),
) -> Result<ExportReport> {
    let export_dir = dir.join(EXPORT_DIR);
    let started = film()?;
    for _ in 0..RENDER_ATTEMPTS {
        let (project, ledger, version) = {
            let _guard = lock(dir)?;
            let version = state_version(dir)?;
            (Project::open(dir)?, read(dir)?, version)
        };
        let state = fold(&project, &ledger);
        let settings = opts.settings(project.config.export.as_ref())?;
        let planned = plan(&project, &ledger, &state, opts, &started);
        observe(Stage::Planned);
        let plan = match planned {
            Ok(plan) => plan,
            Err(refused) => {
                let _guard = lock(dir)?;
                if state_version(dir)? == version {
                    return refuse(&export_dir, refused);
                }
                continue;
            }
        };
        let render = render_export(&project, &ledger, settings, plan)?;
        observe(Stage::Rendered);
        let _guard = lock(dir)?;
        let checked = state_version(dir).and_then(|now| Ok((now == version, film()?)));
        let (unchanged, now) = match checked {
            Ok(checked) => checked,
            Err(err) => {
                let _ = fs::remove_file(&render.narration_temp);
                return Err(err);
            }
        };
        if unchanged {
            if let Some(refusal) = film_moved(&project, &started, &now) {
                let _ = fs::remove_file(&render.narration_temp);
                return refuse(&export_dir, vec![refusal]);
            }
            return publish_export(&project, &export_dir, render);
        }
        let _ = fs::remove_file(&render.narration_temp);
    }
    let _guard = lock(dir)?;
    refuse(
        &export_dir,
        vec![Refusal {
            code: "state-changed",
            lines: Vec::new(),
            message: format!(
                "the project changed while the export rendered, {RENDER_ATTEMPTS} times; export again when it is quiet"
            ),
        }],
    )
}

/// Why the film probed before publishing (`now`) is not the one export
/// started with, if it is not: `film-missing` or `film-changed`.
fn film_moved(project: &Project, started: &FilmOnDisk, now: &FilmOnDisk) -> Option<Refusal> {
    let same = match (started, now) {
        (FilmOnDisk::Missing, FilmOnDisk::Missing) => true,
        (
            FilmOnDisk::Found {
                size,
                modified,
                duration_s,
            },
            FilmOnDisk::Found {
                size: size_now,
                modified: modified_now,
                duration_s: duration_now,
            },
        ) => {
            size == size_now
                && modified == modified_now
                && match (duration_s, duration_now) {
                    (Some(a), Some(b)) => (a - b).abs() <= FILM_DURATION_TOLERANCE_S,
                    (None, None) => true,
                    _ => false,
                }
        }
        _ => false,
    };
    if same {
        return None;
    }
    Some(match now {
        FilmOnDisk::ProbeTimedOut => probe_timeout_refusal(),
        FilmOnDisk::Missing => Refusal {
            code: "film-missing",
            lines: Vec::new(),
            message: format!(
                "the film {} went missing while the export rendered",
                project.film_path().display()
            ),
        },
        FilmOnDisk::Found { .. } => Refusal {
            code: "film-changed",
            lines: Vec::new(),
            message: "the film changed while the export rendered; check the timings, then run `takewise status --refresh-film`".to_owned(),
        },
    })
}

/// Removes an earlier manifest and reports `refused`.
fn refuse(export_dir: &Path, refused: Vec<Refusal>) -> Result<ExportReport> {
    remove_if_present(&export_dir.join(MANIFEST_FILE))?;
    Ok(ExportReport {
        refused,
        ..ExportReport::default()
    })
}

/// The takes placed in the film, ready to render.
struct Plan {
    placed: Vec<Placed>,
    tight: Vec<ExportWarning>,
}

/// Checks the film and places each line's take, or says why export
/// refuses.
fn plan(
    project: &Project,
    ledger: &Ledger,
    state: &State,
    opts: &ExportOptions,
    film: &FilmOnDisk,
) -> std::result::Result<Plan, Vec<Refusal>> {
    let (placed, tight, mut refused) = gather(project, ledger, state, opts.use_proposed);
    if let Some(film) = film_refusal(project, film) {
        refused.insert(0, film);
    }
    if refused.is_empty() {
        Ok(Plan { placed, tight })
    } else {
        Err(refused)
    }
}

/// A rendered narration in its temp file, with what the manifest says.
struct Rendered {
    narration_temp: PathBuf,
    narration_path: PathBuf,
    file: String,
    narration: ManifestNarration,
    settings: ExportConfig,
    report: ExportSettings,
    lines: Vec<ManifestLine>,
    warnings: Vec<ExportWarning>,
}

/// Cleans the takes (`clean`), mixes and renders the narration into a temp
/// file in the export directory. Runs without the lock.
fn render_export(
    project: &Project,
    ledger: &Ledger,
    settings: ExportConfig,
    plan: Plan,
) -> Result<Rendered> {
    let Plan { mut placed, tight } = plan;
    let dir = project.dir.join(EXPORT_DIR);
    let ffmpeg = ffmpeg_for(settings)?;
    if settings.processing == Processing::Clean {
        let ffmpeg = ffmpeg.as_ref().ok_or_else(|| missing_ffmpeg(settings))?;
        clean_takes(project, ledger, &mut placed, ffmpeg)?;
    }
    let film_len = to_usize(seconds_to_samples(
        project.film_duration(),
        project.config.sample_rate,
    ));
    let mixed = mix(&placed, film_len, project.config.sample_rate);
    let target = Target {
        sample_rate: settings.sample_rate,
        bit_depth: settings.bit_depth,
        format: settings.format,
    };
    fs::create_dir_all(&dir).map_err(io(&dir))?;
    let file = narration_file(settings.format);
    let narration_path = dir.join(&file);
    let (narration_temp, report) =
        render_narration(&mixed, settings, target, ffmpeg.as_ref(), &narration_path)?;
    let narration = match describe_file(&narration_temp, &file) {
        Ok(narration) => narration,
        Err(err) => {
            let _ = fs::remove_file(&narration_temp);
            return Err(io(&narration_path)(err));
        }
    };

    let mut warnings = warnings(&placed, project.film_duration());
    warnings.extend(tight);
    warnings.extend(loudness_warning(&report));
    let lines: Vec<ManifestLine> = placed
        .into_iter()
        .map(|p| {
            let mut line = p.line;
            line.dest_samples = line
                .dest_samples
                .map(|n| render::at_rate(n, settings.sample_rate));
            line
        })
        .collect();
    Ok(Rendered {
        narration_temp,
        narration_path,
        file,
        narration,
        settings,
        report,
        lines,
        warnings,
    })
}

/// Writes the manifest and puts the rendered pair in place. Under the lock.
fn publish_export(project: &Project, dir: &Path, render: Rendered) -> Result<ExportReport> {
    let info = &project.config.film_info;
    let manifest = Manifest {
        v: 1,
        film: ManifestFilm {
            path: project.film_path().display().to_string(),
            duration_s: info.duration_s,
            size: info.size,
            modified: &info.modified,
        },
        narration: render.narration,
        settings: render.report,
        lines: &render.lines,
        warnings: &render.warnings,
    };
    publish(
        dir,
        &manifest,
        &render.narration_temp,
        &render.narration_path,
        render.settings.format,
    )?;
    Ok(ExportReport {
        narration: render.file,
        settings: Some(render.report),
        lines: render.lines,
        refused: Vec::new(),
        warnings: render.warnings,
    })
}

/// A `loudness-target-missed` warning when `clean` could not bring the
/// narration to its loudness target.
fn loudness_warning(report: &ExportSettings) -> Option<ExportWarning> {
    if report.loudness_target_reached != Some(false) {
        return None;
    }
    let measured = report
        .loudness_lufs
        .map_or_else(String::new, |lufs| format!(" measures {lufs:.1} LUFS and"));
    Some(ExportWarning {
        code: "loudness-target-missed",
        line: None,
        message: format!(
            "the narration{measured} misses {TARGET_LUFS:.0} LUFS by more than {LOUDNESS_TOLERANCE_LU} LU: the peak limiter stops at 6 dB, so very dynamic speech stays quieter; raise it in the edit or record the loud parts softer"
        ),
    })
}

/// Places each line's take, or collects why export refuses: script
/// problems, lines without a usable take, overlapping speech. Splits the
/// margins of neighbours and returns `tight-gap` warnings.
fn gather(
    project: &Project,
    ledger: &Ledger,
    state: &State,
    use_proposed: bool,
) -> (Vec<Placed>, Vec<ExportWarning>, Vec<Refusal>) {
    let mut refused = Vec::new();
    let problems = validate(&project.script, project.film_duration());
    if !problems.is_empty() {
        let mut lines: Vec<String> = problems.iter().filter_map(|p| p.line.clone()).collect();
        lines.dedup();
        refused.push(Refusal {
            code: "invalid-script",
            lines,
            message: "script.toml has validation problems; see `takewise status`".to_owned(),
        });
    }

    let mut refusals = Refusals::default();
    let mut placed: Vec<Placed> = Vec::new();
    for line in &project.script.lines {
        match place(project, ledger, state, use_proposed, line) {
            Ok(p) => placed.push(p),
            Err((code, message)) => refusals.add(code, &line.id, message),
        }
    }
    for (code, (lines, message)) in refusals.0 {
        let message = format!("{message}: {}", lines.join(", "));
        refused.push(Refusal {
            code,
            lines,
            message,
        });
    }
    let tight = split_margins(&mut placed, project.config.sample_rate);
    refused.extend(overlaps(&placed));
    (placed, tight, refused)
}

/// Writes the manifest and puts both files in place: the old manifest goes
/// first, then the narration, the manifest last, so an interruption
/// anywhere leaves no manifest, or one whose hash does not match the
/// narration, but never an old manifest vouching for a changed export. A
/// narration of the other format goes too, so one narration file sits
/// beside the manifest.
fn publish(
    dir: &Path,
    manifest: &Manifest<'_>,
    narration_temp: &Path,
    narration_path: &Path,
    format: ExportFormat,
) -> Result<()> {
    let manifest_path = dir.join(MANIFEST_FILE);
    let manifest_temp = match write_manifest(&manifest_path, manifest) {
        Ok(temp) => temp,
        Err(err) => {
            let _ = fs::remove_file(narration_temp);
            return Err(err);
        }
    };
    if let Err(err) = remove_if_present(&manifest_path) {
        let _ = fs::remove_file(narration_temp);
        let _ = fs::remove_file(&manifest_temp);
        return Err(err);
    }
    if let Err(err) = rename_temp(narration_temp, narration_path) {
        let _ = fs::remove_file(&manifest_temp);
        return Err(err);
    }
    for other in [ExportFormat::Wav, ExportFormat::Flac] {
        if other != format {
            remove_if_present(&dir.join(narration_file(other)))?;
        }
    }
    rename_temp(&manifest_temp, &manifest_path)
}

/// `ffmpeg` when the settings need it: `clean`, 44.1 kHz or FLAC; `raw`
/// 48 kHz WAV works without it.
fn ffmpeg_for(settings: ExportConfig) -> Result<Option<Ffmpeg>> {
    let needed = settings.processing == Processing::Clean
        || settings.sample_rate != SOURCE_RATE
        || settings.format == ExportFormat::Flac;
    if !needed {
        return Ok(None);
    }
    Ffmpeg::find()
        .map(Some)
        .ok_or_else(|| missing_ffmpeg(settings))
}

fn missing_ffmpeg(settings: ExportConfig) -> Error {
    let mut needs = Vec::new();
    if settings.processing == Processing::Clean {
        needs.push("clean processing");
    }
    if settings.sample_rate != SOURCE_RATE {
        needs.push("44.1 kHz");
    }
    if settings.format == ExportFormat::Flac {
        needs.push("FLAC");
    }
    Error::MissingFfmpeg(format!(
        "ffmpeg not found on PATH or in /opt/homebrew/bin; {} need it. Install it with `brew install ffmpeg`, or export raw 48 kHz WAV (--processing raw --format wav --sample-rate 48000)",
        needs.join(" and ")
    ))
}

/// Replaces each placed take's samples with its cleaned copy (the whole
/// take, so placement is unchanged) and notes its noise reduction.
fn clean_takes(
    project: &Project,
    ledger: &Ledger,
    placed: &mut [Placed],
    ffmpeg: &Ffmpeg,
) -> Result<()> {
    let processor = Cleaner::new(ffmpeg.clone()).map_err(Error::Ffmpeg)?;
    let tmp = tempfile::Builder::new()
        .prefix("takewise-clean-")
        .tempdir()
        .map_err(io(std::env::temp_dir()))?;
    for (i, p) in placed.iter_mut().enumerate() {
        let recorded = ledger.events.iter().find_map(|e| match &e.kind {
            EventKind::Recorded(rec) if e.line == p.line.id && e.take == p.take => {
                Some((rec, e.at))
            }
            _ => None,
        });
        let room = match recorded {
            Some((rec, at)) => find_room_tone(&project.dir, rec, at, p.floor_dbfs),
            None => Err("the take has no recorded event".to_owned()),
        };
        let src = project.dir.join(&p.line.take_file);
        let out = tmp.path().join(format!("{i}.wav"));
        let info = processor
            .clean_take(&src, room.as_ref().ok(), &out)
            .map_err(Error::Ffmpeg)?;
        let (cleaned, _) = read_float_wav(&out).map_err(Error::Ffmpeg)?;
        let len = p.samples.len();
        p.samples = cleaned.into_iter().map(|v| v * UNIT).collect();
        p.samples.resize(len, 0.0);
        p.nr_skipped = match (&room, info.noise_floor_db) {
            (Ok(_), Some(_)) => None,
            (Err(why), _) => Some(why.clone()),
            (Ok(_), None) => Some("no noise floor to reduce to".to_owned()),
        };
        p.line.noise_reduction = Some(match (&room, info.noise_floor_db, &p.nr_skipped) {
            (Ok(tone), Some(floor), _) => format!(
                "afftdn {NOISE_REDUCTION_DB:.0} dB at a noise floor of {floor:.1} dB from {}",
                tone.file
            ),
            (_, _, Some(why)) => format!("skipped: {why}"),
            _ => "skipped".to_owned(),
        });
    }
    Ok(())
}

/// Renders the narration to a temp file beside `path`: `clean` levels the
/// placed narration to −16 LUFS (one static gain through a true-peak
/// limiter that reduces at most 6 dB; see [`render::level`]) and measures
/// the final file again; a true peak above −1 dBTP after resampling or
/// dither lowers the gain by the excess and renders once more. Returns the
/// temp file and the settings for the manifest.
fn render_narration(
    mixed: &[f64],
    settings: ExportConfig,
    target: Target,
    ffmpeg: Option<&Ffmpeg>,
    path: &Path,
) -> Result<(PathBuf, ExportSettings)> {
    let clean = settings.processing == Processing::Clean;
    let leveled = if clean {
        let ffmpeg = ffmpeg.ok_or_else(|| missing_ffmpeg(settings))?;
        Some(render::level(ffmpeg, mixed).map_err(Error::Ffmpeg)?)
    } else {
        None
    };
    let source = leveled.as_ref().map_or(mixed, |l| &l.samples);
    let encode = |trim_db: f64| -> Result<PathBuf> {
        let pcm = render::prepare(source, trim_db, target, ffmpeg).map_err(Error::Ffmpeg)?;
        let temp = write_temp(path, |_| Ok(()))?;
        if let Err(err) = render::write(&temp, &pcm, target, ffmpeg) {
            let _ = fs::remove_file(&temp);
            return Err(Error::Ffmpeg(err));
        }
        Ok(temp)
    };
    let mut trim_db = 0.0;
    let mut temp = encode(trim_db)?;
    let mut loudness: Option<Loudness> = None;
    if clean && let Some(ffmpeg) = ffmpeg {
        let mut after = measure_or_remove(ffmpeg, &temp)?;
        if after.true_peak_dbtp > MAX_TRUE_PEAK_DBTP {
            let _ = fs::remove_file(&temp);
            trim_db = MAX_TRUE_PEAK_DBTP - after.true_peak_dbtp;
            temp = encode(trim_db)?;
            after = measure_or_remove(ffmpeg, &temp)?;
        }
        loudness = Some(after);
    }
    let round = |db: f64| (db * 100.0).round() / 100.0;
    Ok((
        temp,
        ExportSettings {
            processing: settings.processing,
            format: settings.format,
            sample_rate: settings.sample_rate,
            source_sample_rate: SOURCE_RATE,
            bit_depth: settings.bit_depth,
            loudness_lufs: loudness.map(|l| l.integrated_lufs),
            true_peak_dbtp: loudness.map(|l| l.true_peak_dbtp),
            gain_db: leveled.as_ref().map(|l| round(l.gain_db + trim_db)),
            limiter_max_reduction_db: leveled.as_ref().map(|l| round(l.max_reduction_db)),
            loudness_target_reached: loudness
                .map(|l| (l.integrated_lufs - TARGET_LUFS).abs() <= LOUDNESS_TOLERANCE_LU),
        },
    ))
}

fn measure_or_remove(ffmpeg: &Ffmpeg, temp: &Path) -> Result<Loudness> {
    render::measure(ffmpeg, temp).map_err(|err| {
        let _ = fs::remove_file(temp);
        Error::Ffmpeg(err)
    })
}

/// The analysis of a take to place: with the floor of its own pre-roll or,
/// for a piece of a passage, of the passage's.
fn analyze_placed(project: &Project, samples: &[i32], rate: u32, take: &TakeState) -> Analysis {
    let preroll_s = project.config.record.preroll_s;
    analyze_take(samples, rate, preroll_s, take.passage.as_ref())
}

/// Picks a line's take, finds its speech and places it in the film, or says
/// why it cannot (a refusal code and the reason for the message).
fn place(
    project: &Project,
    ledger: &Ledger,
    state: &State,
    use_proposed: bool,
    line: &Line,
) -> std::result::Result<Placed, (&'static str, &'static str)> {
    let rate = project.config.sample_rate;
    let line_state = state.lines.iter().find(|l| l.id == line.id);
    // `fallback`: no choice, so the proposed take stands in for one.
    let choice = line_state.and_then(|ls| match (&ls.chosen, ls.proposed) {
        (Some((take, by)), _) => Some((*take, by.clone(), false)),
        (None, Some(take)) if use_proposed => Some((take, "proposed".to_owned(), true)),
        _ => None,
    });
    let (take_no, by, fallback) = choice.ok_or(("no-choice", "lines without a chosen take"))?;
    let take = line_state
        .and_then(|ls| ls.takes.iter().find(|t| t.take == take_no))
        .ok_or(("missing-file", "chosen takes that were never recorded"))?;
    if fallback && take.effective == Some(Verdict::Retake) {
        return Err((
            "no-acceptable-take",
            "lines whose best take is marked retake; choose one explicitly or record another",
        ));
    }
    let (samples, take_rate) = read_take_wav(&project.dir.join(&take.file))
        .map_err(|_| ("missing-file", "take files that are missing or unreadable"))?;
    if take_rate != rate {
        return Err((
            "sample-rate",
            "takes not recorded at the project's sample rate",
        ));
    }
    let analysis = analyze_placed(project, &samples, rate, take);
    let (_, speech_end) = analysis
        .speech
        .ok_or(("no-speech", "chosen takes with no speech detected"))?;
    // The speech starts where the check measured it: after a breath set
    // apart from the words.
    let check = current_check(project, ledger, line, take_no);
    let speech_start = speech_onset(&analysis, check.and_then(|c| c.transcript.as_deref()))
        .ok_or(("no-speech", "chosen takes with no speech detected"))?;

    let film_len = seconds_to_samples(project.film_duration(), rate).max(0);
    let len = i64::try_from(samples.len()).unwrap_or(i64::MAX);
    let margin = seconds_to_samples(MARGIN_S, rate);
    let mut src0 = (seconds_to_samples(speech_start, rate) - margin).max(0);
    let mut src1 = (seconds_to_samples(speech_end, rate) + margin).min(len);
    let take_start_film = line.start + take.line_offset_s;
    let speech_film = (take_start_film + speech_start, take_start_film + speech_end);
    let duration = project.film_duration();
    if speech_film.1 <= 0.0 || speech_film.0 >= duration {
        return Err((
            "speech-outside-film",
            "chosen takes whose speech lies entirely outside the film",
        ));
    }
    let offset = seconds_to_samples(take_start_film, rate);
    let (mut dest0, mut dest1) = (offset + src0, offset + src1);
    if dest0 < 0 {
        src0 -= dest0;
        dest0 = 0;
    }
    if dest1 > film_len {
        src1 -= dest1 - film_len;
        dest1 = film_len;
    }
    if src1 <= src0 {
        return Err((
            "no-speech",
            "chosen takes whose speech lies outside the film",
        ));
    }
    let speech_dest = (
        (offset + seconds_to_samples(speech_start, rate)).clamp(0, film_len),
        (offset + seconds_to_samples(speech_end, rate)).clamp(0, film_len),
    );
    let clamped = (speech_film.0.max(0.0), speech_film.1.min(duration));
    let cropped_speech = (clamped != speech_film).then_some(speech_film);
    let (src0, src1, dest0, dest1) = (
        to_usize(src0),
        to_usize(src1),
        to_usize(dest0),
        to_usize(dest1),
    );
    Ok(Placed {
        line: ManifestLine {
            id: line.id.clone(),
            text: line.text.clone(),
            take_file: take.file.clone(),
            chosen_by: by,
            source_samples: [to_u64(src0), to_u64(src1)],
            dest_samples: [to_u64(dest0), to_u64(dest1)],
            speech_film_s: [clamped.0, clamped.1],
            words_film_s: check.and_then(|c| words_film_s(c, take_start_film)),
            noise_reduction: None,
        },
        samples: samples.iter().map(|&s| f64::from(s)).collect(),
        take: take_no,
        floor_dbfs: analysis.noise_floor_dbfs,
        src: src0,
        dest: (dest0, dest1),
        speech: (to_usize(speech_dest.0), to_usize(speech_dest.1)),
        fade_room: (usize::MAX, usize::MAX),
        old_text: take.recorded_against_old_text,
        cropped_speech,
        nr_skipped: None,
    })
}

/// The take's latest check against the line as it is now.
fn current_check<'a>(
    project: &Project,
    ledger: &'a Ledger,
    line: &Line,
    take: u32,
) -> Option<&'a Checked> {
    let current = project.snapshot(&line.id)?;
    ledger
        .events
        .iter()
        .rev()
        .filter(|e| e.line == line.id && e.take == take)
        .find_map(|e| match &e.kind {
            EventKind::Checked(checked) if checked.snapshot.matches(&current) => Some(checked),
            _ => None,
        })
}

/// The words of a check in film seconds; `None` without a transcript.
fn words_film_s(checked: &Checked, take_start_film: f64) -> Option<Vec<ManifestWord>> {
    let words = checked.transcript.as_ref()?;
    Some(
        words
            .iter()
            .map(|w| ManifestWord {
                text: w.text.clone(),
                start: take_start_film + w.start,
                end: take_start_film + w.end,
            })
            .collect(),
    )
}

fn warnings(placed: &[Placed], film_duration: f64) -> Vec<ExportWarning> {
    let mut warnings = Vec::new();
    for p in placed {
        if p.old_text {
            warnings.push(ExportWarning {
                code: "recorded-against-old-text",
                line: Some(p.line.id.clone()),
                message: format!(
                    "{} was recorded against different text than the line has now",
                    p.line.take_file
                ),
            });
        }
        if let Some((start, end)) = p.cropped_speech {
            warnings.push(ExportWarning {
                code: "speech-cropped",
                line: Some(p.line.id.clone()),
                message: format!(
                    "the speech of {} runs from {start:.3} s to {end:.3} s, partly outside the film (0 s to {film_duration:.3} s); the part outside is cropped",
                    p.line.take_file
                ),
            });
        }
        if let Some(why) = &p.nr_skipped {
            warnings.push(ExportWarning {
                code: "noise-reduction-skipped",
                line: Some(p.line.id.clone()),
                message: format!("{}: noise reduction skipped: {why}", p.line.take_file),
            });
        }
    }
    warnings
}

/// Where only the margins of two neighbouring takes overlap, splits the
/// gap between their speech at its midpoint: each keeps its margin up to
/// there. Takes whose speech overlaps are left for [`overlaps`]. Returns a
/// `tight-gap` warning for each pair whose speech is less than
/// [`TIGHT_GAP_S`] apart.
fn split_margins(placed: &mut [Placed], rate: u32) -> Vec<ExportWarning> {
    let tight_gap = to_usize(seconds_to_samples(TIGHT_GAP_S, rate));
    let mut warnings = Vec::new();
    let mut order: Vec<usize> = (0..placed.len()).collect();
    order.sort_by_key(|&i| placed[i].speech);
    for pair in order.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let (a_end, b_start) = (placed[a].speech.1, placed[b].speech.0);
        if a_end <= b_start && b_start - a_end < tight_gap {
            let ms = (b_start - a_end) as f64 * 1000.0 / f64::from(rate);
            warnings.push(ExportWarning {
                code: "tight-gap",
                line: Some(placed[a].line.id.clone()),
                message: format!(
                    "the speech of {} and {} is only {ms:.0} ms apart; the cut between them has shortened fades",
                    placed[a].line.id, placed[b].line.id
                ),
            });
        }
        if a_end > b_start || placed[a].dest.1 <= placed[b].dest.0 {
            continue;
        }
        let mid = a_end + (b_start - a_end) / 2;
        let first = &mut placed[a];
        first.dest.1 = first.dest.1.min(mid);
        first.fade_room.1 = first.dest.1 - a_end;
        let second = &mut placed[b];
        if second.dest.0 < mid {
            second.src += mid - second.dest.0;
            second.dest.0 = mid;
        }
        second.fade_room.0 = b_start - second.dest.0;
    }
    for p in placed {
        let len = p.dest.1 - p.dest.0;
        p.line.source_samples = [to_u64(p.src), to_u64(p.src + len)];
        p.line.dest_samples = [to_u64(p.dest.0), to_u64(p.dest.1)];
    }
    warnings
}

/// Pairs of takes whose speech (without the margins) overlaps in the film.
fn overlaps(placed: &[Placed]) -> Vec<Refusal> {
    let mut refused = Vec::new();
    for (i, a) in placed.iter().enumerate() {
        for b in placed.iter().skip(i + 1) {
            if a.speech.0 < b.speech.1 && b.speech.0 < a.speech.1 {
                refused.push(Refusal {
                    code: "overlap",
                    lines: vec![a.line.id.clone(), b.line.id.clone()],
                    message: format!(
                        "the speech of {} and {} overlaps in the film; choose another take or move a start",
                        a.line.id, b.line.id
                    ),
                });
            }
        }
    }
    refused
}

/// Copies each placed range into a silent film-length buffer with linear
/// fades at both ends, each as long as its room allows (none where a split
/// gap leaves no margin). In 24-bit units, unrounded.
fn mix(placed: &[Placed], film_len: usize, rate: u32) -> Vec<f64> {
    let mut out = vec![0.0; film_len];
    let fade = to_usize(seconds_to_samples(FADE_S, rate)).max(1);
    for p in placed {
        let len = p.dest.1 - p.dest.0;
        let ramp = |room: usize| fade.min(len / 2).max(1).min(room);
        let (fade_in, fade_out) = (ramp(p.fade_room.0), ramp(p.fade_room.1));
        let source = p.samples.iter().skip(p.src).take(len);
        for (k, (slot, &sample)) in out[p.dest.0..p.dest.1].iter_mut().zip(source).enumerate() {
            let to_end = len - 1 - k;
            *slot = if k < fade_in {
                sample * (k as f64 / fade_in as f64)
            } else if to_end < fade_out {
                sample * (to_end as f64 / fade_out as f64)
            } else {
                sample
            };
        }
    }
    out
}

/// Writes the manifest to a temp file next to `path` and returns its path.
fn write_manifest(path: &Path, manifest: &Manifest<'_>) -> Result<PathBuf> {
    let mut json = serde_json::to_vec_pretty(manifest).map_err(|err| Error::Parse {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    json.push(b'\n');
    write_temp(path, |file| file.write_all(&json))
}

fn describe_file(path: &Path, file: &str) -> std::io::Result<ManifestNarration> {
    let bytes = fs::read(path)?;
    let sha256 = Sha256::digest(&bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    Ok(ManifestNarration {
        file: file.to_owned(),
        size: to_u64(bytes.len()),
        sha256,
    })
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(io(path)(err)),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::edit::{Field, set_line_field};
    use crate::ledger::{self, Chosen, Event, EventKind};
    use crate::project::FilmInfo;
    use crate::state::fold;
    use crate::takes::{
        RecordedInput, TakeRef, begin_take, commit_take, read_take_wav, write_take_wav,
    };
    use crate::testutil::{RATE, burst, noise};
    use crate::{ExportConfig, ExportFormat, Processing};

    const FILM_SAMPLES: usize = 2_472_000; // 51.5 s

    fn setup(script: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("p.takewise");
        let info = FilmInfo {
            duration_s: 51.5,
            size: 1,
            modified: "2026-11-22T20:05:11Z".to_owned(),
        };
        Project::init(&dir, Path::new("film.mp4"), info).unwrap();
        fs::write(dir.join("script.toml"), script).unwrap();
        (tmp, dir)
    }

    /// Records a 5 s take with speech at `from..to` seconds into the take,
    /// captured from 2 s before the line's start.
    fn take(dir: &Path, line: &str, speech: Option<(f64, f64)>) -> TakeRef {
        take_with(dir, line, speech.as_slice())
    }

    /// [`take`] with a sound at each of `bursts`.
    fn take_with(dir: &Path, line: &str, bursts: &[(f64, f64)]) -> TakeRef {
        let project = Project::open(dir).unwrap();
        let input = RecordedInput {
            sample_rate: RATE,
            device: "Test Mic".to_owned(),
            channel: 1,
            line_offset_s: -2.0,
            snapshot: project.snapshot(line).unwrap(),
            dropouts: None,
            film_shift_ms: None,
            passage: None,
        };
        let partial = begin_take(dir, line, &input).unwrap();
        let mut audio = noise(5.0);
        for &(from, to) in bursts {
            burst(&mut audio, from, to);
        }
        write_take_wav(&partial.wav, &audio, RATE).unwrap();
        commit_take(dir, partial).unwrap()
    }

    /// Hears "Hello there." from `start` seconds into the take.
    struct HelloAt(f64);

    impl crate::whisper::Transcriber for HelloAt {
        fn transcribe(&self, _: &Path) -> crate::check::Transcript {
            let word = |text: &str, start: f64| crate::ledger::Word {
                text: text.to_owned(),
                start,
                end: start + 0.3,
                confidence: Some(0.9),
            };
            crate::check::Transcript::Words(vec![
                word("Hello", self.0),
                word("there.", self.0 + 0.35),
            ])
        }
        fn model(&self) -> String {
            "ggml-fake.bin".to_owned()
        }
    }

    fn choose(dir: &Path, take: &TakeRef) {
        let event = Event::new(
            &take.line,
            take.take,
            EventKind::Chosen(Chosen {
                by: "narrator".to_owned(),
            }),
        );
        ledger::append(dir, &event).unwrap();
    }

    /// The film as the project measured it.
    fn unchanged_film(dir: &Path) -> FilmOnDisk {
        let info = Project::open(dir).unwrap().config.film_info;
        FilmOnDisk::Found {
            size: info.size,
            modified: info.modified,
            duration_s: Some(info.duration_s),
        }
    }

    fn run(dir: &Path, use_proposed: bool) -> ExportReport {
        export(
            dir,
            &ExportOptions {
                use_proposed,
                ..ExportOptions::default()
            },
            &|| Ok(unchanged_film(dir)),
        )
        .unwrap()
    }

    fn narration(dir: &Path) -> Vec<i32> {
        let (samples, rate) = read_take_wav(&dir.join("export/narration.wav")).unwrap();
        assert_eq!(rate, RATE);
        samples
    }

    /// The first sample louder than a quarter of full scale.
    fn onset(samples: &[i32]) -> usize {
        samples
            .iter()
            .position(|s| s.unsigned_abs() > (1 << 21))
            .expect("a burst in the export")
    }

    fn refused_codes(report: &ExportReport) -> Vec<&'static str> {
        report.refused.iter().map(|r| r.code).collect()
    }

    const ONE_LINE: &str = "[[line]]\nid = \"hook\"\nstart = 10.0\ntext = \"Hello there.\"\n";

    #[test]
    fn export_places_sample_exactly() {
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        let report = run(&dir, false);
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        let at = onset(&narration(&dir));
        assert!(at.abs_diff(480_000) <= 480, "onset at {at}");

        let line = &report.lines[0];
        assert_eq!(line.id, "hook");
        assert_eq!(line.take_file, "takes/hook/001.wav");
        assert_eq!(line.chosen_by, "narrator");
        // Source: speech ± 150 ms; destination = (10.0 - 2.0) s + source.
        assert_eq!(line.source_samples, [96_000 - 7_200, 144_000 + 7_200]);
        assert_eq!(line.dest_samples, [384_000 + 88_800, 384_000 + 151_200]);
        assert!((line.speech_film_s[0] - 10.0).abs() < 0.02, "{line:?}");
        assert!((line.speech_film_s[1] - 11.0).abs() < 0.02, "{line:?}");

        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("export/manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["v"], 1);
        assert_eq!(manifest["sample_rate"], 48_000);
        assert_eq!(manifest["film"]["duration_s"], 51.5);
        assert!(
            manifest["film"]["path"]
                .as_str()
                .unwrap()
                .ends_with("film.mp4")
        );
        assert_eq!(manifest["lines"][0]["dest_samples"][0], 472_800);
    }

    #[test]
    fn export_includes_word_times() {
        struct Hears;
        impl crate::whisper::Transcriber for Hears {
            fn transcribe(&self, _: &Path) -> crate::check::Transcript {
                let word = |text: &str, start: f64, end: f64| crate::ledger::Word {
                    text: text.to_owned(),
                    start,
                    end,
                    confidence: Some(0.9),
                };
                crate::check::Transcript::Words(vec![
                    word("Hello", 2.1, 2.4),
                    // A sound annotation is not a word of the line.
                    word("(coughs)", 2.42, 2.48),
                    word("there.", 2.5, 2.9),
                ])
            }
            fn model(&self) -> String {
                "ggml-fake.bin".to_owned()
            }
        }
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        let manifest = |dir: &Path| -> serde_json::Value {
            serde_json::from_slice(&fs::read(dir.join("export/manifest.json")).unwrap()).unwrap()
        };

        // Unchecked: no word times.
        let report = run(&dir, false);
        assert_eq!(report.lines[0].words_film_s, None);
        assert!(manifest(&dir)["lines"][0].get("words_film_s").is_none());

        crate::run_check::check_take(&dir, &t, &Hears, "0.1.0").unwrap();
        let report = run(&dir, false);
        let words = report.lines[0].words_film_s.as_ref().expect("word times");
        let got: Vec<(&str, f64, f64)> = words
            .iter()
            .map(|w| (w.text.as_str(), w.start, w.end))
            .collect();
        // Film time = line start 10.0 + offset -2.0 + time in the take.
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].0, got[1].0), ("Hello", "there."));
        assert!((got[0].1 - 10.1).abs() < 1e-9 && (got[0].2 - 10.4).abs() < 1e-9);
        assert!((got[1].1 - 10.5).abs() < 1e-9 && (got[1].2 - 10.9).abs() < 1e-9);
        let json = &manifest(&dir)["lines"][0]["words_film_s"];
        assert_eq!(json[0]["text"], "Hello", "{json}");
        assert!((json[1]["start"].as_f64().unwrap() - 10.5).abs() < 1e-9);

        // Moving the line makes the check stale: no word times until rechecked.
        set_line_field(&dir, "hook", Field::Start(12.0), None).unwrap();
        let report = run(&dir, false);
        assert_eq!(report.lines[0].words_film_s, None);
    }

    #[test]
    fn export_follows_moved_line() {
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        set_line_field(&dir, "hook", Field::Start(12.0), None).unwrap();
        let report = run(&dir, false);
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        let at = onset(&narration(&dir));
        assert!(at.abs_diff(576_000) <= 480, "onset at {at}");
    }

    #[test]
    fn export_crops_before_film_zero() {
        let script = "[[line]]\nid = \"hook\"\nstart = 0.0\ntext = \"Hello.\"\n";
        let (_tmp, dir) = setup(script);
        // Speech starts 0.5 s before film zero and runs to 1.0 s.
        let t = take(&dir, "hook", Some((1.5, 3.0)));
        choose(&dir, &t);
        let report = run(&dir, false);
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        let line = &report.lines[0];
        assert_eq!(line.dest_samples[0], 0);
        assert_eq!(line.source_samples[0], 96_000);
        assert_eq!(line.dest_samples[1], 48_000 + 7_200);
        assert_eq!(line.speech_film_s[0], 0.0, "clamped to the film: {line:?}");
        assert!((line.speech_film_s[1] - 1.0).abs() < 0.02, "{line:?}");
        let audio = narration(&dir);
        assert_eq!(audio.len(), FILM_SAMPLES);
        assert!(onset(&audio) < 960, "speech is placed right from film zero");

        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        let warning = &report.warnings[0];
        assert_eq!(
            (warning.code, warning.line.as_deref().unwrap_or_default()),
            ("speech-cropped", "hook")
        );
        assert!(warning.message.contains("-0.500"), "{}", warning.message);
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("export/manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["warnings"][0]["code"], "speech-cropped");
        assert_eq!(manifest["lines"][0]["speech_film_s"][0], 0.0);
    }

    #[test]
    fn export_crops_past_film_end_with_a_warning() {
        let script = "[[line]]\nid = \"hook\"\nstart = 50.0\ntext = \"Bye.\"\n";
        let (_tmp, dir) = setup(script);
        // Speech runs from 50.0 s to 52.5 s film time; the film ends at 51.5 s.
        let t = take(&dir, "hook", Some((2.0, 4.5)));
        choose(&dir, &t);
        let report = run(&dir, false);
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        let line = &report.lines[0];
        assert_eq!(line.dest_samples[1], FILM_SAMPLES as u64);
        assert_eq!(line.speech_film_s[1], 51.5, "{line:?}");
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert_eq!(report.warnings[0].code, "speech-cropped");
        assert!(
            report.warnings[0].message.contains("52.500"),
            "{}",
            report.warnings[0].message
        );
    }

    #[test]
    fn export_refuses_speech_entirely_before_film_zero() {
        let script = "[[line]]\nid = \"hook\"\nstart = 0.0\ntext = \"Hello.\"\n";
        let (_tmp, dir) = setup(script);
        // Speech runs from -1.5 s to -0.1 s film time; only the 150 ms
        // margin would reach past film zero.
        let t = take(&dir, "hook", Some((0.5, 1.9)));
        choose(&dir, &t);
        let report = run(&dir, false);
        assert_eq!(refused_codes(&report), vec!["speech-outside-film"]);
        assert_eq!(report.refused[0].lines, vec!["hook".to_owned()]);
        assert!(!dir.join("export/narration.wav").exists());
    }

    #[test]
    fn export_refuses_overlap() {
        let script = "[[line]]\nid = \"one\"\nstart = 10.0\ntext = \"A.\"\n\n[[line]]\nid = \"two\"\nstart = 11.0\ntext = \"B.\"\n";
        let (_tmp, dir) = setup(script);
        let a = take(&dir, "one", Some((2.0, 4.0)));
        let b = take(&dir, "two", Some((2.0, 3.0)));
        choose(&dir, &a);
        choose(&dir, &b);
        let report = run(&dir, false);
        assert_eq!(refused_codes(&report), vec!["overlap"]);
        assert_eq!(
            report.refused[0].lines,
            vec!["one".to_owned(), "two".to_owned()]
        );
        assert_eq!(report.lines, []);
        assert!(!dir.join("export/narration.wav").exists());
    }

    #[test]
    fn a_breath_before_the_words_is_not_their_start() {
        // "one" speaks at 10..11.9 s of film. "two" breathes at 11.8..11.95
        // s, over the end of "one", then speaks from 12.2 s: its check
        // measures the start after the breath, and so does the export.
        let script = "[[line]]\nid = \"one\"\nstart = 10.0\ntext = \"A.\"\n\n[[line]]\nid = \"two\"\nstart = 12.0\ntext = \"Hello there.\"\n";
        let (_tmp, dir) = setup(script);
        let a = take(&dir, "one", Some((2.0, 3.9)));
        let b = take_with(&dir, "two", &[(1.8, 1.95), (2.2, 3.0)]);
        let checked = crate::run_check::check_take(&dir, &b, &HelloAt(2.25), "0.1.0").unwrap();
        assert_eq!(checked.verdict, Verdict::Keep, "{:?}", checked.reasons);
        choose(&dir, &a);
        choose(&dir, &b);
        let report = run(&dir, false);
        assert_eq!(refused_codes(&report), Vec::<&str>::new(), "{report:?}");
        let two = &report.lines[1];
        assert!((two.speech_film_s[0] - 12.2).abs() < 0.02, "{two:?}");
        // The breath is left out where the gap is split.
        assert!(two.dest_samples[0] > samples_at(11.95), "{two:?}");
    }

    fn samples_at(film_s: f64) -> u64 {
        u64::try_from(seconds_to_samples(film_s, RATE)).unwrap()
    }

    #[test]
    fn export_splits_overlapping_margins_between_neighbours() {
        // Speech at 10..11 s and 11.1..12.1 s of film: only the 150 ms
        // margins overlap, so the 0.1 s gap is split at 11.05 s.
        let script = "[[line]]\nid = \"one\"\nstart = 10.0\ntext = \"A.\"\n\n[[line]]\nid = \"two\"\nstart = 11.1\ntext = \"B.\"\n";
        let (_tmp, dir) = setup(script);
        let a = take(&dir, "one", Some((2.0, 3.0)));
        let b = take(&dir, "two", Some((2.0, 3.0)));
        choose(&dir, &a);
        choose(&dir, &b);
        let report = run(&dir, false);
        assert_eq!(refused_codes(&report), Vec::<&str>::new(), "{report:?}");
        let (one, two) = (&report.lines[0], &report.lines[1]);
        assert_eq!(one.dest_samples[1], two.dest_samples[0]);
        assert!(one.dest_samples[1].abs_diff(530_400) <= 480, "{one:?}");
        // Each keeps its margin on the other side and its source range
        // follows its destination.
        assert_eq!(one.dest_samples[0], 384_000 + 96_000 - 7_200);
        assert_eq!(
            one.source_samples[1] - one.source_samples[0],
            one.dest_samples[1] - one.dest_samples[0]
        );
        assert_eq!(two.dest_samples[0] - 436_800, two.source_samples[0]);
        assert_eq!(two.source_samples[1], 144_000 + 7_200);
    }

    #[test]
    fn a_tight_gap_gets_fades_that_spare_the_speech_and_a_warning() {
        // Speech at 10..11 s and 11.01..12.01 s of film: a 10 ms gap, too
        // short for two 10 ms fades outside the speech.
        let script = "[[line]]\nid = \"one\"\nstart = 10.0\ntext = \"A.\"\n\n[[line]]\nid = \"two\"\nstart = 11.01\ntext = \"B.\"\n";
        let (_tmp, dir) = setup(script);
        let a = take(&dir, "one", Some((2.0, 3.0)));
        let b = take(&dir, "two", Some((2.0, 3.0)));
        choose(&dir, &a);
        choose(&dir, &b);
        let report = run(&dir, false);
        assert_eq!(refused_codes(&report), Vec::<&str>::new(), "{report:?}");
        let tight: Vec<&ExportWarning> = report
            .warnings
            .iter()
            .filter(|w| w.code == "tight-gap")
            .collect();
        assert_eq!(tight.len(), 1, "{:?}", report.warnings);
        assert_eq!(tight[0].line.as_deref(), Some("one"));
        assert!(tight[0].message.contains("two"), "{tight:?}");

        // Within each take's speech the narration is the take, unfaded.
        let out = narration(&dir);
        for (line, t) in report.lines.iter().zip([&a, &b]) {
            let (source, _) =
                read_take_wav(&dir.join(format!("takes/{}/{:03}.wav", t.line, t.take))).unwrap();
            let [src0, _] = line.source_samples;
            let [dest0, _] = line.dest_samples;
            let from = samples_at(line.speech_film_s[0]) + 1;
            let to = samples_at(line.speech_film_s[1]) - 1;
            for d in from..to {
                let i = usize::try_from(d).unwrap();
                let s = usize::try_from(d - dest0 + src0).unwrap();
                assert_eq!(out[i], source[s], "{} at sample {d}", line.id);
            }
        }

        // A gap of 20 ms or more is no warning.
        let script = script.replace("11.01", "11.02");
        let (_tmp, dir) = setup(&script);
        let a = take(&dir, "one", Some((2.0, 3.0)));
        let b = take(&dir, "two", Some((2.0, 3.0)));
        choose(&dir, &a);
        choose(&dir, &b);
        let report = run(&dir, false);
        assert!(
            report.warnings.iter().all(|w| w.code != "tight-gap"),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn export_refuses_missing_choice() {
        let script = "[[line]]\nid = \"one\"\nstart = 10.0\ntext = \"A.\"\n\n[[line]]\nid = \"two\"\nstart = 20.0\ntext = \"B.\"\n\n[[line]]\nid = \"three\"\nstart = 30.0\ntext = \"C.\"\n";
        let (_tmp, dir) = setup(script);
        let a = take(&dir, "one", Some((2.0, 3.0)));
        take(&dir, "two", Some((2.0, 3.0)));
        choose(&dir, &a);
        let report = run(&dir, false);
        assert_eq!(refused_codes(&report), vec!["no-choice"]);
        assert_eq!(
            report.refused[0].lines,
            vec!["two".to_owned(), "three".to_owned()]
        );
        assert!(!dir.join("export").join("narration.wav").exists());

        // Proposed takes fill in, but a line with no takes still fails.
        let report = run(&dir, true);
        assert_eq!(refused_codes(&report), vec!["no-choice"]);
        assert_eq!(report.refused[0].lines, vec!["three".to_owned()]);
    }

    #[test]
    fn export_use_proposed() {
        let (_tmp, dir) = setup(ONE_LINE);
        take(&dir, "hook", Some((2.0, 3.0)));
        take(&dir, "hook", Some((2.5, 3.0)));
        let report = run(&dir, true);
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        assert_eq!(report.lines[0].chosen_by, "proposed");
        assert_eq!(report.lines[0].take_file, "takes/hook/002.wav");
        let at = onset(&narration(&dir));
        assert!(at.abs_diff(504_000) <= 480, "onset at {at}");
    }

    #[test]
    fn use_proposed_refuses_a_line_whose_best_take_is_retake() {
        let (_tmp, dir) = setup(ONE_LINE);
        for _ in 0..2 {
            let t = take(&dir, "hook", Some((2.0, 3.0)));
            let judgment = crate::takes::Judgment {
                by: "claude".to_owned(),
                verdict: crate::ledger::Verdict::Retake,
                score: None,
                reason: "rushed".to_owned(),
            };
            crate::takes::judge_take(&dir, "hook", t.take, judgment).unwrap();
        }
        let project = Project::open(&dir).unwrap();
        let state = fold(&project, &ledger::read(&dir).unwrap());
        assert_eq!(
            state.lines[0].proposed,
            Some(2),
            "still proposed for display"
        );
        let report = run(&dir, true);
        assert_eq!(refused_codes(&report), vec!["no-acceptable-take"]);
        assert_eq!(report.refused[0].lines, vec!["hook".to_owned()]);
        assert!(!dir.join("export/narration.wav").exists());
    }

    #[test]
    fn explicit_choice_by_proposed_is_exported_like_any_choice() {
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        let judgment = crate::takes::Judgment {
            by: "claude".to_owned(),
            verdict: crate::ledger::Verdict::Retake,
            score: None,
            reason: "rushed".to_owned(),
        };
        crate::takes::judge_take(&dir, "hook", t.take, judgment).unwrap();
        crate::takes::choose_take(&dir, "hook", t.take, "proposed").unwrap();
        for use_proposed in [false, true] {
            let report = run(&dir, use_proposed);
            assert_eq!(report.refused, [], "use_proposed = {use_proposed}");
            assert_eq!(report.lines[0].chosen_by, "proposed");
        }
    }

    #[test]
    fn take_recorded_against_old_text_is_exported_with_a_warning() {
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        set_line_field(&dir, "hook", Field::Text("Hi there.".to_owned()), None).unwrap();
        let report = run(&dir, false);
        assert!(report.refused.is_empty(), "{:?}", report.refused);
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert_eq!(report.warnings[0].code, "recorded-against-old-text");
        assert_eq!(report.warnings[0].line.as_deref(), Some("hook"));
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("export/manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["warnings"][0]["code"], "recorded-against-old-text");
        assert_eq!(manifest["warnings"][0]["line"], "hook");

        // A take recorded against the current text carries no warning.
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        assert_eq!(run(&dir, false).warnings, []);
    }

    #[test]
    fn export_refuses_silent_take() {
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", None);
        choose(&dir, &t);
        let report = run(&dir, false);
        assert_eq!(refused_codes(&report), vec!["no-speech"]);
        assert_eq!(report.refused[0].lines, vec!["hook".to_owned()]);
        assert!(!dir.join("export/narration.wav").exists());
    }

    #[test]
    fn export_refuses_missing_file() {
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        // Simulate a take file lost outside Takewise.
        fs::rename(
            dir.join("takes/hook/001.wav"),
            dir.join("takes/hook/moved.wav"),
        )
        .unwrap();
        let report = run(&dir, false);
        assert_eq!(refused_codes(&report), vec!["missing-file"]);
    }

    #[test]
    fn manifest_records_narration_size_and_hash() {
        use sha2::{Digest as _, Sha256};
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        assert_eq!(run(&dir, false).refused, []);
        let wav = fs::read(dir.join("export/narration.wav")).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("export/manifest.json")).unwrap()).unwrap();
        let narration = &manifest["narration"];
        assert_eq!(narration["file"], "narration.wav");
        assert_eq!(narration["size"], wav.len());
        let hash = Sha256::digest(&wav)
            .iter()
            .fold(String::new(), |mut hex, b| {
                std::fmt::Write::write_fmt(&mut hex, format_args!("{b:02x}")).unwrap();
                hex
            });
        assert_eq!(narration["sha256"], hash);
        let leftovers: Vec<_> = fs::read_dir(dir.join("export"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 2, "no temp files left: {leftovers:?}");
    }

    #[test]
    fn refusal_after_export_removes_the_manifest() {
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        assert_eq!(run(&dir, false).refused, []);
        assert!(dir.join("export/manifest.json").exists());

        // A silent second take is chosen: export now refuses.
        let silent = take(&dir, "hook", None);
        choose(&dir, &silent);
        assert_eq!(refused_codes(&run(&dir, false)), vec!["no-speech"]);
        assert!(
            !dir.join("export/manifest.json").exists(),
            "no stale manifest stays behind to vouch for the old WAV"
        );
        assert!(dir.join("takes/hook/001.wav").exists(), "takes are kept");
    }

    #[test]
    fn interrupted_export_leaves_no_old_manifest() {
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        assert_eq!(run(&dir, false).refused, []);
        assert!(dir.join("export/manifest.json").exists());

        // Renaming the new WAV into place fails: a non-empty directory sits
        // where the WAV goes.
        let wav = dir.join("export/narration.wav");
        fs::remove_file(&wav).unwrap();
        fs::create_dir(&wav).unwrap();
        fs::write(wav.join("keep"), b"x").unwrap();
        let result = export(&dir, &ExportOptions::default(), &|| {
            Ok(unchanged_film(&dir))
        });
        assert!(result.is_err(), "{result:?}");
        assert!(
            !dir.join("export/manifest.json").exists(),
            "the old manifest is gone before the new WAV is moved into place"
        );
    }

    #[test]
    fn export_waits_for_the_project_lock() {
        use std::sync::mpsc;
        use std::time::Duration;
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        let guard = crate::fsutil::lock(&dir).unwrap();
        let (tx, rx) = mpsc::channel();
        let other = dir.clone();
        let handle = std::thread::spawn(move || {
            let report = run(&other, false);
            tx.send(report).unwrap();
        });
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "export must wait for the lock"
        );
        drop(guard);
        let report = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(report.refused, []);
        handle.join().unwrap();
    }

    #[test]
    fn export_reads_state_under_the_lock() {
        use std::sync::mpsc;
        use std::time::Duration;
        let (_tmp, dir) = setup(ONE_LINE);
        let first = take(&dir, "hook", Some((2.0, 3.0)));
        let second = take(&dir, "hook", Some((2.5, 3.0)));
        choose(&dir, &first);
        // An outside fold sees take 1 chosen.
        let project = Project::open(&dir).unwrap();
        let before = fold(&project, &ledger::read(&dir).unwrap());
        assert_eq!(before.lines[0].chosen, Some((1, "narrator".to_owned())));

        // Another writer holds the lock while export starts, and chooses
        // take 2 before releasing it.
        let guard = crate::fsutil::lock(&dir).unwrap();
        let (tx, rx) = mpsc::channel();
        let other = dir.clone();
        let handle = std::thread::spawn(move || tx.send(run(&other, false)).unwrap());
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        let event = Event::new(
            "hook",
            second.take,
            EventKind::Chosen(Chosen {
                by: "claude".to_owned(),
            }),
        );
        ledger::append_locked(&dir, &event).unwrap();
        drop(guard);

        let report = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        handle.join().unwrap();
        assert_eq!(report.refused, []);
        assert_eq!(report.lines[0].take_file, "takes/hook/002.wav");
        assert_eq!(report.lines[0].chosen_by, "claude");
    }

    #[test]
    fn export_refuses_a_missing_or_changed_film() {
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        let opts = ExportOptions::default();
        let refused = |film: FilmOnDisk| {
            let report = export(&dir, &opts, &|| Ok(film.clone())).unwrap();
            assert!(!dir.join("export/narration.wav").exists());
            refused_codes(&report)
        };
        assert_eq!(refused(FilmOnDisk::Missing), vec!["film-missing"]);
        let FilmOnDisk::Found {
            size,
            modified,
            duration_s,
        } = unchanged_film(&dir)
        else {
            unreachable!()
        };
        let found = |size: u64, modified: &str, duration_s: Option<f64>| FilmOnDisk::Found {
            size,
            modified: modified.to_owned(),
            duration_s,
        };
        assert_eq!(
            refused(found(size + 1, &modified, duration_s)),
            vec!["film-changed"]
        );
        assert_eq!(
            refused(found(size, "2026-11-23T08:00:00Z", duration_s)),
            vec!["film-changed"]
        );
        assert_eq!(
            refused(found(size, &modified, Some(52.0))),
            vec!["film-changed"]
        );
        // Without ffprobe the duration is unknown: size and time decide.
        let report = export(&dir, &opts, &|| Ok(found(size, &modified, None))).unwrap();
        assert_eq!(report.refused, []);
        // And a refusal afterwards removes the manifest.
        assert_eq!(
            refused_codes(&export(&dir, &opts, &|| Ok(FilmOnDisk::Missing)).unwrap()),
            vec!["film-missing"]
        );
        assert!(!dir.join("export/manifest.json").exists());
    }

    fn choose_by(dir: &Path, take: u32, by: &str) {
        let event = Event::new(
            "hook",
            take,
            EventKind::Chosen(Chosen { by: by.to_owned() }),
        );
        ledger::append(dir, &event).unwrap();
    }

    #[test]
    fn a_change_while_rendering_renders_again() {
        let (_tmp, dir) = setup(ONE_LINE);
        let first = take(&dir, "hook", Some((2.0, 3.0)));
        let second = take(&dir, "hook", Some((2.5, 3.0)));
        choose(&dir, &first);
        let mut renders = 0;
        // The lock is free while rendering: `append` takes it.
        let report = export_observed(
            &dir,
            &ExportOptions::default(),
            &|| Ok(unchanged_film(&dir)),
            &mut |stage| {
                if stage != Stage::Rendered {
                    return;
                }
                renders += 1;
                if renders == 1 {
                    choose_by(&dir, second.take, "claude");
                }
            },
        )
        .unwrap();
        assert_eq!(renders, 2);
        assert_eq!(report.refused, []);
        assert_eq!(report.lines[0].take_file, "takes/hook/002.wav");
        let m: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("export/manifest.json")).unwrap()).unwrap();
        assert_eq!(m["lines"][0]["chosen_by"], "claude");
    }

    #[test]
    fn a_state_that_keeps_changing_refuses_export() {
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        assert_eq!(run(&dir, false).refused, []);
        let mut renders = 0;
        let report = export_observed(
            &dir,
            &ExportOptions::default(),
            &|| Ok(unchanged_film(&dir)),
            &mut |stage| {
                if stage != Stage::Rendered {
                    return;
                }
                renders += 1;
                choose_by(&dir, t.take, &format!("agent-{renders}"));
            },
        )
        .unwrap();
        assert_eq!(renders, 2, "one render, then one more");
        assert_eq!(refused_codes(&report), vec!["state-changed"]);
        assert!(!dir.join("export/manifest.json").exists());
        let leftovers: Vec<_> = fs::read_dir(dir.join("export"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert_eq!(leftovers, Vec::<std::ffi::OsString>::new());
    }

    #[test]
    fn a_film_that_changes_while_rendering_refuses_export() {
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        assert_eq!(run(&dir, false).refused, []);
        let start = unchanged_film(&dir);
        let FilmOnDisk::Found { size, .. } = start.clone() else {
            unreachable!()
        };
        let probes = std::cell::Cell::new(0);
        for (later, code) in [
            (FilmOnDisk::Missing, "film-missing"),
            (FilmOnDisk::ProbeTimedOut, "film-probe-timeout"),
            (
                FilmOnDisk::Found {
                    size: size + 1,
                    modified: "2026-11-23T08:00:00Z".to_owned(),
                    duration_s: Some(52.0),
                },
                "film-changed",
            ),
        ] {
            probes.set(0);
            let film = || {
                probes.set(probes.get() + 1);
                Ok(if probes.get() == 1 {
                    start.clone()
                } else {
                    later.clone()
                })
            };
            let report =
                export_observed(&dir, &ExportOptions::default(), &film, &mut |_| {}).unwrap();
            assert_eq!(refused_codes(&report), vec![code]);
            assert_eq!(probes.get(), 2, "probed at the start and before publishing");
            assert!(!dir.join("export/manifest.json").exists());
        }
    }

    #[test]
    fn takes_load_without_the_lock() {
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        let mut stages = Vec::new();
        let report = export_observed(
            &dir,
            &ExportOptions::default(),
            &|| Ok(unchanged_film(&dir)),
            &mut |stage| {
                let file = fs::File::open(dir.join(crate::fsutil::LOCK_FILE)).unwrap();
                assert!(file.try_lock().is_ok(), "the lock is free at {stage:?}");
                stages.push(stage);
            },
        )
        .unwrap();
        assert_eq!(report.refused, []);
        assert_eq!(stages, [Stage::Planned, Stage::Rendered]);
    }

    #[test]
    fn export_length_equals_film() {
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        run(&dir, false);
        let reader = hound::WavReader::open(dir.join("export/narration.wav")).unwrap();
        let spec = reader.spec();
        assert_eq!(
            (spec.channels, spec.bits_per_sample, spec.sample_rate),
            (1, 24, RATE)
        );
        assert_eq!(reader.duration() as usize, FILM_SAMPLES);
        // One front-centre channel: plain mono, not a left channel alone.
        let header = fs::read(dir.join("export/narration.wav")).unwrap();
        assert_eq!(
            crate::wav::channel_mask(&header),
            Some(crate::wav::FRONT_CENTER)
        );
        // Outside the placed range the export is silent.
        let audio = narration(&dir);
        assert_eq!(audio[472_799], 0);
        assert_eq!(audio[535_200], 0);
        // 10 ms fades: the first placed sample is silent and the level ramps up.
        assert_eq!(audio[472_800], 0);
    }

    fn export_with(dir: &Path, config: ExportConfig) -> ExportReport {
        crate::edit::set_export(dir, &config).unwrap();
        let report = run(dir, false);
        assert_eq!(report.refused, [], "{report:?}");
        report
    }

    fn manifest(dir: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(dir.join("export/manifest.json")).unwrap()).unwrap()
    }

    fn export_files(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir.join("export"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Decodes an export file at its own rate, as float samples.
    fn decode(ffmpeg: &Ffmpeg, path: &Path) -> (Vec<f64>, u32) {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("decoded.wav");
        ffmpeg
            .run(&[
                std::ffi::OsStr::new("-loglevel"),
                std::ffi::OsStr::new("error"),
                std::ffi::OsStr::new("-i"),
                path.as_os_str(),
                std::ffi::OsStr::new("-c:a"),
                std::ffi::OsStr::new("pcm_f32le"),
                out.as_os_str(),
            ])
            .unwrap();
        read_float_wav(&out).unwrap()
    }

    fn peak_at(x: &[f64]) -> usize {
        x.iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, _)| i)
            .unwrap()
    }

    /// A take whose speech is a quiet tone with one loud click at 2.5 s of
    /// the take (sample 120 000), so the click is the peak.
    fn click_take(dir: &Path) -> TakeRef {
        let project = Project::open(dir).unwrap();
        let input = RecordedInput {
            sample_rate: RATE,
            device: "Test Mic".to_owned(),
            channel: 1,
            line_offset_s: -2.0,
            snapshot: project.snapshot("hook").unwrap(),
            dropouts: None,
            film_shift_ms: None,
            passage: None,
        };
        let partial = begin_take(dir, "hook", &input).unwrap();
        let mut audio = noise(5.0);
        crate::testutil::burst_at(&mut audio, 2.0, 3.0, -30.0);
        audio[120_000] = 6 << 20;
        write_take_wav(&partial.wav, &audio, RATE).unwrap();
        commit_take(dir, partial).unwrap()
    }

    /// A take whose speech, from 2 to 4 s of the take, is a 140 Hz voice
    /// with syllables at 4 Hz at −26 dBFS RMS and four plosive-like bursts
    /// whose peaks stand `crest_db` above that RMS.
    fn peaky_take(dir: &Path, crest_db: f64) -> TakeRef {
        let project = Project::open(dir).unwrap();
        let input = RecordedInput {
            sample_rate: RATE,
            device: "Test Mic".to_owned(),
            channel: 1,
            line_offset_s: -2.0,
            snapshot: project.snapshot("hook").unwrap(),
            dropouts: None,
            film_shift_ms: None,
            passage: None,
        };
        let partial = begin_take(dir, "hook", &input).unwrap();
        let tau = std::f64::consts::TAU;
        let rate = f64::from(RATE);
        let (from, len): (usize, usize) = (96_000, 96_000);
        let mut voice: Vec<f64> = (0..len)
            .map(|i| {
                let t = i as f64 / rate;
                let syllables = 0.5 - 0.5 * (t * 4.0 * tau).cos();
                let tone: f64 = (1..=10)
                    .map(|k| (t * 140.0 * f64::from(k) * tau).sin() / f64::from(k))
                    .sum();
                syllables * tone
            })
            .collect();
        let rms = (voice.iter().map(|v| v * v).sum::<f64>() / len as f64).sqrt();
        let level = 10f64.powf(-26.0 / 20.0);
        for v in &mut voice {
            *v *= level / rms;
        }
        let peak = level * 10f64.powf(crest_db / 20.0);
        let burst = 192; // 4 ms
        for at in [0.3, 0.8, 1.3, 1.7] {
            let start = crate::testutil::samples(at);
            for i in 0..burst {
                let window = (std::f64::consts::PI * i as f64 / burst as f64).sin();
                voice[start + i] = peak * window * (i as f64 / rate * 2_000.0 * tau).sin();
            }
        }
        let mut audio = noise(5.0);
        for (i, v) in voice.into_iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let s = (v * f64::from(crate::takes::FULL_SCALE)).round() as i32;
            audio[from + i] = s;
        }
        write_take_wav(&partial.wav, &audio, RATE).unwrap();
        commit_take(dir, partial).unwrap()
    }

    #[test]
    fn clean_brings_peaky_speech_to_minus_16_lufs() {
        let Some(ffmpeg) = ffmpeg_or_skip() else {
            return;
        };
        let (_tmp, dir) = setup(ONE_LINE);
        let t = peaky_take(&dir, 16.0);
        choose(&dir, &t);
        // Raw, the peaks stand about 16 dB above the loudness: one static
        // gain capped by the true peak could not reach −16 LUFS.
        export_with(&dir, ExportConfig::default());
        let raw = render::measure(&ffmpeg, &dir.join("export/narration.wav")).unwrap();
        assert!(raw.true_peak_dbtp - raw.integrated_lufs >= 15.5, "{raw:?}");
        for (rate, bits) in [(48_000, 24), (44_100, 16)] {
            let report = export_with(
                &dir,
                ExportConfig {
                    processing: Processing::Clean,
                    sample_rate: rate,
                    bit_depth: bits,
                    ..ExportConfig::default()
                },
            );
            let measured = render::measure(&ffmpeg, &dir.join("export/narration.wav")).unwrap();
            assert!(
                (measured.integrated_lufs + 16.0).abs() <= 0.5,
                "{rate}/{bits}: {measured:?}"
            );
            assert!(
                measured.true_peak_dbtp <= -1.0,
                "{rate}/{bits}: {measured:?}"
            );
            let settings = report.settings.unwrap();
            assert_eq!(settings.loudness_target_reached, Some(true));
            let reduction = settings.limiter_max_reduction_db.unwrap();
            assert!(reduction > 0.5 && reduction <= 6.0, "{reduction}");
            let m = manifest(&dir);
            assert_eq!(m["loudness_target_reached"], true);
            assert_eq!(m["limiter_max_reduction_db"], reduction);
        }
    }

    fn ffmpeg_or_skip() -> Option<Ffmpeg> {
        let found = Ffmpeg::find();
        if found.is_none() {
            eprintln!("skipped: ffmpeg not found");
        }
        found
    }

    #[test]
    fn raw_default_unchanged() {
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        let report = run(&dir, false);
        assert_eq!(report.narration, "narration.wav");
        let m = manifest(&dir);
        assert_eq!(m["processing"], "raw");
        assert_eq!(m["format"], "wav");
        assert_eq!(m["sample_rate"], 48_000);
        assert_eq!(m["source_sample_rate"], 48_000);
        assert_eq!(m["bit_depth"], 24);
        assert!(m.get("loudness_lufs").is_none(), "{m}");
        assert!(m["lines"][0].get("noise_reduction").is_none(), "{m}");
        // Between the fades, the take's samples are copied bit for bit.
        let (take_audio, _) = read_take_wav(&dir.join("takes/hook/001.wav")).unwrap();
        let out = narration(&dir);
        let [src0, _] = report.lines[0].source_samples;
        let [dest0, dest1] = report.lines[0].dest_samples;
        let (src0, dest0, dest1) = (to_usize_u(src0), to_usize_u(dest0), to_usize_u(dest1));
        for k in 480..(dest1 - dest0 - 480) {
            assert_eq!(out[dest0 + k], take_audio[src0 + k], "sample {k}");
        }
        assert_eq!(export_files(&dir), ["manifest.json", "narration.wav"]);
    }

    fn to_usize_u(n: u64) -> usize {
        usize::try_from(n).unwrap()
    }

    #[test]
    fn raw_wav_needs_no_ffmpeg() {
        let raw = ExportConfig::default();
        assert_eq!(ffmpeg_for(raw).unwrap(), None);
        // 16 bits are dithered in Rust: still no ffmpeg.
        let sixteen = ExportConfig {
            bit_depth: 16,
            ..raw
        };
        assert_eq!(ffmpeg_for(sixteen).unwrap(), None);
        for needs in [
            ExportConfig {
                processing: Processing::Clean,
                ..raw
            },
            ExportConfig {
                format: ExportFormat::Flac,
                ..raw
            },
            ExportConfig {
                sample_rate: 44_100,
                ..raw
            },
        ] {
            match ffmpeg_for(needs) {
                Ok(found) => assert!(found.is_some()),
                Err(err) => assert_eq!(err.code(), "missing-tool"),
            }
        }
        // And the raw export itself runs without it.
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        let report = export_with(&dir, sixteen);
        assert_eq!(report.settings.unwrap().bit_depth, 16);
        let spec = hound::WavReader::open(dir.join("export/narration.wav"))
            .unwrap()
            .spec();
        assert_eq!((spec.bits_per_sample, spec.sample_rate), (16, 48_000));
        // 16-bit uses the plain PCM header, which carries no channel mask.
        let header = fs::read(dir.join("export/narration.wav")).unwrap();
        assert_eq!(&header[20..22], &1_u16.to_le_bytes());
    }

    #[test]
    fn clean_hits_minus_16_lufs_within_half_lu() {
        let Some(ffmpeg) = ffmpeg_or_skip() else {
            return;
        };
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        let report = export_with(
            &dir,
            ExportConfig {
                processing: Processing::Clean,
                ..ExportConfig::default()
            },
        );
        let settings = report.settings.unwrap();
        let measured = render::measure(&ffmpeg, &dir.join("export/narration.wav")).unwrap();
        assert!(
            (measured.integrated_lufs + 16.0).abs() <= 0.5,
            "{measured:?}"
        );
        assert!(measured.true_peak_dbtp <= -1.0, "{measured:?}");
        assert_eq!(settings.loudness_lufs, Some(measured.integrated_lufs));
        assert_eq!(settings.true_peak_dbtp, Some(measured.true_peak_dbtp));
        let m = manifest(&dir);
        assert_eq!(m["processing"], "clean");
        assert_eq!(m["loudness_lufs"], measured.integrated_lufs);
        // No room tone was recorded: noise reduction was skipped, and why,
        // also as a warning.
        let note = m["lines"][0]["noise_reduction"].as_str().unwrap();
        assert!(note.starts_with("skipped: no room tone"), "{note}");
        let skipped: Vec<&ExportWarning> = report
            .warnings
            .iter()
            .filter(|w| w.code == "noise-reduction-skipped")
            .collect();
        assert_eq!(skipped.len(), 1, "{:?}", report.warnings);
        assert_eq!(skipped[0].line.as_deref(), Some("hook"));
        assert!(skipped[0].message.contains("no room tone"), "{skipped:?}");
        assert_eq!(m["warnings"][0]["code"], "noise-reduction-skipped");
    }

    #[test]
    fn true_peak_never_above_minus_one() {
        let Some(ffmpeg) = ffmpeg_or_skip() else {
            return;
        };
        let (_tmp, dir) = setup(ONE_LINE);
        let t = click_take(&dir);
        choose(&dir, &t);
        for (rate, bits) in [(48_000, 24), (44_100, 16)] {
            let report = export_with(
                &dir,
                ExportConfig {
                    processing: Processing::Clean,
                    sample_rate: rate,
                    bit_depth: bits,
                    ..ExportConfig::default()
                },
            );
            let measured = render::measure(&ffmpeg, &dir.join("export/narration.wav")).unwrap();
            // The click would need more than 6 dB of limiting: the limiter
            // stops at 6 dB, true peak stays at −1 dBTP at most, and the
            // speech stays below −16 LUFS, recorded as a shortfall.
            assert!(
                measured.true_peak_dbtp <= -1.0,
                "{rate}/{bits}: {measured:?}"
            );
            assert!(
                measured.integrated_lufs < -16.5,
                "{rate}/{bits}: {measured:?}"
            );
            let settings = report.settings.unwrap();
            assert_eq!(settings.true_peak_dbtp, Some(measured.true_peak_dbtp));
            assert_eq!(settings.loudness_target_reached, Some(false));
            let reduction = settings.limiter_max_reduction_db.unwrap();
            assert!(reduction > 5.0 && reduction <= 6.0, "{reduction}");
            assert_eq!(manifest(&dir)["loudness_target_reached"], false);
            let missed = report
                .warnings
                .iter()
                .find(|w| w.code == "loudness-target-missed")
                .expect("a loudness warning");
            assert_eq!(missed.line, None);
            assert!(missed.message.contains("LUFS"), "{missed:?}");
            let listed = manifest(&dir)["warnings"].clone();
            assert!(
                listed
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|w| w["code"] == "loudness-target-missed" && w.get("line").is_none()),
                "{listed}"
            );
        }
    }

    #[test]
    fn clean_places_a_click_where_raw_does() {
        let Some(ffmpeg) = ffmpeg_or_skip() else {
            return;
        };
        let (_tmp, dir) = setup(ONE_LINE);
        let t = click_take(&dir);
        choose(&dir, &t);
        // The take starts 8 s into the film: the click belongs at 504 000.
        let raw = narration(&dir_after(&dir, ExportConfig::default()));
        let raw_at = peak_at(&raw.iter().map(|&s| f64::from(s)).collect::<Vec<_>>());
        assert_eq!(raw_at, 504_000);
        // With a room tone, so noise reduction (and its delay) is in play.
        let tone = crate::room::RoomInput {
            input: "Test Mic",
            channel: 1,
            output: "Phones",
            floor_dbfs: -65.0,
            gated: false,
        };
        crate::room::save_room_tone(&dir, &noise(5.0), RATE, jiff::Timestamp::now(), &tone)
            .unwrap();
        let report = export_with(
            &dir,
            ExportConfig {
                processing: Processing::Clean,
                ..ExportConfig::default()
            },
        );
        let note = manifest(&dir)["lines"][0]["noise_reduction"].clone();
        assert!(note.as_str().unwrap().starts_with("afftdn 10 dB"), "{note}");
        assert!(
            report
                .warnings
                .iter()
                .all(|w| w.code != "noise-reduction-skipped"),
            "{:?}",
            report.warnings
        );
        let (clean, _) = decode(&ffmpeg, &dir.join("export/narration.wav"));
        assert!(
            peak_at(&clean).abs_diff(504_000) <= 1,
            "{}",
            peak_at(&clean)
        );
        // At 44.1 kHz the click is at round(504 000 × 44 100 / 48 000).
        export_with(
            &dir,
            ExportConfig {
                processing: Processing::Clean,
                sample_rate: 44_100,
                ..ExportConfig::default()
            },
        );
        let (resampled, rate) = decode(&ffmpeg, &dir.join("export/narration.wav"));
        assert_eq!(rate, 44_100);
        assert!(
            peak_at(&resampled).abs_diff(463_050) <= 1,
            "{}",
            peak_at(&resampled)
        );
    }

    /// Exports with `config` and returns `dir`.
    fn dir_after(dir: &Path, config: ExportConfig) -> PathBuf {
        export_with(dir, config);
        dir.to_path_buf()
    }

    #[test]
    fn export_44k_16bit_flac_ranges_and_length() {
        let Some(ffmpeg) = ffmpeg_or_skip() else {
            return;
        };
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        let at48 = run(&dir, false).lines[0].dest_samples;
        let report = export_with(
            &dir,
            ExportConfig {
                format: ExportFormat::Flac,
                bit_depth: 16,
                sample_rate: 44_100,
                processing: Processing::Clean,
            },
        );
        assert_eq!(report.narration, "narration.flac");
        assert_eq!(
            report.lines[0].dest_samples,
            at48.map(|n| render::at_rate(n, 44_100))
        );
        let m = manifest(&dir);
        assert_eq!(m["sample_rate"], 44_100);
        assert_eq!(m["source_sample_rate"], 48_000);
        assert_eq!(m["bit_depth"], 16);
        assert_eq!(m["format"], "flac");
        assert_eq!(m["narration"]["file"], "narration.flac");
        // Source ranges stay in take samples at 48 kHz.
        assert_eq!(m["lines"][0]["source_samples"][0], 96_000 - 7_200);
        let path = dir.join("export/narration.flac");
        let (decoded, rate) = decode(&ffmpeg, &path);
        assert_eq!(rate, 44_100);
        assert_eq!(decoded.len(), 2_271_150);
        let measured = render::measure(&ffmpeg, &path).unwrap();
        assert!(measured.true_peak_dbtp <= -1.0, "{measured:?}");
        let probe = std::process::Command::new(ffmpeg.path.with_file_name("ffprobe"))
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=sample_fmt,channel_layout",
                "-of",
                "csv=p=0",
            ])
            .arg(&path)
            .output();
        if let Ok(probe) = probe {
            assert_eq!(String::from_utf8_lossy(&probe.stdout).trim(), "s16,mono");
        }
    }

    #[test]
    fn format_switch_leaves_one_file() {
        if ffmpeg_or_skip().is_none() {
            return;
        }
        let (_tmp, dir) = setup(ONE_LINE);
        let t = take(&dir, "hook", Some((2.0, 3.0)));
        choose(&dir, &t);
        run(&dir, false);
        assert_eq!(export_files(&dir), ["manifest.json", "narration.wav"]);
        let flac = ExportConfig {
            format: ExportFormat::Flac,
            ..ExportConfig::default()
        };
        export_with(&dir, flac);
        assert_eq!(export_files(&dir), ["manifest.json", "narration.flac"]);
        let m = manifest(&dir);
        assert_eq!(m["narration"]["file"], "narration.flac");
        let bytes = fs::read(dir.join("export/narration.flac")).unwrap();
        assert_eq!(m["narration"]["size"], bytes.len());
        export_with(&dir, ExportConfig::default());
        assert_eq!(export_files(&dir), ["manifest.json", "narration.wav"]);
        assert_eq!(manifest(&dir)["narration"]["file"], "narration.wav");
    }
}
