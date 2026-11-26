//! The command-line interface: argument parsing and one handler per command.

use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::json;
use takewise_core::{
    self as tw, AvailableInput, CalibrationInfo, CheckConfig, ExportOptions, Field, FilmInfo,
    Judgment, LineState, Problem, Project, RepairReport, Route, ScriptVersion, State, Verdict,
    check_input,
};
use takewise_media::{DeviceInfo, Transport};

use crate::calibrate;
use crate::film;
use crate::output::Outcome;

/// Record voiceover narration over a finished film. Every command takes the
/// project directory and prints one JSON object with --json.
/// Exit codes: 0 success, 1 a finding, 2 usage or setup error.
#[derive(Debug, Parser)]
#[command(name = "takewise", version, about, long_about = None)]
pub struct Cli {
    /// Print one JSON object on stdout: {"ok":true,"data":…} or {"ok":false,"error":{"code","message"}}.
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a project with an empty script and the film's info.
    Init {
        dir: PathBuf,
        /// The finished film (any format ffprobe reads).
        #[arg(long)]
        film: PathBuf,
    },
    /// Open the recording window, which runs inside Takewise.app so the
    /// microphone permission belongs to Takewise.
    Record { dir: PathBuf },
    /// The recording window itself; `record` starts it inside the app bundle.
    #[command(hide = true)]
    Window {
        dir: PathBuf,
        /// Where to write whether the window opened, for `record`.
        #[arg(long)]
        result: Option<PathBuf>,
    },
    /// Measure the input and output latency with a loopback test: hold the
    /// headphones to the microphone; beeps play and are recorded. Runs
    /// inside Takewise.app like `record`.
    Calibrate {
        dir: PathBuf,
        /// Store the measured offset as `[input] latency_offset_ms`.
        #[arg(long)]
        write: bool,
        /// Play through this output device instead of the system's default
        /// (the device the narrator's headphones hang on).
        #[arg(long)]
        output: Option<String>,
    },
    /// The measurement itself; `calibrate` runs it inside the app bundle.
    #[command(hide = true)]
    CalibrateRun {
        dir: PathBuf,
        #[arg(long)]
        film: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        output: Option<String>,
    },
    /// Lines, takes, verdicts, proposed and chosen takes, and problems.
    Status {
        dir: PathBuf,
        /// Exit 1 when a line has no chosen take or any problem exists
        /// (notices, such as crash leftovers, do not count).
        #[arg(long)]
        strict: bool,
        /// First adopt take files left without a ledger event by a crash,
        /// finish passages a crash interrupted, and cut published passages
        /// that still lack their takes.
        #[arg(long)]
        repair: bool,
        /// Re-measure the film and update `film_info`, as `film` does
        /// without --film.
        #[arg(long)]
        refresh_film: bool,
    },
    /// Point the project at a new render of its film, keeping the script,
    /// the takes and the setup; warns when the duration changed or a
    /// line's slot now runs past the film's end. Without --film, the film
    /// at the path the project has is measured again.
    Film {
        dir: PathBuf,
        /// The new render (any format ffprobe reads).
        #[arg(long)]
        film: Option<PathBuf>,
    },
    /// Edit script.toml.
    Script {
        #[command(subcommand)]
        command: ScriptCommand,
    },
    /// Check takes: speech, sound, timing and words (whisper-cli), and
    /// append a `checked` event for each. Without --line, checks every take
    /// without a check against its line as it is now. Retake verdicts exit
    /// 0; whisper-cli or the model missing exits 2.
    Check {
        dir: PathBuf,
        /// Check this line's takes, even those already checked.
        #[arg(long)]
        line: Option<String>,
        /// With --line: only this take.
        #[arg(long, requires = "line")]
        take: Option<u32>,
        /// Only takes whose latest check was made against older text or an
        /// older slot.
        #[arg(long)]
        stale_only: bool,
    },
    /// Add an outside judgment of a take.
    #[command(allow_negative_numbers = true)]
    Judge {
        dir: PathBuf,
        line: String,
        take: u32,
        #[arg(long)]
        verdict: VerdictArg,
        #[arg(long)]
        reason: String,
        /// Who judged, for example the model name.
        #[arg(long)]
        by: String,
        /// A score from 0 to 1; higher ranks first among equal takes.
        #[arg(long, value_parser = parse_score)]
        score: Option<f64>,
    },
    /// Choose a take for a line, or with --passage the take of every line
    /// that was cut from one passage.
    Choose {
        dir: PathBuf,
        #[arg(required_unless_present = "passage", conflicts_with = "passage")]
        line: Option<String>,
        #[arg(required_unless_present = "passage", conflicts_with = "passage")]
        take: Option<u32>,
        /// Choose the takes cut from this passage (a take's `passage.id` in
        /// `status --json`), for every line that has one: all of them or,
        /// when one's file is missing, none.
        #[arg(long)]
        passage: Option<String>,
        #[arg(long, default_value = "narrator")]
        by: String,
    },
    /// Write export/narration.wav (or .flac) and export/manifest.json.
    Export(ExportArgs),
    /// Check whisper-cli (with a 1 s transcription), the whisper model,
    /// ffmpeg, ffprobe, the film and the audio devices.
    Doctor { dir: Option<PathBuf> },
    /// Writes this process's microphone permission as JSON to `out`;
    /// `doctor` runs it inside Takewise.app, whose permission it is.
    #[command(hide = true)]
    MicStatus {
        #[arg(long)]
        out: PathBuf,
    },
    /// The speech model the word checks need (not bundled, 1.5 GB).
    Models {
        #[command(subcommand)]
        command: ModelsCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ModelsCommand {
    /// Whether the model is in `$TAKEWISE_MODELS` or
    /// `~/.local/share/takewise/models`.
    Status,
    /// Download the model from Hugging Face, resuming an interrupted
    /// download, and install it once its SHA-256 matches. Progress goes to
    /// stderr.
    Install,
}

#[derive(Debug, Subcommand)]
enum ScriptCommand {
    /// Change one field of one line, keeping the file's comments and layout.
    #[command(allow_negative_numbers = true)]
    #[command(group(ArgGroup::new("field").required(true).args(["text", "start", "end", "note", "clear_end", "clear_note"])))]
    Set {
        dir: PathBuf,
        line: String,
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        start: Option<f64>,
        #[arg(long)]
        end: Option<f64>,
        #[arg(long)]
        note: Option<String>,
        /// Remove the explicit end, so the slot ends at the next line.
        #[arg(long)]
        clear_end: bool,
        #[arg(long)]
        clear_note: bool,
        /// Refuse the edit unless script.toml still has this version (from `status`).
        #[arg(long)]
        expect: Option<ScriptVersion>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum VerdictArg {
    Keep,
    Retake,
    Listen,
}

impl From<VerdictArg> for Verdict {
    fn from(arg: VerdictArg) -> Self {
        match arg {
            VerdictArg::Keep => Verdict::Keep,
            VerdictArg::Retake => Verdict::Retake,
            VerdictArg::Listen => Verdict::Listen,
        }
    }
}

fn parse_score(s: &str) -> Result<f64, String> {
    let score: f64 = s.parse().map_err(|_| format!("{s:?} is not a number"))?;
    if (0.0..=1.0).contains(&score) {
        Ok(score)
    } else {
        Err(format!("the score must be between 0 and 1, not {s}"))
    }
}

/// A usage or setup error: exit code 2.
#[derive(Debug)]
pub enum CliError {
    Core(tw::Error),
    Setup(String),
    /// Transcription cannot be set up: whisper-cli, ffmpeg or the model.
    Whisper(tw::SetupError),
    /// The model could not be installed.
    Model(tw::models::InstallError),
}

impl CliError {
    pub fn code(&self) -> &'static str {
        match self {
            CliError::Core(err) => err.code(),
            CliError::Setup(_) => "setup",
            CliError::Whisper(err) => err.code(),
            CliError::Model(err) => err.code(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Core(err) => err.fmt(f),
            CliError::Setup(message) => f.write_str(message),
            CliError::Whisper(err) => err.fmt(f),
            CliError::Model(err) => err.fmt(f),
        }
    }
}

impl From<tw::Error> for CliError {
    fn from(err: tw::Error) -> Self {
        CliError::Core(err)
    }
}

pub fn run(cli: Cli) -> Result<Outcome, CliError> {
    match cli.command {
        Command::Init { dir, film } => init(&dir, &film),
        Command::Record { dir } => record(&dir),
        Command::Calibrate { dir, write, output } => calibrate(&dir, write, output.as_deref()),
        Command::CalibrateRun {
            dir,
            film,
            out,
            output,
        } => calibrate_run(&dir, &film, &out, output.as_deref()),
        Command::Window { dir, result } => {
            crate::window::run(&dir, result.as_deref()).map_err(CliError::Setup)?;
            Ok(Outcome::new(json!({}), String::new()))
        }
        Command::Status {
            dir,
            strict,
            repair,
            refresh_film,
        } => status(&dir, strict, repair, refresh_film),
        Command::Film { dir, film } => film_command(&dir, film.as_deref()),
        Command::Script {
            command:
                ScriptCommand::Set {
                    dir,
                    line,
                    text,
                    start,
                    end,
                    note,
                    clear_end,
                    clear_note,
                    expect,
                },
        } => {
            let field = match (text, start, end, note) {
                (Some(text), ..) => Field::Text(text),
                (_, Some(start), ..) => Field::Start(start),
                (_, _, Some(end), _) => Field::End(Some(end)),
                (.., Some(note)) => Field::Note(Some(note)),
                _ if clear_end => Field::End(None),
                _ => {
                    debug_assert!(clear_note);
                    Field::Note(None)
                }
            };
            script_set(&dir, &line, field, expect.as_ref())
        }
        Command::Check {
            dir,
            line,
            take,
            stale_only,
        } => check(&dir, line.as_deref(), take, stale_only),
        Command::Judge {
            dir,
            line,
            take,
            verdict,
            reason,
            by,
            score,
        } => {
            let judgment = Judgment {
                by,
                verdict: verdict.into(),
                score,
                reason,
            };
            tw::judge_take(&dir, &line, take, judgment.clone())?;
            Ok(Outcome::new(
                json!({ "line": line, "take": take, "verdict": judgment.verdict, "by": judgment.by, "score": judgment.score }),
                format!("Judged {line} take {take}: {}", judgment.verdict.as_str()),
            ))
        }
        Command::Choose {
            dir,
            line,
            take,
            passage,
            by,
        } => choose(&dir, line.as_deref(), take, passage.as_deref(), &by),
        Command::Export(args) => export(&args),
        Command::Doctor { dir } => Ok(doctor(dir.as_deref())),
        Command::MicStatus { out } => mic_status(&out),
        Command::Models { command } => match command {
            ModelsCommand::Status => Ok(models_status()),
            ModelsCommand::Install => models_install(),
        },
    }
}

/// `choose`: one take of one line, or with `--passage` every take cut from
/// one passage. A missing take file, or a passage not yet cut into all its
/// takes, is a finding (exit 1) and chooses nothing.
fn choose(
    dir: &Path,
    line: Option<&str>,
    take: Option<u32>,
    passage: Option<&str>,
    by: &str,
) -> Result<Outcome, CliError> {
    let (line, take) = match (passage, line, take) {
        (Some(passage), ..) => return choose_passage(dir, passage, by),
        (None, Some(line), Some(take)) => (line, take),
        // clap requires a line and a take without --passage.
        _ => {
            return Err(CliError::Setup(
                "choose needs a line and a take, or --passage <id>".to_owned(),
            ));
        }
    };
    match tw::choose_take(dir, line, take, by) {
        Err(err @ tw::Error::MissingFile { .. }) => {
            return Ok(Outcome::new(json!({ "line": line, "take": take }), "")
                .with_finding("missing-file", err.to_string()));
        }
        result => result?,
    }
    Ok(Outcome::new(
        json!({ "line": line, "take": take, "by": by }),
        format!("Chose {line} take {take} ({by})"),
    ))
}

fn choose_passage(dir: &Path, passage: &str, by: &str) -> Result<Outcome, CliError> {
    let choice = match tw::choose_passage(dir, passage, by) {
        Err(err @ tw::Error::MissingFile { .. }) => {
            return Ok(Outcome::new(json!({ "passage": passage }), "")
                .with_finding("missing-file", err.to_string()));
        }
        Err(err @ tw::Error::PassageNotSplit { .. }) => {
            return Ok(Outcome::new(json!({ "passage": passage }), "")
                .with_finding("passage-not-split", err.to_string()));
        }
        result => result?,
    };
    let chosen: Vec<String> = choice
        .chosen
        .iter()
        .map(|take| format!("{} take {}", take.line, take.take))
        .collect();
    let mut text = if chosen.is_empty() {
        "Chose nothing".to_owned()
    } else {
        format!("Chose {} ({by})", chosen.join(", "))
    };
    for skipped in &choice.skipped {
        let _ = write!(text, "\nskipped {}: {}", skipped.line, skipped.reason);
    }
    Ok(Outcome::new(&choice, text))
}

/// Writes this process's microphone permission to `out` (run inside
/// Takewise.app by `doctor`).
fn mic_status(out: &Path) -> Result<Outcome, CliError> {
    let permission = takewise_media::microphone_permission();
    std::fs::write(out, json!(permission).to_string())
        .map_err(|err| CliError::Setup(format!("{}: {err}", out.display())))?;
    Ok(Outcome::new(json!(permission), permission.as_str()))
}

/// The model Takewise downloads: `[check] whisper_model`'s default.
fn pinned_model() -> tw::models::ModelSpec {
    tw::models::PINNED.spec()
}

/// "1.5 GB" (binary gigabytes, as the design names the model's size).
fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / f64::from(1u32 << 30))
}

fn models_status() -> Outcome {
    use tw::models::ModelState;
    let status = tw::models::status(&tw::whisper::models_dir(), &pinned_model());
    let path = status.path.display();
    let size = gigabytes(status.model.size);
    let text = match status.state {
        ModelState::Installed => format!("Installed: {path}"),
        ModelState::Missing => {
            format!("Missing: {path}\nInstall it with `takewise models install` ({size}).")
        }
        ModelState::Partial { bytes } => format!(
            "Partly downloaded ({} of {size}): {path}\nResume with `takewise models install`.",
            gigabytes(bytes)
        ),
        ModelState::WrongSize { bytes } => format!(
            "{path} is {bytes} bytes, not the {} of {}; move it away and run `takewise models install`.",
            status.model.size, status.model.file
        ),
    };
    Outcome::new(&status, text)
}

fn models_install() -> Result<Outcome, CliError> {
    use std::io::{IsTerminal as _, Write as _};
    use tw::models::Installed;
    let dir = tw::whisper::models_dir();
    let spec = pinned_model();
    let tty = std::io::stderr().is_terminal();
    let mut shown = None;
    let result = tw::models::install(&dir, &spec, &tw::models::Download::default(), |p| {
        // Whole percents on a terminal, every 5 % otherwise (agents, logs).
        let step = if tty { 1 } else { 5 };
        let mark = (
            p.bytes.saturating_mul(100) / p.total.max(1) / step,
            p.verifying,
        );
        if shown != Some(mark) {
            shown = Some(mark);
            let line = if p.verifying {
                format!("Verifying {}…", spec.file)
            } else {
                format!(
                    "Downloading {}: {} of {} ({:.0} %)",
                    spec.file,
                    gigabytes(p.bytes),
                    gigabytes(p.total),
                    p.percent()
                )
            };
            let mut err = std::io::stderr().lock();
            let _ = if tty {
                write!(err, "\r\x1b[2K{line}")
            } else {
                writeln!(err, "{line}")
            };
            let _ = err.flush();
        }
        std::ops::ControlFlow::Continue(())
    });
    if tty && shown.is_some() {
        eprintln!();
    }
    let installed = result.map_err(CliError::Model)?;
    let path = tw::models::model_path(&dir, &spec);
    let text = match installed {
        Installed::Downloaded => format!("Installed {}", path.display()),
        Installed::AlreadyInstalled => format!("Already installed: {}", path.display()),
    };
    Ok(Outcome::new(
        json!({ "result": installed, "path": path, "model": spec }),
        text,
    ))
}

/// The film given on the command line, as `project.toml` stores it: an
/// absolute path with links resolved.
fn film_arg_path(film_arg: &Path) -> Result<PathBuf, CliError> {
    std::fs::canonicalize(film_arg).map_err(|err| {
        CliError::Setup(format!(
            "cannot find the film {}: {err}",
            film_arg.display()
        ))
    })
}

/// Measures the film (`film_arg`, or the one the project has) and points
/// the project at it: `takewise film` and `status --refresh-film`.
fn point_film(dir: &Path, film_arg: Option<&Path>) -> Result<tw::FilmChange, CliError> {
    let project = Project::open(dir)?;
    let film = film_arg.map(film_arg_path).transpose()?;
    let info = film::probe(film.as_deref().unwrap_or(&project.film_path()))?;
    Ok(tw::set_film(
        dir,
        film.as_deref(),
        &info,
        jiff::Timestamp::now(),
    )?)
}

/// `film`: point the project at a new render, or measure its film again.
fn film_command(dir: &Path, film_arg: Option<&Path>) -> Result<Outcome, CliError> {
    let change = point_film(dir, film_arg)?;
    let now = change.film_info.duration_s;
    let mut text = match &change.previous {
        None => format!(
            "{} already uses {} ({now} s); nothing changed.",
            dir.display(),
            change.film.display()
        ),
        Some(previous) if previous.film == change.film => format!(
            "Measured {} again: {now} s (was {} s). The script, the takes and the setup are kept.",
            change.film.display(),
            previous.duration_s
        ),
        Some(previous) => format!(
            "{} now uses {} ({now} s), in place of {} ({} s). The script, the takes and the setup are kept.",
            dir.display(),
            change.film.display(),
            previous.film.display(),
            previous.duration_s
        ),
    };
    text.push('\n');
    for w in &change.warnings {
        match &w.line {
            Some(line) => writeln!(text, "warning: {line}: {} ({})", w.message, w.code),
            None => writeln!(text, "warning: {} ({})", w.message, w.code),
        }
        .ok();
    }
    Ok(Outcome::new(&change, text))
}

pub(crate) fn init(dir: &Path, film_arg: &Path) -> Result<Outcome, CliError> {
    let film_path = film_arg_path(film_arg)?;
    let info = film::probe(&film_path)?;
    let project = Project::init(dir, &film_path, info)?;
    // New projects export clean (a project without `[export]` stays raw).
    tw::set_export(
        dir,
        &tw::ExportConfig {
            processing: tw::Processing::Clean,
            ..tw::ExportConfig::default()
        },
    )?;
    let mut text = format!(
        "Created {} for {} ({:.1} s). Write the lines into script.toml.",
        dir.display(),
        film_path.display(),
        project.config.film_info.duration_s
    );
    // The devices chosen last, in the window, are where a new project starts.
    if let Ok(Some(defaults)) = tw::load_defaults() {
        let input = defaults
            .input
            .as_deref()
            .map(|name| (name, defaults.channel));
        if input.is_some() || defaults.output.is_some() {
            tw::set_route(dir, input, defaults.output.as_deref())?;
            if let Some((name, channel)) = input {
                let _ = write!(
                    text,
                    " Input: {name:?} channel {channel} (your last choice)."
                );
            }
        }
    }
    Ok(Outcome::new(
        json!({ "dir": dir, "film": film_path, "film_info": project.config.film_info }),
        text,
    ))
}

#[derive(Serialize)]
struct StatusData {
    script_version: String,
    film: FilmStatus,
    #[serde(flatten)]
    state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    repair: Option<RepairReport>,
}

#[derive(Serialize)]
struct FilmStatus {
    path: PathBuf,
    info: FilmInfo,
    missing: bool,
    changed: bool,
    /// The film the project used before the one it has now (`takewise
    /// film`), and when it was replaced.
    #[serde(skip_serializing_if = "Option::is_none")]
    previous: Option<tw::PreviousFilm>,
}

fn status(dir: &Path, strict: bool, repair: bool, refresh_film: bool) -> Result<Outcome, CliError> {
    let repair = if repair { Some(tw::repair(dir)?) } else { None };
    if refresh_film {
        point_film(dir, None)?;
    }
    // The script is read once: the version issued is the one folded.
    let (project, script_version) = Project::open_versioned(dir)?;
    let ledger = tw::read(dir)?;
    let mut state = tw::fold(&project, &ledger);

    let film_path = project.film_path();
    let missing = !film_path.exists();
    let changed =
        !missing && !film::matches(&film_path, &project.config.film_info).unwrap_or(false);
    if missing {
        state.problems.push(Problem::new(
            None,
            "film-missing",
            format!(
                "the film {} is missing; point the project at the film with `takewise film <dir> --film <path>`",
                film_path.display()
            ),
        ));
    }
    if changed {
        state.problems.push(Problem::new(
            None,
            "film-changed",
            "the film changed since it was measured; check the timings, then run `takewise film <dir>`",
        ));
    }
    let route = current_route(&project);
    if let Some(stale) = tw::calibration_stale(&project.config, &route) {
        state
            .notices
            .push(Problem::new(None, "calibration-stale", stale));
    }
    if let Some(note) = tw::remembered::latency_note(&project.config, &route) {
        state
            .notices
            .push(Problem::new(None, "calibration-remembered", note));
    }
    if let Some(warning) = tw::setup_warning(&project.config, &route) {
        let code = if project.config.setup.is_none() {
            "no-setup"
        } else {
            "setup-changed"
        };
        state.notices.push(Problem::new(None, code, warning));
    }
    // Audio a window could not save into this project, kept elsewhere.
    if let Some(root) = tw::rescue::rescue_root() {
        state.notices.extend(tw::rescue::notices(&root, dir));
    }

    let data = StatusData {
        script_version: script_version.to_string(),
        film: FilmStatus {
            path: film_path,
            info: project.config.film_info.clone(),
            missing,
            changed,
            previous: project.config.previous_film.clone(),
        },
        state,
        repair,
    };
    let text = status_text(&project, &data);
    let unchosen: Vec<&str> = data
        .state
        .lines
        .iter()
        .filter(|l| l.chosen.is_none())
        .map(|l| l.id.as_str())
        .collect();
    let problems = data.state.problems.len();
    let mut outcome = Outcome::new(&data, text);
    if strict && (!unchosen.is_empty() || problems > 0) {
        let mut message = String::new();
        if !unchosen.is_empty() {
            let _ = write!(
                message,
                "lines without a chosen take: {}",
                unchosen.join(", ")
            );
        }
        if problems > 0 {
            if !message.is_empty() {
                message.push_str("; ");
            }
            let _ = write!(message, "{problems} problem(s)");
        }
        outcome = outcome.with_finding("strict", message);
    }
    Ok(outcome)
}

/// The project's route as it would play now: an unset output is the
/// system's default output, and the buffer size is the input's current one
/// (both unknown when CoreAudio cannot tell).
fn current_route(project: &Project) -> Route {
    let mut route = Route::of(&project.config);
    if route.output.is_none() {
        route.output = takewise_media::default_output().ok().map(|d| d.name);
    }
    route.buffer_frames = route
        .input
        .as_deref()
        .and_then(|device| takewise_media::buffer_frames(device).ok());
    route
}

fn take_label(take: Option<u32>) -> String {
    take.map_or_else(|| "–".to_owned(), |t| t.to_string())
}

fn line_text(line: &LineState) -> String {
    let shown = line
        .chosen
        .as_ref()
        .map(|(take, _)| *take)
        .or(line.proposed)
        .and_then(|take| line.takes.iter().find(|t| t.take == take));
    let chosen = match &line.chosen {
        Some((take, by)) => format!("{take} ({by})"),
        None => "–".to_owned(),
    };
    let mut text = format!(
        "{:<14} {:>2} take(s)  chosen {chosen:<14} proposed {:<3}",
        line.id,
        line.takes.len(),
        take_label(line.proposed)
    );
    if let Some(take) = shown {
        text.push_str(take.effective.map_or("unchecked", Verdict::as_str));
        if take.stale_check {
            text.push_str(" stale");
        }
    }
    text.trim_end().to_owned()
}

fn status_text(project: &Project, data: &StatusData) -> String {
    let mut text = format!(
        "{}  ·  film {:.1} s  ·  {} line(s)\n",
        project.dir.display(),
        project.film_duration(),
        data.state.lines.len()
    );
    if let Some(previous) = &data.film.previous {
        let _ = writeln!(
            text,
            "film replaced {}: was {} ({} s)",
            previous.replaced_at,
            previous.film.display(),
            previous.duration_s
        );
    }
    for line in &data.state.lines {
        text.push_str(&line_text(line));
        text.push('\n');
    }
    if let Some(repair) = &data.repair {
        for take in &repair.adopted {
            let _ = writeln!(text, "repaired: adopted {} take {}", take.line, take.take);
        }
        for path in &repair.metadata_without_audio {
            let _ = writeln!(
                text,
                "repair: {} has no audio; kept, not adopted",
                path.display()
            );
        }
        for id in &repair.passages_published {
            let _ = writeln!(text, "repaired: finished passage {id}");
        }
        for take in &repair.pieces {
            let _ = writeln!(
                text,
                "repaired: cut {} take {} from its passage",
                take.line, take.take
            );
        }
        for failure in &repair.passages_failed {
            let _ = writeln!(
                text,
                "repair: passage {} could not be finished: {}",
                failure.passage, failure.error
            );
        }
        for path in &repair.passages_unplaced {
            let _ = writeln!(
                text,
                "repair: {} is a passage interrupted before it was placed on the film; kept, not cut",
                path.display()
            );
        }
    }
    for (title, list) in [
        ("problems", &data.state.problems),
        ("notices", &data.state.notices),
    ] {
        if list.is_empty() {
            continue;
        }
        let _ = writeln!(text, "{title}:");
        for p in list {
            let line = p
                .line
                .as_deref()
                .map(|l| format!("{l}: "))
                .unwrap_or_default();
            let _ = writeln!(text, "  {line}{} ({})", p.message, p.code);
        }
    }
    for orphan in &data.state.orphans {
        let _ = writeln!(
            text,
            "orphan: {} take {} (its line id is gone)",
            orphan.line, orphan.take
        );
    }
    text
}

fn script_set(
    dir: &Path,
    line: &str,
    field: Field,
    expect: Option<&ScriptVersion>,
) -> Result<Outcome, CliError> {
    let version = tw::set_line_field(dir, line, field, expect)?;
    Ok(Outcome::new(
        json!({ "line": line, "script_version": version.to_string() }),
        format!("Updated {line}"),
    ))
}

#[derive(Debug, clap::Args)]
pub struct ExportArgs {
    dir: PathBuf,
    /// Use the proposed take for lines without a chosen one.
    #[arg(long)]
    use_proposed: bool,
    /// The narration file's format (overrides `[export] format`).
    #[arg(long, value_enum)]
    format: Option<FormatArg>,
    /// Bits per sample (overrides `[export] bit_depth`); 16 is dithered.
    #[arg(long, value_parser = ["16", "24"])]
    bit_depth: Option<String>,
    /// The narration's sample rate (overrides `[export] sample_rate`).
    #[arg(long, value_parser = ["48000", "44100"])]
    sample_rate: Option<String>,
    /// `raw` places the takes as recorded; `clean` filters them and sets the
    /// loudness to -16 LUFS (overrides `[export] processing`).
    #[arg(long, value_enum)]
    processing: Option<ProcessingArg>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum FormatArg {
    Wav,
    Flac,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProcessingArg {
    Raw,
    Clean,
}

impl ExportArgs {
    fn options(&self) -> ExportOptions {
        ExportOptions {
            use_proposed: self.use_proposed,
            format: self.format.map(|f| match f {
                FormatArg::Wav => tw::ExportFormat::Wav,
                FormatArg::Flac => tw::ExportFormat::Flac,
            }),
            bit_depth: self.bit_depth.as_deref().and_then(|b| b.parse().ok()),
            sample_rate: self.sample_rate.as_deref().and_then(|r| r.parse().ok()),
            processing: self.processing.map(|p| match p {
                ProcessingArg::Raw => tw::Processing::Raw,
                ProcessingArg::Clean => tw::Processing::Clean,
            }),
        }
    }
}

/// The film at `path` now, measured like `init` measured it: its size and
/// modification time, and its duration when `ffprobe` can read it within
/// [`tw::ffmpeg::FILM_PROBE_TIMEOUT`] (export probes again under the
/// project lock; a stalled probe is stopped and refuses the export).
fn film_on_disk(path: &Path) -> tw::Result<tw::FilmOnDisk> {
    if !path.exists() {
        return Ok(tw::FilmOnDisk::Missing);
    }
    let (size, modified) = film::stamp_io(path).map_err(|source| tw::Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let duration_s = match tw::whisper::find_tool("ffprobe") {
        Some(ffprobe) => {
            match tw::ffmpeg::probe_duration(&ffprobe, path, tw::ffmpeg::FILM_PROBE_TIMEOUT) {
                Ok(duration_s) => duration_s,
                Err(tw::ffmpeg::ProbeTimedOut) => return Ok(tw::FilmOnDisk::ProbeTimedOut),
            }
        }
        None => None,
    };
    Ok(tw::FilmOnDisk::Found {
        size,
        modified,
        duration_s,
    })
}

fn export(args: &ExportArgs) -> Result<Outcome, CliError> {
    let dir = args.dir.as_path();
    // Probed at the start and again just before publishing.
    let film = Project::open(dir)?.film_path();
    let report = tw::export(dir, &args.options(), &|| film_on_disk(&film))?;
    if report.refused.is_empty() {
        let narration = dir.join("export").join(&report.narration);
        let mut text = format!(
            "Wrote {} ({} line(s))",
            narration.display(),
            report.lines.len()
        );
        if let Some(settings) = &report.settings {
            let _ = write!(
                text,
                ": {} {} Hz {}-bit {}",
                settings.processing.as_str(),
                settings.sample_rate,
                settings.bit_depth,
                settings.format.extension()
            );
            if let (Some(lufs), Some(peak)) = (settings.loudness_lufs, settings.true_peak_dbtp) {
                let _ = write!(text, ", {lufs:.1} LUFS, true peak {peak:.1} dBTP");
            }
        }
        text.push('\n');
        for w in &report.warnings {
            match &w.line {
                Some(line) => writeln!(text, "warning: {line}: {} ({})", w.message, w.code),
                None => writeln!(text, "warning: {} ({})", w.message, w.code),
            }
            .ok();
        }
        return Ok(Outcome::new(
            json!({
                "narration": narration,
                "manifest": dir.join("export/manifest.json"),
                "settings": report.settings,
                "lines": report.lines,
                "warnings": report.warnings,
            }),
            text,
        ));
    }
    let message = report
        .refused
        .iter()
        .map(|r| format!("{} ({})", r.message, r.code))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Outcome::new(&report, "")
        .with_finding("export-refused", format!("export refused:\n{message}")))
}

/// One take's check, as `check --json` lists it.
#[derive(Serialize)]
struct TakeCheck<'a> {
    line: &'a str,
    take: u32,
    #[serde(flatten)]
    checked: &'a tw::Checked,
}

/// A take that could not be checked at all, such as an unreadable file.
#[derive(Serialize)]
struct CheckError {
    line: String,
    take: u32,
    code: &'static str,
    message: String,
}

/// The takes `check` runs on: one take, a line's takes, or every take
/// without a current check; `stale_only` keeps those checked against an
/// older text or slot.
fn takes_to_check(
    dir: &Path,
    line: Option<&str>,
    take: Option<u32>,
    stale_only: bool,
) -> Result<Vec<tw::TakeRef>, CliError> {
    let project = Project::open(dir)?;
    let ledger = tw::read(dir)?;
    if let Some(id) = line {
        if project.snapshot(id).is_none() {
            return Err(tw::Error::UnknownLine(id.to_owned()).into());
        }
        if let Some(n) = take
            && ledger.recorded(id, n).is_none()
        {
            return Err(tw::Error::UnknownTake {
                line: id.to_owned(),
                take: n,
            }
            .into());
        }
    }
    let selected =
        |t: &tw::TakeRef| line.is_none_or(|id| t.line == id) && take.is_none_or(|n| t.take == n);
    if stale_only || line.is_none() {
        return Ok(tw::unchecked_takes(&project, &ledger)
            .into_iter()
            .filter(|u| !stale_only || u.stale)
            .map(|u| u.take)
            .filter(selected)
            .collect());
    }
    let mut takes: Vec<tw::TakeRef> = ledger
        .events
        .iter()
        .filter(|e| matches!(e.kind, tw::EventKind::Recorded(_)))
        .map(|e| tw::TakeRef {
            line: e.line.clone(),
            take: e.take,
        })
        .filter(selected)
        .collect();
    takes.sort();
    takes.dedup();
    Ok(takes)
}

fn check(
    dir: &Path,
    line: Option<&str>,
    take: Option<u32>,
    stale_only: bool,
) -> Result<Outcome, CliError> {
    let takes = takes_to_check(dir, line, take, stale_only)?;
    if takes.is_empty() {
        // Takes left with their words unchecked are not checked again while
        // checking would fail the same way: say why, as a setup error.
        let project = Project::open(dir)?;
        let ledger = tw::read(dir)?;
        let failed = tw::words_not_checked_takes(&project, &ledger)
            .into_iter()
            .any(|t| line.is_none_or(|id| t.line == id) && take.is_none_or(|n| t.take == n));
        if failed {
            tw::WhisperCli::discover(&project.config.check).map_err(CliError::Whisper)?;
        }
        return Ok(Outcome::new(
            json!({ "checks": [], "errors": [] }),
            "Nothing to check.",
        ));
    }
    let config = Project::open(dir)?.config.check;
    let transcriber = tw::WhisperCli::discover(&config).map_err(CliError::Whisper)?;
    // A Takewise that crashed or was force-quit mid-check left its tools.
    crate::window::reap_orphaned_tools(&[transcriber.whisper.clone(), transcriber.ffmpeg.clone()]);
    let mut results = Vec::new();
    let mut errors = Vec::new();
    for take in &takes {
        match tw::check_take(dir, take, &transcriber, env!("CARGO_PKG_VERSION")) {
            Ok(checked) => results.push((take, checked)),
            Err(err) => errors.push(CheckError {
                line: take.line.clone(),
                take: take.take,
                code: err.code(),
                message: err.to_string(),
            }),
        }
    }
    let mut text = String::new();
    for (take, checked) in &results {
        let _ = write!(
            text,
            "{} take {}: {}",
            take.line,
            take.take,
            checked.verdict.as_str()
        );
        for reason in &checked.reasons {
            let _ = write!(text, "; {}", reason.message);
        }
        text.push('\n');
    }
    for err in &errors {
        let _ = writeln!(
            text,
            "{} take {}: not checked: {}",
            err.line, err.take, err.message
        );
    }
    let checks: Vec<TakeCheck> = results
        .iter()
        .map(|(take, checked)| TakeCheck {
            line: &take.line,
            take: take.take,
            checked,
        })
        .collect();
    let outcome = Outcome::new(json!({ "checks": checks, "errors": errors }), text);
    if errors.is_empty() {
        Ok(outcome)
    } else {
        let message = format!("{} take(s) could not be checked", errors.len());
        Ok(outcome.with_finding("check-failed", message))
    }
}

#[derive(Serialize)]
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn tool_check(name: &'static str, install: &str) -> Check {
    match tw::whisper::find_tool(name) {
        Some(path) => Check {
            name,
            ok: true,
            detail: path.display().to_string(),
        },
        None => Check {
            name,
            ok: false,
            detail: format!(
                "not on PATH or in /opt/homebrew/bin or /usr/local/bin; install with `{install}`"
            ),
        },
    }
}

/// Validates the project and its film, then runs the recording window
/// inside Takewise.app and waits for it to close; fails when the window
/// could not open.
fn record(dir: &Path) -> Result<Outcome, CliError> {
    let project = Project::open(dir)?;
    let film = project.film_path();
    if !film.is_file() {
        return Err(CliError::Setup(format!(
            "the film {} does not exist; put it back or point `film` in project.toml at it",
            film.display()
        )));
    }
    let problems = tw::validate(&project.script, project.film_duration());
    if !problems.is_empty() {
        let mut text = String::new();
        for problem in &problems {
            let line = problem.line.as_deref().unwrap_or("-");
            let _ = writeln!(text, "{line}: {}", problem.message);
        }
        let count = problems.len();
        return Ok(
            Outcome::new(json!({ "problems": problems }), text).with_finding(
                "invalid-script",
                format!("fix the script before recording ({count} problem(s))"),
            ),
        );
    }
    let dir = std::path::absolute(dir)
        .map_err(|err| CliError::Setup(format!("{}: {err}", dir.display())))?;
    let work = tempfile::tempdir()
        .map_err(|err| CliError::Setup(format!("no folder for the window's result: {err}")))?;
    let result = work.path().join("window.json");
    let bundle = run_in_bundle(&[
        "window".as_ref(),
        dir.as_os_str(),
        "--result".as_ref(),
        result.as_os_str(),
    ])?;
    window_opened(&result)?;
    Ok(Outcome::new(
        json!({ "bundle": bundle }),
        format!("Recording window closed ({}).", bundle.display()),
    ))
}

/// Whether the window opened, by the result it wrote to `result`.
fn window_opened(result: &Path) -> Result<(), CliError> {
    #[derive(serde::Deserialize)]
    struct Reply {
        #[serde(default)]
        ok: bool,
        error: Option<String>,
    }
    let text = std::fs::read_to_string(result).map_err(|_| {
        CliError::Setup(
            "the recording window did not start (Takewise.app wrote no result)".to_owned(),
        )
    })?;
    let reply: Reply = serde_json::from_str(&text)
        .map_err(|err| CliError::Setup(format!("unreadable result from the window: {err}")))?;
    match (reply.ok, reply.error) {
        (_, Some(error)) => Err(CliError::Setup(format!(
            "the recording window could not open: {error}"
        ))),
        (true, None) => Ok(()),
        (false, None) => Err(CliError::Setup(
            "the recording window did not say whether it opened".to_owned(),
        )),
    }
}

/// Runs `takewise <args>` inside Takewise.app and waits for it to quit.
fn run_in_bundle(args: &[&std::ffi::OsStr]) -> Result<PathBuf, CliError> {
    let bundle = find_bundle()?;
    let status = std::process::Command::new("open")
        .arg("-n")
        .arg("-W")
        .arg(&bundle)
        .arg("--args")
        .args(args)
        .status()
        .map_err(|err| CliError::Setup(format!("could not run `open`: {err}")))?;
    if !status.success() {
        return Err(CliError::Setup(format!(
            "`open {}` failed ({status})",
            bundle.display()
        )));
    }
    Ok(bundle)
}

/// Makes the beep film, measures inside Takewise.app, reports the placement
/// error and with `--write` stores the offset that cancels it.
fn calibrate(dir: &Path, write: bool, output: Option<&str>) -> Result<Outcome, CliError> {
    let project = Project::open(dir)?;
    if project.config.input.device.is_none() {
        return Err(CliError::Setup(
            "no input device chosen; set [input] device in project.toml (`takewise doctor` lists them)"
                .to_owned(),
        ));
    }
    let dir = std::path::absolute(dir)
        .map_err(|err| CliError::Setup(format!("{}: {err}", dir.display())))?;
    let work = std::env::temp_dir().join(format!("takewise-calibrate-{}", std::process::id()));
    std::fs::create_dir_all(&work)
        .map_err(|err| CliError::Setup(format!("{}: {err}", work.display())))?;
    let measured = measure_in_bundle(&dir, &work, output);
    let _ = std::fs::remove_dir_all(&work);
    let measurement = measured?;

    let placed = measurement.placed.len();
    let mut text = format!(
        "Input {:?}, output {:?}: {placed} of {} beeps placed",
        measurement.input, measurement.output, measurement.expected
    );
    let (Some(mean), Some(sd)) = (measurement.mean_ms, measurement.sd_ms) else {
        return Ok(
            Outcome::new(json!({ "measurement": measurement }), text).with_finding(
                "too-few-beeps",
                "no beeps detected; hold the headphones to the mic and raise the volume",
            ),
        );
    };
    let _ = write!(
        text,
        ", mean error {mean:+.1} ms, standard deviation {sd:.1} ms."
    );
    if placed < calibrate::MIN_BEEPS {
        return Ok(Outcome::new(json!({ "measurement": measurement }), text).with_finding(
            "too-few-beeps",
            format!(
                "only {placed} beeps detected (need {}); hold the headphones to the mic and raise the volume",
                calibrate::MIN_BEEPS
            ),
        ));
    }
    let offset = calibrate::offset_for(mean);
    let far = calibrate::outliers(&measurement.placed, mean);
    if !far.is_empty() {
        let list: Vec<String> = far
            .iter()
            .map(|p| format!("{:.1} s ({:+.1} ms)", p.film_s, p.error_ms))
            .collect();
        let why = format!(
            "{} beep(s) landed more than {} ms from the mean error: {}; the placement is not consistent enough for one offset. Check the setup (wired headphones held to the mic, nothing else playing) and run it again.",
            far.len(),
            calibrate::MAX_DEVIATION_MS,
            list.join(", ")
        );
        if write {
            let _ = write!(text, "\nNot written: latency_offset_ms stays as it is.");
            return Ok(Outcome::new(
                json!({ "measurement": measurement, "latency_offset_ms": offset, "written": false }),
                text,
            )
            .with_finding("inconsistent-beeps", why));
        }
        let _ = write!(text, "\nWarning: {why}");
    }
    if write {
        let info = CalibrationInfo {
            input: measurement.input.clone(),
            output: measurement.output.clone(),
            buffer_frames: measurement.buffer_frames,
            date: calibrate::today(),
            from: None,
        };
        tw::set_calibration(&dir, offset, &info)?;
        let _ = write!(
            text,
            "\nWrote latency_offset_ms = {offset} and the devices it was measured on to project.toml."
        );
        // Other projects on exactly these devices and buffer reuse it.
        let remembered = tw::remembered::remembered_path()
            .map(|path| tw::remembered::remember_latency(&path, offset, &info));
        if let Some(Err(err)) = remembered {
            let _ = write!(text, "\nCould not remember it for other projects: {err}");
        }
    } else {
        let _ = write!(
            text,
            "\nlatency_offset_ms should be {offset}; run with --write to store it."
        );
    }
    Ok(Outcome::new(
        json!({ "measurement": measurement, "latency_offset_ms": offset, "written": write }),
        text,
    ))
}

fn measure_in_bundle(
    dir: &Path,
    work: &Path,
    output: Option<&str>,
) -> Result<calibrate::Measurement, CliError> {
    #[derive(serde::Deserialize)]
    struct Reply {
        measurement: Option<calibrate::Measurement>,
        error: Option<String>,
    }
    let film = calibrate::make_beep_film(work).map_err(CliError::Setup)?;
    let out = work.join("result.json");
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "calibrate-run".as_ref(),
        dir.as_os_str(),
        "--film".as_ref(),
        film.as_os_str(),
        "--out".as_ref(),
        out.as_os_str(),
    ];
    if let Some(output) = output {
        args.push(std::ffi::OsStr::new("--output"));
        args.push(std::ffi::OsStr::new(output));
    }
    run_in_bundle(&args)?;
    let text = std::fs::read_to_string(&out).map_err(|err| {
        CliError::Setup(format!(
            "the measurement in Takewise.app gave no result ({err}); was the microphone allowed?"
        ))
    })?;
    let reply: Reply = serde_json::from_str(&text)
        .map_err(|err| CliError::Setup(format!("unreadable measurement: {err}")))?;
    match (reply.measurement, reply.error) {
        (Some(measurement), _) => Ok(measurement),
        (None, error) => Err(CliError::Setup(format!(
            "calibration failed: {}",
            error.unwrap_or_else(|| "no reason given".to_owned())
        ))),
    }
}

/// Inside Takewise.app: measure and write the result for `calibrate`.
fn calibrate_run(
    dir: &Path,
    film: &Path,
    out: &Path,
    output: Option<&str>,
) -> Result<Outcome, CliError> {
    let result = Project::open(dir)
        .map_err(|err| err.to_string())
        .and_then(|project| calibrate::measure(&project, film, output));
    let reply = match result {
        Ok(measurement) => json!({ "measurement": measurement }),
        Err(error) => json!({ "error": error }),
    };
    std::fs::write(out, reply.to_string())
        .map_err(|err| CliError::Setup(format!("{}: {err}", out.display())))?;
    Ok(Outcome::new(json!({}), String::new()))
}

/// Takewise.app: the bundle this binary runs in (through symlinks, such as
/// Homebrew's), else `$TAKEWISE_APP` (which must then exist), else
/// `/Applications/Takewise.app`, else, in debug builds, the repository's
/// `build/Takewise.app`.
fn find_bundle() -> Result<PathBuf, CliError> {
    let is_bundle = |app: &Path| app.join("Contents/MacOS/takewise").is_file();
    if let Some(app) = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(bundle_of_exe)
    {
        return Ok(app);
    }
    if let Some(app) = std::env::var_os("TAKEWISE_APP") {
        let app = PathBuf::from(app);
        return if is_bundle(&app) {
            Ok(app)
        } else {
            Err(CliError::Setup(format!(
                "TAKEWISE_APP is {}, which is not a Takewise.app; build one with `mise run bundle`",
                app.display()
            )))
        };
    }
    let candidates = bundle_candidates();
    candidates
        .iter()
        .find(|app| is_bundle(app))
        .cloned()
        .ok_or_else(|| {
            let places: Vec<String> = candidates.iter().map(|c| c.display().to_string()).collect();
            CliError::Setup(format!(
                "Takewise.app is not in {}; install it, run `mise run bundle` or set TAKEWISE_APP",
                places.join(" or ")
            ))
        })
}

/// The bundle `exe` runs in, after resolving symlinks.
fn bundle_of_exe(exe: &Path) -> Option<PathBuf> {
    enclosing_bundle(&exe.canonicalize().ok()?)
}

/// Where to look for Takewise.app when this binary is not in one. The
/// repository's build directory is a candidate only in debug builds: a
/// release binary must not depend on the path it was built at.
fn bundle_candidates() -> Vec<PathBuf> {
    std::iter::once(PathBuf::from("/Applications/Takewise.app"))
        .chain(repo_build_bundle())
        .collect()
}

/// The repository's `build/Takewise.app`, from the path this binary was
/// built at.
#[cfg(debug_assertions)]
fn repo_build_bundle() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(|repo| repo.join("build/Takewise.app"))
}

#[cfg(not(debug_assertions))]
fn repo_build_bundle() -> Option<PathBuf> {
    None
}

/// `X.app` when `exe` is `X.app/Contents/MacOS/<binary>`.
fn enclosing_bundle(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app")
        .then(|| app.to_owned())
}

/// What `doctor` found out about the audio devices.
struct AudioReport {
    checks: Vec<Check>,
    warnings: Vec<String>,
    inputs: Vec<serde_json::Value>,
    output: Option<serde_json::Value>,
    /// The output the recording window plays through, with `from`; `None`
    /// when it plays nothing (the project's output is not connected).
    playback: Option<serde_json::Value>,
    text: String,
}

fn device_json(device: &DeviceInfo) -> serde_json::Value {
    json!({
        "name": device.name,
        "sample_rate": device.sample_rate,
        "channels": device.channels,
        "transport": device.transport.as_str(),
    })
}

/// Lists the input devices and the default output, checks the project's
/// input and output devices, says which output the recording window plays
/// through, and warns when that is Bluetooth.
fn audio_devices(project: Option<&Project>) -> AudioReport {
    let mut report = AudioReport {
        checks: Vec::new(),
        warnings: Vec::new(),
        inputs: Vec::new(),
        output: None,
        playback: None,
        text: String::new(),
    };
    match takewise_media::input_devices() {
        Ok(inputs) => {
            report.text.push_str("Input devices:\n");
            for input in &inputs {
                let _ = writeln!(
                    report.text,
                    "  {:<32} {:>6} Hz  {} ch  {}",
                    input.name,
                    input.sample_rate,
                    input.channels,
                    input.transport.as_str()
                );
            }
            report.inputs = inputs.iter().map(device_json).collect();
            if let Some(project) = project {
                let available: Vec<AvailableInput> = inputs
                    .iter()
                    .map(|input| AvailableInput {
                        name: input.name.clone(),
                        sample_rate: input.sample_rate,
                    })
                    .collect();
                let configured = project.config.input.device.as_deref();
                let (ok, detail) = match check_input(configured, &available) {
                    Ok(()) => (
                        true,
                        format!(
                            "{} channel {}",
                            configured.unwrap_or_default(),
                            project.config.input.channel
                        ),
                    ),
                    Err(message) => (false, message),
                };
                report.checks.push(Check {
                    name: "input",
                    ok,
                    detail,
                });
            }
        }
        Err(err) => report.checks.push(Check {
            name: "input-devices",
            ok: false,
            detail: err.to_string(),
        }),
    }
    let default = takewise_media::default_output();
    if let Err(err) = &default {
        report.warnings.push(format!("no default output: {err}"));
    }
    let named = project.and_then(|p| p.config.output.device.as_deref());
    let outputs = if named.is_some() {
        takewise_media::output_devices().unwrap_or_default()
    } else {
        Vec::new()
    };
    // Connected as the window decides it: by the lookup that sets the
    // players, so a Bluetooth device that briefly lists no configuration
    // still counts.
    let connected = named.is_some_and(|name| takewise_media::output_uid(name).is_ok());
    let playback = playback_report(named, connected, &outputs, default.as_ref().ok());
    report.checks.extend(playback.check);
    report.warnings.extend(playback.warnings);
    report.text.push_str(&playback.text);
    report.output = default.as_ref().ok().map(device_json);
    report.playback = playback.json;
    report
}

/// What `doctor` says about where the recording window plays the film and
/// the takes.
#[derive(Default)]
struct PlaybackReport {
    /// The `output` check, for a project that names an output.
    check: Option<Check>,
    warnings: Vec<String>,
    json: Option<serde_json::Value>,
    text: String,
}

/// Where the recording window plays: the project's `[output] device`
/// (`named`) while it is `connected`; nothing while it is not, since the
/// window never plays through the system output in its place and does not
/// record; and the system's `default` output when the project names none.
/// A named output that is not connected fails the `output` check. A
/// Bluetooth output that plays is a warning: its latency is long and
/// varies.
fn playback_report(
    named: Option<&str>,
    connected: bool,
    outputs: &[DeviceInfo],
    default: Option<&DeviceInfo>,
) -> PlaybackReport {
    let describe = |device: &DeviceInfo| format!("{} ({})", device.name, device.transport.as_str());
    let mut report = PlaybackReport::default();
    // The device that plays (when it is listed), its name, and whose it is.
    let playing: Option<(Option<&DeviceInfo>, &str, &str)> = match named {
        Some(name) if connected => {
            let device = outputs.iter().find(|o| o.name == name);
            report.check = Some(Check {
                name: "output",
                ok: true,
                detail: format!(
                    "{}: the recording window plays the film and the takes there",
                    device.map_or_else(|| name.to_owned(), describe)
                ),
            });
            Some((device, name, "project"))
        }
        Some(name) => {
            let system = default.map_or_else(String::new, |d| {
                let late = if d.transport == Transport::Bluetooth {
                    ", whose delay would put the film's sound late against your voice"
                } else {
                    ""
                };
                format!(" (now {}{late})", describe(d))
            });
            report.check = Some(Check {
                name: "output",
                ok: false,
                detail: format!(
                    "{name}, the project's output, is not connected, so the recording window plays nothing and does not record; it never plays through the system output{system} in its place. Connect {name}, or pick another output, or System output, in the audio panel (A)"
                ),
            });
            None
        }
        None => default.map(|d| (Some(d), d.name.as_str(), "system")),
    };
    match playing {
        Some((device, name, from)) => {
            let mut json = device.map_or_else(|| json!({ "name": name }), device_json);
            json["from"] = json!(from);
            report.json = Some(json);
            if let Some(device) = device
                && device.transport == Transport::Bluetooth
            {
                report.warnings.push(format!(
                    "output {} is Bluetooth; its latency is long and varies, so record with wired or built-in headphones",
                    device.name
                ));
            }
            let whose = if from == "project" {
                "the project's output"
            } else {
                "the system output"
            };
            let shown = device.map_or_else(|| name.to_owned(), describe);
            let _ = writeln!(report.text, "Playback: {shown}, {whose}");
        }
        None => {
            let _ = writeln!(
                report.text,
                "Playback: nothing, {} is not connected",
                named.unwrap_or_default()
            );
        }
    }
    if let Some(d) = default {
        let _ = writeln!(
            report.text,
            "System output: {} ({} Hz, {})",
            d.name,
            d.sample_rate,
            d.transport.as_str()
        );
    }
    report
}

/// Whether the configured model is there, and how to install it if not.
fn model_check(model: &str) -> Check {
    use tw::models::ModelState;
    let dir = tw::whisper::models_dir();
    let model_path = dir.join(model);
    let Some(spec) = tw::models::pinned(model) else {
        return Check {
            name: "whisper-model",
            ok: model_path.is_file(),
            detail: if model_path.is_file() {
                model_path.display().to_string()
            } else {
                format!(
                    "{} is missing; download {model} there",
                    model_path.display()
                )
            },
        };
    };
    let size = gigabytes(spec.size);
    let (ok, detail) = match tw::models::status(&dir, &spec).state {
        ModelState::Installed => (true, model_path.display().to_string()),
        ModelState::Missing => (
            false,
            format!(
                "{} is missing; install it with `takewise models install` ({size})",
                model_path.display()
            ),
        ),
        ModelState::Partial { bytes } => (
            false,
            format!(
                "{} is partly downloaded ({} of {size}); resume with `takewise models install`",
                model_path.display(),
                gigabytes(bytes)
            ),
        ),
        // Another file of that name may still be a model whisper-cli
        // reads; the transcription check below tells.
        ModelState::WrongSize { bytes } => (
            true,
            format!(
                "{} ({bytes} bytes, not the {} Takewise installs)",
                model_path.display(),
                spec.size
            ),
        ),
    };
    Check {
        name: "whisper-model",
        ok,
        detail,
    }
}

/// Whether Takewise.app may use the microphone. The permission is the
/// app's, so it is asked inside the app (in the background, without a
/// window or a prompt); this process would report its terminal's.
fn microphone_check() -> Result<Check, String> {
    let bundle = find_bundle().map_err(|err| err.to_string())?;
    let work = tempfile::Builder::new()
        .prefix("takewise-doctor-")
        .tempdir()
        .map_err(|err| err.to_string())?;
    let out = work.path().join("microphone.json");
    let status = std::process::Command::new("open")
        .args(["-n", "-W", "-g", "-j"])
        .arg(&bundle)
        .args(["--args", "mic-status", "--out"])
        .arg(&out)
        .status()
        .map_err(|err| format!("could not run `open`: {err}"))?;
    if !status.success() {
        return Err(format!("`open {}` failed ({status})", bundle.display()));
    }
    let text = std::fs::read_to_string(&out).map_err(|err| format!("no answer ({err})"))?;
    let permission: tw::MicPermission =
        serde_json::from_str(&text).map_err(|err| format!("unreadable answer: {err}"))?;
    let (ok, detail) = match (permission, tw::mic_advice(permission)) {
        (_, Some(advice)) => (false, advice),
        (tw::MicPermission::NotDetermined, None) => {
            let detail = "not asked yet; macOS asks the first time Takewise listens to your \
                 input, usually when you pick it in the audio panel";
            (true, detail.to_owned())
        }
        (_, None) => (true, format!("{} may use it", bundle.display())),
    };
    Ok(Check {
        name: "microphone",
        ok,
        detail,
    })
}

/// Transcribes one second of silence, to prove whisper-cli, ffmpeg and
/// the model work together.
fn transcription_check(config: &CheckConfig) -> Check {
    let fail = |detail: String| Check {
        name: "transcription",
        ok: false,
        detail,
    };
    let transcriber = match tw::WhisperCli::discover(config) {
        Ok(transcriber) => transcriber,
        Err(err) => return fail(err.to_string()),
    };
    let silence = match tempfile::Builder::new()
        .prefix("takewise-doctor-")
        .tempdir()
    {
        Ok(dir) => dir,
        Err(err) => return fail(format!("could not create a temporary directory: {err}")),
    };
    let wav = silence.path().join("silence.wav");
    if let Err(err) = tw::write_take_wav(&wav, &vec![0; 48_000], 48_000) {
        return fail(err.to_string());
    }
    let began = std::time::Instant::now();
    match tw::Transcriber::transcribe(&transcriber, &wav) {
        tw::Transcript::Words(_) => Check {
            name: "transcription",
            ok: true,
            detail: format!(
                "transcribed 1 s of silence in {:.1} s with {}",
                began.elapsed().as_secs_f64(),
                transcriber.model_name()
            ),
        },
        tw::Transcript::Failed(err) => fail(err),
    }
}

fn doctor(dir: Option<&Path>) -> Outcome {
    let mut checks = vec![tool_check("whisper-cli", "brew install whisper-cpp")];
    let project = dir.map(Project::open);
    let config = match &project {
        Some(Ok(project)) => project.config.check.clone(),
        _ => CheckConfig::default(),
    };
    checks.push(model_check(&config.whisper_model));
    checks.push(tool_check("ffmpeg", "brew install ffmpeg"));
    checks.push(tool_check("ffprobe", "brew install ffmpeg"));
    if checks.iter().all(|check| check.ok) {
        checks.push(transcription_check(&config));
    }
    match &project {
        None => {}
        Some(Err(err)) => checks.push(Check {
            name: "project",
            ok: false,
            detail: err.to_string(),
        }),
        Some(Ok(project)) => {
            let film_path = project.film_path();
            let (ok, detail) = match film::matches(&film_path, &project.config.film_info) {
                Ok(true) => (true, film_path.display().to_string()),
                Ok(false) => (
                    false,
                    format!(
                        "{} changed since it was measured; run `takewise status --refresh-film`",
                        film_path.display()
                    ),
                ),
                Err(err) => (false, err.to_string()),
            };
            checks.push(Check {
                name: "film",
                ok,
                detail,
            });
        }
    }

    let mut devices = audio_devices(project.as_ref().and_then(|p| p.as_ref().ok()));
    checks.append(&mut devices.checks);
    match microphone_check() {
        Ok(check) => checks.push(check),
        Err(why) => devices.warnings.push(format!(
            "could not ask Takewise.app for its microphone permission: {why}"
        )),
    }

    let mut text = String::new();
    for check in &checks {
        let mark = if check.ok { "ok  " } else { "FAIL" };
        let _ = writeln!(text, "{mark} {:<14} {}", check.name, check.detail);
    }
    for warning in &devices.warnings {
        let _ = writeln!(text, "warn {warning}");
    }
    text.push_str(&devices.text);
    let failed: Vec<&str> = checks.iter().filter(|c| !c.ok).map(|c| c.name).collect();
    let outcome = Outcome::new(
        json!({
            "checks": checks,
            "warnings": devices.warnings,
            "inputs": devices.inputs,
            "output": devices.output,
            "playback": devices.playback,
        }),
        text,
    );
    if failed.is_empty() {
        outcome
    } else {
        outcome.with_finding("doctor", format!("failed checks: {}", failed.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_result_is_read() {
        let tmp = tempfile::tempdir().expect("dir");
        let path = tmp.path().join("window.json");
        let err = window_opened(&path).expect_err("no result");
        assert!(err.to_string().contains("did not start"), "{err}");
        std::fs::write(&path, r#"{"error":"the film is not a film"}"#).expect("write");
        let err = window_opened(&path).expect_err("failed");
        assert_eq!(err.code(), "setup");
        assert!(err.to_string().contains("the film is not a film"), "{err}");
        std::fs::write(&path, r#"{"ok":true}"#).expect("write");
        assert!(window_opened(&path).is_ok());
    }

    #[test]
    fn a_symlinked_binary_finds_its_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let macos = tmp.path().join("Takewise.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::write(macos.join("takewise"), "").unwrap();
        // Homebrew links the binary into its bin directory.
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::os::unix::fs::symlink(macos.join("takewise"), bin.join("takewise")).unwrap();
        let app = tmp.path().join("Takewise.app").canonicalize().unwrap();
        assert_eq!(bundle_of_exe(&bin.join("takewise")), Some(app.clone()));
        assert_eq!(bundle_of_exe(&macos.join("takewise")), Some(app));
        assert_eq!(bundle_of_exe(&bin.join("missing")), None);
    }

    fn device(name: &str, transport: Transport) -> DeviceInfo {
        DeviceInfo {
            name: name.to_owned(),
            sample_rate: 48_000,
            channels: 2,
            transport,
            output_source: takewise_media::OutputSource::Unknown,
        }
    }

    #[test]
    fn a_missing_project_output_fails_and_nothing_plays() {
        // The issue's setup: the project names the interface, the Mac's
        // default output is Bluetooth headphones.
        let sony = device("WH-1000XM5", Transport::Bluetooth);
        let report = playback_report(Some("RØDECaster Duo"), false, &[], Some(&sony));
        let check = report.check.expect("an output check");
        assert!(!check.ok);
        assert_eq!(check.name, "output");
        assert!(check.detail.contains("RØDECaster Duo"), "{}", check.detail);
        assert!(
            check.detail.contains("WH-1000XM5 (bluetooth)")
                && check.detail.contains("late against your voice"),
            "{}",
            check.detail
        );
        assert_eq!(report.json, None);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(
            report
                .text
                .contains("Playback: nothing, RØDECaster Duo is not connected"),
            "{}",
            report.text
        );
        // A wired default output: still a failure, without the delay.
        let speakers = device("MacBook Air Speakers", Transport::BuiltIn);
        let report = playback_report(Some("RØDECaster Duo"), false, &[], Some(&speakers));
        let check = report.check.expect("an output check");
        assert!(!check.ok);
        assert!(!check.detail.contains("late"), "{}", check.detail);
    }

    #[test]
    fn a_connected_project_output_plays_whatever_the_default() {
        let duo = device("RØDECaster Duo", Transport::Usb);
        let sony = device("WH-1000XM5", Transport::Bluetooth);
        let outputs = [duo.clone(), sony.clone()];
        let report = playback_report(Some("RØDECaster Duo"), true, &outputs, Some(&sony));
        let check = report.check.expect("an output check");
        assert!(check.ok, "{}", check.detail);
        assert!(
            check.detail.starts_with("RØDECaster Duo (usb)"),
            "{}",
            check.detail
        );
        let json = report.json.expect("plays");
        assert_eq!(json["name"], "RØDECaster Duo");
        assert_eq!(json["transport"], "usb");
        assert_eq!(json["from"], "project");
        // The Bluetooth default is not used, so it is not warned about.
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(
            report
                .text
                .contains("Playback: RØDECaster Duo (usb), the project's output")
        );
        assert!(
            report
                .text
                .contains("System output: WH-1000XM5 (48000 Hz, bluetooth)")
        );
        // A project that chose Bluetooth headphones gets the warning.
        let report = playback_report(Some("WH-1000XM5"), true, &outputs, Some(&duo));
        assert!(report.check.expect("a check").ok);
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        // Connected but not listed (no configuration for a moment): named.
        let report = playback_report(Some("Other"), true, &outputs, Some(&duo));
        assert_eq!(report.json.expect("plays")["name"], "Other");
    }

    #[test]
    fn without_a_project_output_the_system_output_plays() {
        let sony = device("WH-1000XM5", Transport::Bluetooth);
        let report = playback_report(None, false, &[], Some(&sony));
        assert!(report.check.is_none());
        let json = report.json.expect("plays");
        assert_eq!(json["name"], "WH-1000XM5");
        assert_eq!(json["from"], "system");
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(report.warnings[0].contains("is Bluetooth"));
        let report = playback_report(None, false, &[], None);
        assert!(report.check.is_none() && report.json.is_none());
    }

    #[test]
    fn the_build_directory_is_a_candidate_only_in_debug_builds() {
        let candidates = bundle_candidates();
        assert_eq!(candidates[0], Path::new("/Applications/Takewise.app"));
        let from_build = candidates.iter().any(|c| c.ends_with("build/Takewise.app"));
        assert_eq!(from_build, cfg!(debug_assertions), "{candidates:?}");
    }
}
