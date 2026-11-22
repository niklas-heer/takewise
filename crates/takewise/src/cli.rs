//! The command-line interface: argument parsing and one handler per command.

use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::{ArgGroup, Parser, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::json;
use takewise_core::{
    self as tw, CheckConfig, ExportOptions, Field, FilmInfo, Judgment, LineState, Problem, Project,
    RepairReport, ScriptVersion, State, Verdict,
};

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
    /// Open the recording window (milestone 2).
    Record { dir: PathBuf },
    /// Lines, takes, verdicts, proposed and chosen takes, and problems.
    Status {
        dir: PathBuf,
        /// Exit 1 when a line has no chosen take or any problem exists.
        #[arg(long)]
        strict: bool,
        /// Adopt take files left without a ledger event by a crash first.
        #[arg(long)]
        repair: bool,
        /// Re-measure the film and update `film_info`.
        #[arg(long)]
        refresh_film: bool,
    },
    /// Edit script.toml.
    Script {
        #[command(subcommand)]
        command: ScriptCommand,
    },
    /// Run checks on takes (milestone 3).
    Check {
        dir: PathBuf,
        #[arg(long)]
        line: Option<String>,
        #[arg(long)]
        take: Option<u32>,
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
    /// Choose a take for a line.
    Choose {
        dir: PathBuf,
        line: String,
        take: u32,
        #[arg(long, default_value = "narrator")]
        by: String,
    },
    /// Write export/narration.wav and export/manifest.json.
    Export {
        dir: PathBuf,
        /// Use the proposed take for lines without a chosen one.
        #[arg(long)]
        use_proposed: bool,
    },
    /// Check whisper-cli, the whisper model, ffprobe and the film.
    Doctor { dir: Option<PathBuf> },
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
    NotYet(String),
}

impl CliError {
    pub fn code(&self) -> &'static str {
        match self {
            CliError::Core(err) => err.code(),
            CliError::Setup(_) => "setup",
            CliError::NotYet(_) => "not-implemented",
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Core(err) => err.fmt(f),
            CliError::Setup(message) | CliError::NotYet(message) => f.write_str(message),
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
        Command::Record { .. } => Err(CliError::NotYet(
            "recording arrives in milestone 2; until then takes can only be added through takewise-core".to_owned(),
        )),
        Command::Status {
            dir,
            strict,
            repair,
            refresh_film,
        } => status(&dir, strict, repair, refresh_film),
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
        Command::Check { .. } => Err(CliError::NotYet(
            "checks arrive in milestone 3; agents can add judgments with `takewise judge` now".to_owned(),
        )),
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
        Command::Choose { dir, line, take, by } => {
            tw::choose_take(&dir, &line, take, &by)?;
            Ok(Outcome::new(
                json!({ "line": line, "take": take, "by": by }),
                format!("Chose {line} take {take} ({by})"),
            ))
        }
        Command::Export { dir, use_proposed } => export(&dir, use_proposed),
        Command::Doctor { dir } => Ok(doctor(dir.as_deref())),
    }
}

fn init(dir: &Path, film_arg: &Path) -> Result<Outcome, CliError> {
    let film_path = std::fs::canonicalize(film_arg).map_err(|err| {
        CliError::Setup(format!(
            "cannot find the film {}: {err}",
            film_arg.display()
        ))
    })?;
    let info = film::probe(&film_path)?;
    let project = Project::init(dir, &film_path, info)?;
    let text = format!(
        "Created {} for {} ({:.1} s). Write the lines into script.toml.",
        dir.display(),
        film_path.display(),
        project.config.film_info.duration_s
    );
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
}

fn status(dir: &Path, strict: bool, repair: bool, refresh_film: bool) -> Result<Outcome, CliError> {
    let repair = if repair { Some(tw::repair(dir)?) } else { None };
    if refresh_film {
        let info = film::probe(&Project::open(dir)?.film_path())?;
        tw::set_film_info(dir, &info)?;
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
            format!("the film {} is missing", film_path.display()),
        ));
    }
    if changed {
        state.problems.push(Problem::new(
            None,
            "film-changed",
            "the film changed since it was measured; check the timings, then run `takewise status --refresh-film`",
        ));
    }

    let data = StatusData {
        script_version: script_version.to_string(),
        film: FilmStatus {
            path: film_path,
            info: project.config.film_info.clone(),
            missing,
            changed,
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
    }
    if !data.state.problems.is_empty() {
        text.push_str("problems:\n");
        for p in &data.state.problems {
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

fn export(dir: &Path, use_proposed: bool) -> Result<Outcome, CliError> {
    let project = Project::open(dir)?;
    let state = tw::fold(&project, &tw::read(dir)?);
    let report = tw::export(&project, &state, &ExportOptions { use_proposed })?;
    if report.refused.is_empty() {
        let narration = dir.join("export/narration.wav");
        let text = format!(
            "Wrote {} ({} line(s))",
            narration.display(),
            report.lines.len()
        );
        return Ok(Outcome::new(
            json!({
                "narration": narration,
                "manifest": dir.join("export/manifest.json"),
                "lines": report.lines,
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

#[derive(Serialize)]
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

fn tool_check(name: &'static str, install: &str) -> Check {
    match which(name) {
        Some(path) => Check {
            name,
            ok: true,
            detail: path.display().to_string(),
        },
        None => Check {
            name,
            ok: false,
            detail: format!("not on PATH; install with `{install}`"),
        },
    }
}

/// `$TAKEWISE_MODELS`, else `~/.local/share/takewise/models`.
fn models_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TAKEWISE_MODELS") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local/share/takewise/models")
}

fn doctor(dir: Option<&Path>) -> Outcome {
    let mut checks = vec![tool_check("whisper-cli", "brew install whisper-cpp")];
    let project = dir.map(Project::open);
    let model = match &project {
        Some(Ok(project)) => project.config.check.whisper_model.clone(),
        _ => CheckConfig::default().whisper_model,
    };
    let model_path = models_dir().join(&model);
    checks.push(Check {
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
    });
    checks.push(tool_check("ffprobe", "brew install ffmpeg"));
    match project {
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

    let mut text = String::new();
    for check in &checks {
        let mark = if check.ok { "ok  " } else { "FAIL" };
        let _ = writeln!(text, "{mark} {:<14} {}", check.name, check.detail);
    }
    text.push_str("Input devices are checked from milestone 2.\n");
    let failed: Vec<&str> = checks.iter().filter(|c| !c.ok).map(|c| c.name).collect();
    let outcome = Outcome::new(json!({ "checks": checks }), text);
    if failed.is_empty() {
        outcome
    } else {
        outcome.with_finding("doctor", format!("failed checks: {}", failed.join(", ")))
    }
}
