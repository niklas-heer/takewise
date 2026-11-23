//! Runs the take checks on recorded narration, read-only, to tune them on
//! real takes: `cargo run --release -p takewise-core --example evaluate --
//! <lines.toml>`.
//!
//! The TOML names WAV files (relative to `dir`, or absolute) and the line
//! each one should say; `from` and `to` cut a part out of a longer file
//! (seconds), so one full take can be checked phrase by phrase:
//!
//! ```toml
//! dir = "/path/to/voice"
//! preroll_s = 0.3            # optional: the noise floor is measured here
//!
//! [[line]]
//! file = "line-1.wav"
//! text = "I put our whole family tree together."
//!
//! [[line]]
//! file = "raw/take.wav"
//! from = 1.5
//! to = 8.0
//! text = "…"
//! ```
//!
//! Each line is checked with `line_offset_s = 0` and a slot one second longer
//! than the audio, so only the word and sound rules decide; the early-start
//! rule is shown separately as the breath lead (energy start to first word).
//! Audio is read where it is; a cut part is written to a temporary file that
//! is removed afterwards. Nothing is written to a project.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use serde::Deserialize;
use takewise_core::check::{Transcript, build_checked};
use takewise_core::whisper::{Transcriber as _, WhisperCli, keep_within};
use takewise_core::{CheckConfig, Snapshot, analyze, checker, read_take_wav, write_take_wav};

#[derive(Deserialize)]
struct Lines {
    #[serde(default)]
    dir: PathBuf,
    #[serde(default = "default_preroll")]
    preroll_s: f64,
    line: Vec<Line>,
}

#[derive(Deserialize)]
struct Line {
    file: PathBuf,
    text: String,
    from: Option<f64>,
    to: Option<f64>,
}

fn default_preroll() -> f64 {
    0.3
}

fn main() -> ExitCode {
    let Some(path) = std::env::args_os().nth(1) else {
        eprintln!("usage: evaluate <lines.toml>");
        return ExitCode::from(2);
    };
    match run(Path::new(&path)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(2)
        }
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn sample_at(seconds: f64, rate: u32) -> usize {
    (seconds * f64::from(rate)).round().max(0.0) as usize
}

fn run(path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let lines: Lines = toml::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
    let whisper = WhisperCli::discover(&CheckConfig::default()).map_err(|err| err.to_string())?;
    let checker_name = checker("evaluate", &whisper.model_name());
    let tmp = tempfile::tempdir().map_err(|err| err.to_string())?;

    println!("| # | verdict | reasons | speech (s) | breath lead (s) | transcribe (s) | heard |");
    println!("|---|---|---|---|---|---|---|");
    for (n, line) in lines.line.iter().enumerate() {
        let source = lines.dir.join(&line.file);
        let (mut samples, rate) =
            read_take_wav(&source).map_err(|err| format!("{}: {err}", source.display()))?;
        let mut wav = source.clone();
        if line.from.is_some() || line.to.is_some() {
            let from = sample_at(line.from.unwrap_or(0.0), rate).min(samples.len());
            let to = line
                .to
                .map_or(samples.len(), |to| sample_at(to, rate))
                .clamp(from, samples.len());
            samples = samples[from..to].to_vec();
            wav = tmp.path().join(format!("part-{}.wav", n + 1));
            write_take_wav(&wav, &samples, rate).map_err(|err| err.to_string())?;
        }
        let length = samples.len() as f64 / f64::from(rate);
        let analysis = analyze(&samples, rate, lines.preroll_s);
        let began = Instant::now();
        let transcript = match whisper.transcribe(&wav) {
            Transcript::Words(words) => Transcript::Words(keep_within(words, analysis.speech)),
            failed @ Transcript::Failed(_) => failed,
        };
        let seconds = began.elapsed().as_secs_f64();
        let snapshot = Snapshot {
            text: line.text.clone(),
            start: 0.0,
            end: length + 1.0,
        };
        let checked = build_checked(&snapshot, 0.0, &analysis, &transcript, &checker_name);
        let reasons: Vec<String> = checked
            .reasons
            .iter()
            .map(|r| format!("{}: {}", r.code, r.message))
            .collect();
        let speech = analysis
            .speech
            .map_or_else(|| "–".to_owned(), |(a, b)| format!("{a:.2}–{b:.2}"));
        let (heard, lead) = match &transcript {
            Transcript::Words(words) => (
                words
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" "),
                match (analysis.speech, words.first()) {
                    (Some((start, _)), Some(first)) => format!("{:.2}", first.start - start),
                    _ => "–".to_owned(),
                },
            ),
            Transcript::Failed(err) => (format!("(failed: {err})"), "–".to_owned()),
        };
        println!(
            "| {} | {} | {} | {speech} | {lead} | {seconds:.1} | {heard} |",
            n + 1,
            checked.verdict.as_str(),
            if reasons.is_empty() {
                "–".to_owned()
            } else {
                reasons.join("; ")
            },
        );
    }
    Ok(())
}
