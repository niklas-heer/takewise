//! Export: one narration WAV aligned to the film's first frame, and a manifest.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result, io};
use crate::fsutil::{lock, rename_temp, write_temp};
use crate::ledger::{EventKind, Ledger, Verdict, read};
use crate::project::Project;
use crate::script::{Line, validate};
use crate::speech::analyze;
use crate::state::{State, fold};
use crate::takes::{encode_wav, read_take_wav};

pub const EXPORT_DIR: &str = "export";
pub const NARRATION_FILE: &str = "narration.wav";
pub const MANIFEST_FILE: &str = "manifest.json";
/// Audio kept on either side of the detected speech.
const MARGIN_S: f64 = 0.150;
const FADE_S: f64 = 0.010;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportOptions {
    /// Use the proposed take for lines without a chosen one.
    pub use_proposed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ExportReport {
    pub lines: Vec<ManifestLine>,
    /// Why nothing was written; empty on success.
    pub refused: Vec<Refusal>,
    /// Exported, but worth a look; also listed in the manifest.
    pub warnings: Vec<ExportWarning>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportWarning {
    pub code: &'static str,
    pub line: String,
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
    /// Half-open sample range taken from the take.
    pub source_samples: [u64; 2],
    /// Half-open sample range in `narration.wav`.
    pub dest_samples: [u64; 2],
    /// Detected speech start and end in film seconds, clamped to the film;
    /// a `speech-cropped` warning gives the unclamped values.
    pub speech_film_s: [f64; 2],
    /// The transcribed words in film seconds, from the take's latest check
    /// against the line as it is now; absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub words_film_s: Option<Vec<ManifestWord>>,
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
    /// The WAV this manifest describes; a mismatch means the pair is torn.
    narration: ManifestNarration,
    sample_rate: u32,
    lines: &'a [ManifestLine],
    warnings: &'a [ExportWarning],
}

#[derive(Serialize)]
struct ManifestNarration {
    /// Relative to the manifest.
    file: &'static str,
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
    samples: Vec<i32>,
    /// Source start and destination start and end, after cropping.
    src: usize,
    dest: (usize, usize),
    /// The take was recorded against different text than the line has now.
    old_text: bool,
    /// The unclamped speech in film seconds, when part of it lies outside
    /// the film.
    cropped_speech: Option<(f64, f64)>,
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

/// Writes `export/narration.wav` (film length, mono 24-bit) and
/// `export/manifest.json` from each line's chosen take. The project, script
/// and ledger are read and folded under the project lock, so the pair
/// reflects the state at write time. On refusal it writes nothing and removes
/// an earlier manifest, so no stale pair looks valid; the export is
/// regenerable and takes are untouched.
pub fn export(dir: &Path, opts: &ExportOptions) -> Result<ExportReport> {
    let _guard = lock(dir)?;
    let project = Project::open(dir)?;
    let ledger = read(dir)?;
    let state = fold(&project, &ledger);
    export_locked(&project, &ledger, &state, opts.use_proposed)
}

fn export_locked(
    project: &Project,
    ledger: &Ledger,
    state: &State,
    use_proposed: bool,
) -> Result<ExportReport> {
    let dir = project.dir.join(EXPORT_DIR);
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
    refused.extend(overlaps(&placed));
    if !refused.is_empty() {
        remove_if_present(&dir.join(MANIFEST_FILE))?;
        return Ok(ExportReport {
            lines: Vec::new(),
            refused,
            warnings: Vec::new(),
        });
    }

    let rate = project.config.sample_rate;
    let film_len = to_usize(seconds_to_samples(project.film_duration(), rate));
    let out = mix(&placed, film_len, rate);
    fs::create_dir_all(&dir).map_err(io(&dir))?;
    let wav_path = dir.join(NARRATION_FILE);
    let wav_temp = write_temp(&wav_path, |file| {
        let copy = file.try_clone()?;
        encode_wav(copy, &out, rate)
            .map(drop)
            .map_err(|err| std::io::Error::other(err.to_string()))
    })?;
    let narration = match describe_wav(&wav_temp) {
        Ok(narration) => narration,
        Err(err) => {
            let _ = fs::remove_file(&wav_temp);
            return Err(io(&wav_path)(err));
        }
    };

    let warnings = warnings(&placed, project.film_duration());
    let lines: Vec<ManifestLine> = placed.into_iter().map(|p| p.line).collect();
    let info = &project.config.film_info;
    let manifest = Manifest {
        v: 1,
        film: ManifestFilm {
            path: project.film_path().display().to_string(),
            duration_s: info.duration_s,
            size: info.size,
            modified: &info.modified,
        },
        narration,
        sample_rate: rate,
        lines: &lines,
        warnings: &warnings,
    };
    let manifest_path = dir.join(MANIFEST_FILE);
    let manifest_temp = match write_manifest(&manifest_path, &manifest) {
        Ok(temp) => temp,
        Err(err) => {
            let _ = fs::remove_file(&wav_temp);
            return Err(err);
        }
    };
    // The old manifest goes first, then the WAV, the manifest last: an
    // interruption anywhere leaves no manifest, or one whose hash does not
    // match the WAV, but never an old manifest vouching for a changed export.
    if let Err(err) = remove_if_present(&manifest_path) {
        let _ = fs::remove_file(&wav_temp);
        let _ = fs::remove_file(&manifest_temp);
        return Err(err);
    }
    if let Err(err) = rename_temp(&wav_temp, &wav_path) {
        let _ = fs::remove_file(&manifest_temp);
        return Err(err);
    }
    rename_temp(&manifest_temp, &manifest_path)?;
    Ok(ExportReport {
        lines,
        refused: Vec::new(),
        warnings,
    })
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
    let (speech_start, speech_end) = analyze(&samples, rate, project.config.record.preroll_s)
        .speech
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
            words_film_s: words_film_s(project, ledger, line, take_no, take_start_film),
        },
        samples,
        src: src0,
        dest: (dest0, dest1),
        old_text: take.recorded_against_old_text,
        cropped_speech,
    })
}

/// The words of the take's latest check against the line as it is now, in
/// film seconds; `None` without such a check or without a transcript.
fn words_film_s(
    project: &Project,
    ledger: &Ledger,
    line: &Line,
    take: u32,
    take_start_film: f64,
) -> Option<Vec<ManifestWord>> {
    let current = project.snapshot(&line.id)?;
    let checked = ledger
        .events
        .iter()
        .rev()
        .filter(|e| e.line == line.id && e.take == take)
        .find_map(|e| match &e.kind {
            EventKind::Checked(checked) if checked.snapshot.matches(&current) => Some(checked),
            _ => None,
        })?;
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
                line: p.line.id.clone(),
                message: format!(
                    "{} was recorded against different text than the line has now",
                    p.line.take_file
                ),
            });
        }
        if let Some((start, end)) = p.cropped_speech {
            warnings.push(ExportWarning {
                code: "speech-cropped",
                line: p.line.id.clone(),
                message: format!(
                    "the speech of {} runs from {start:.3} s to {end:.3} s, partly outside the film (0 s to {film_duration:.3} s); the part outside is cropped",
                    p.line.take_file
                ),
            });
        }
    }
    warnings
}

fn overlaps(placed: &[Placed]) -> Vec<Refusal> {
    let mut refused = Vec::new();
    for (i, a) in placed.iter().enumerate() {
        for b in placed.iter().skip(i + 1) {
            if a.dest.0 < b.dest.1 && b.dest.0 < a.dest.1 {
                refused.push(Refusal {
                    code: "overlap",
                    lines: vec![a.line.id.clone(), b.line.id.clone()],
                    message: format!(
                        "{} and {} overlap in the film; choose another take or move a start",
                        a.line.id, b.line.id
                    ),
                });
            }
        }
    }
    refused
}

/// Copies each placed range into a silent film-length buffer with linear
/// fades at both ends.
fn mix(placed: &[Placed], film_len: usize, rate: u32) -> Vec<i32> {
    let mut out = vec![0i32; film_len];
    let fade = to_usize(seconds_to_samples(FADE_S, rate)).max(1);
    for p in placed {
        let len = p.dest.1 - p.dest.0;
        let ramp = fade.min(len / 2).max(1);
        let source = p.samples.iter().skip(p.src).take(len);
        for (k, (slot, &sample)) in out[p.dest.0..p.dest.1].iter_mut().zip(source).enumerate() {
            let edge = k.min(len - 1 - k);
            *slot = if edge < ramp {
                scale(sample, edge as f64 / ramp as f64)
            } else {
                sample
            };
        }
    }
    out
}

#[allow(clippy::cast_possible_truncation)]
fn scale(sample: i32, gain: f64) -> i32 {
    (f64::from(sample) * gain).round() as i32
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

fn describe_wav(path: &Path) -> std::io::Result<ManifestNarration> {
    let bytes = fs::read(path)?;
    let sha256 = Sha256::digest(&bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    Ok(ManifestNarration {
        file: NARRATION_FILE,
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
        let project = Project::open(dir).unwrap();
        let input = RecordedInput {
            sample_rate: RATE,
            device: "Test Mic".to_owned(),
            channel: 1,
            line_offset_s: -2.0,
            snapshot: project.snapshot(line).unwrap(),
            dropouts: None,
            film_shift_ms: None,
        };
        let partial = begin_take(dir, line, &input).unwrap();
        let mut audio = noise(5.0);
        if let Some((from, to)) = speech {
            burst(&mut audio, from, to);
        }
        write_take_wav(&partial.wav, &audio, RATE).unwrap();
        commit_take(dir, partial).unwrap()
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

    fn run(dir: &Path, use_proposed: bool) -> ExportReport {
        export(dir, &ExportOptions { use_proposed }).unwrap()
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
            (warning.code, warning.line.as_str()),
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
        assert_eq!(report.warnings[0].line, "hook");
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
        let result = export(&dir, &ExportOptions::default());
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
        // Outside the placed range the export is silent.
        let audio = narration(&dir);
        assert_eq!(audio[472_799], 0);
        assert_eq!(audio[535_200], 0);
        // 10 ms fades: the first placed sample is silent and the level ramps up.
        assert_eq!(audio[472_800], 0);
    }
}
