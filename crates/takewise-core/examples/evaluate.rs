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
//!
//! `evaluate --spellings` needs no audio or whisper: it checks a corpus of
//! lines against transcripts spelled the way whisper writes them (times,
//! amounts, hyphens, "all right", negative numbers, annotations over several
//! words) and fails when a verdict differs from the expected one.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use serde::Deserialize;
use takewise_core::check::{Transcript, build_checked};
use takewise_core::speech::Frames;
use takewise_core::whisper::{Transcriber as _, WhisperCli, keep_within};
use takewise_core::words::isolate_annotations;
use takewise_core::{
    Analysis, CheckConfig, Snapshot, Verdict, Word, analyze, checker, read_take_wav, write_take_wav,
};

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
        eprintln!("usage: evaluate <lines.toml> | evaluate --spellings");
        return ExitCode::from(2);
    };
    if path == "--spellings" {
        return spellings();
    }
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

/// Lines, what whisper writes for them, and the verdict the checks owe.
const SPELLINGS: &[(&str, &str, Verdict)] = &[
    // Times.
    ("Meet me at 9:30.", "Meet me at 9.30.", Verdict::Keep),
    ("Meet me at 9:30.", "Meet me at 9:30.", Verdict::Keep),
    ("Meet me at 9:30.", "Meet me at nine thirty.", Verdict::Keep),
    ("Meet me at nine thirty.", "Meet me at 9:30.", Verdict::Keep),
    ("Meet me at 9:30.", "Meet me at 9.35.", Verdict::Listen),
    ("Meet me at 10:00.", "Meet me at 10.00.", Verdict::Keep),
    ("Meet me at 10:00.", "Meet me at 10.", Verdict::Keep),
    (
        "Meet me at 10:00.",
        "Meet me at ten o'clock.",
        Verdict::Keep,
    ),
    (
        "Meet me at ten o'clock.",
        "Meet me at 10:00.",
        Verdict::Keep,
    ),
    ("Meet me at 10:00.", "Meet me at 11.00.", Verdict::Listen),
    // Amounts.
    (
        "It costs $3.50.",
        "It costs three dollars fifty.",
        Verdict::Keep,
    ),
    (
        "It costs three dollars fifty.",
        "It costs $3.50.",
        Verdict::Keep,
    ),
    (
        "It costs $3.50.",
        "It costs three dollars and fifty cents.",
        Verdict::Keep,
    ),
    // A number after an amount is its own unless the line has the cents.
    (
        "It cost five dollars twenty years ago.",
        "It cost $5 20 years ago.",
        Verdict::Keep,
    ),
    (
        "It weighed seven pounds eight ounces.",
        "It weighed 7 pounds 8 ounces.",
        Verdict::Keep,
    ),
    (
        "It costs $5.20.",
        "It costs five dollars twenty.",
        Verdict::Keep,
    ),
    // "Three fifty" may be 350: listen.
    ("It costs $3.50.", "It costs three fifty.", Verdict::Listen),
    // Hyphens.
    (
        "I recorded the voice-over.",
        "I recorded the voiceover.",
        Verdict::Keep,
    ),
    ("Send me an email.", "Send me an e-mail.", Verdict::Keep),
    ("Everyone came.", "Every one came.", Verdict::Listen),
    // All right.
    ("Alright, let's go.", "All right, let's go.", Verdict::Keep),
    ("All right, let's go.", "Alright, let's go.", Verdict::Keep),
    // Negative numbers.
    (
        "It was -5 degrees.",
        "It was minus five degrees.",
        Verdict::Keep,
    ),
    (
        "It was -5 degrees.",
        "It was negative five degrees.",
        Verdict::Keep,
    ),
    ("It was -5 degrees.", "It was 5 degrees.", Verdict::Listen),
    ("It lost -$5.", "It lost minus five dollars.", Verdict::Keep),
    ("It lost -$5.", "It lost $5.", Verdict::Listen),
    ("Ten minus five is five.", "10 - 5 is 5.", Verdict::Keep),
    ("Ten minus five is five.", "10 minus 5 is 5.", Verdict::Keep),
    // Annotations over several words: a sound, no leaked words.
    (
        "Nobody misses a birthday.",
        "Nobody [background music playing] misses a birthday.",
        Verdict::Listen,
    ),
    (
        "Nobody misses a birthday.",
        "Nobody misses a birthday. (soft music)",
        Verdict::Listen,
    ),
];

/// Speech from 1 s to 4 s of a 5 s take, sound throughout it.
fn synthetic_analysis() -> Analysis {
    let levels = (0..500)
        .map(|i| {
            if (100..400).contains(&i) {
                -20.0
            } else {
                -70.0
            }
        })
        .collect();
    Analysis {
        speech: Some((1.0, 4.0)),
        peak_dbfs: -6.0,
        clipped_samples: 0,
        noise_floor_dbfs: -70.0,
        frames: Frames {
            levels_dbfs: levels,
            threshold_dbfs: -55.0,
            frame_s: 0.01,
        },
    }
}

/// `heard`'s words spread evenly over the speech, made into whisper's words.
fn synthetic_words(heard: &str) -> Vec<Word> {
    let parts: Vec<&str> = heard.split_whitespace().collect();
    #[allow(clippy::cast_precision_loss)]
    let step = 3.0 / parts.len() as f64;
    let words = (0_u32..)
        .zip(&parts)
        .map(|(i, text)| Word {
            text: (*text).to_owned(),
            start: 1.0 + f64::from(i) * step,
            end: 1.0 + f64::from(i + 1) * step,
            confidence: Some(0.9),
        })
        .collect();
    isolate_annotations(words)
}

fn spellings() -> ExitCode {
    let analysis = synthetic_analysis();
    let mut failed = 0;
    println!("| script | heard | expected | verdict | reasons |");
    println!("|---|---|---|---|---|");
    for &(script, heard, expected) in SPELLINGS {
        let snapshot = Snapshot {
            text: script.to_owned(),
            start: 0.0,
            end: 6.0,
        };
        let transcript = Transcript::Words(synthetic_words(heard));
        let checked = build_checked(&snapshot, 0.0, &analysis, &transcript, "evaluate");
        let reasons: Vec<&str> = checked.reasons.iter().map(|r| r.message.as_str()).collect();
        let mark = if checked.verdict == expected {
            ""
        } else {
            failed += 1;
            " ✗"
        };
        println!(
            "| {script} | {heard} | {} | {}{mark} | {} |",
            expected.as_str(),
            checked.verdict.as_str(),
            if reasons.is_empty() {
                "–".to_owned()
            } else {
                reasons.join("; ")
            }
        );
    }
    if failed == 0 {
        println!("\nall {} verdicts as expected", SPELLINGS.len());
        ExitCode::SUCCESS
    } else {
        eprintln!("{failed} of {} verdicts differ", SPELLINGS.len());
        ExitCode::FAILURE
    }
}
