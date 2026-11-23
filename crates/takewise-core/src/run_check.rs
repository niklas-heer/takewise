//! Running the checks on a recorded take and recording the result.
//!
//! [`check_take`] reads the take and its `recorded` event, takes the line's
//! snapshot from the script as it is when the check starts, runs the energy
//! analysis and the transcriber, applies the verdict rules and appends the
//! `checked` event under the project lock. A script edited while the check
//! runs leaves the event with the older snapshot, so the fold shows it as
//! stale instead of it vouching for text it never saw.

use std::path::Path;

use serde::Serialize;

use crate::check::{Transcript, build_checked};
use crate::error::{Error, Result};
use crate::ledger::{self, Checked, Event, EventKind, Ledger, Snapshot};
use crate::project::Project;
use crate::speech::analyze;
use crate::takes::{TakeRef, read_take_wav};
use crate::whisper::{Transcriber, keep_within};

/// The `checker` field of a `checked` event: the Takewise version and the
/// whisper model, such as `takewise 0.1.0 whisper ggml-large-v3-turbo.bin`.
pub fn checker(version: &str, model: &str) -> String {
    format!("takewise {version} whisper {model}")
}

/// Checks one recorded take against its line as the script has it now and
/// appends the `checked` event. `version` is the Takewise version for the
/// `checker` field. Transcription failures are a `listen` verdict, never an
/// error; errors are an unknown line or take and an unreadable take file.
pub fn check_take(
    dir: &Path,
    take: &TakeRef,
    transcriber: &dyn Transcriber,
    version: &str,
) -> Result<Checked> {
    let project = Project::open(dir)?;
    let snapshot = project
        .snapshot(&take.line)
        .ok_or_else(|| Error::UnknownLine(take.line.clone()))?;
    let ledger = ledger::read(dir)?;
    let recorded = ledger
        .recorded(&take.line, take.take)
        .ok_or_else(|| Error::UnknownTake {
            line: take.line.clone(),
            take: take.take,
        })?;
    let wav = dir.join(&recorded.file);
    let (samples, rate) = read_take_wav(&wav)?;
    let analysis = analyze(&samples, rate, project.config.record.preroll_s);
    let transcript = match transcriber.transcribe(&wav) {
        Transcript::Words(words) => Transcript::Words(keep_within(words, analysis.speech)),
        failed @ Transcript::Failed(_) => failed,
    };
    let checked = build_checked(
        &snapshot,
        recorded.line_offset_s,
        &analysis,
        &transcript,
        &checker(version, &transcriber.model()),
    );
    let event = Event::new(&take.line, take.take, EventKind::Checked(checked.clone()));
    ledger::append(dir, &event)?;
    Ok(checked)
}

/// A take whose effective check is missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unchecked {
    pub take: TakeRef,
    /// It was checked before, against another text or slot.
    pub stale: bool,
}

/// The takes of the script's lines that have no `checked` event matching
/// the line as it is now, in script and take order. Takes whose file is
/// missing are left out: there is nothing to check.
pub fn unchecked_takes(project: &Project, ledger: &Ledger) -> Vec<Unchecked> {
    let mut found = Vec::new();
    for line in &project.script.lines {
        let Some(current) = project.snapshot(&line.id) else {
            continue;
        };
        let mut takes: Vec<(u32, &str)> = ledger
            .events
            .iter()
            .filter(|e| e.line == line.id)
            .filter_map(|e| match &e.kind {
                EventKind::Recorded(rec) => Some((e.take, rec.file.as_str())),
                _ => None,
            })
            .collect();
        takes.sort_by_key(|(take, _)| *take);
        takes.dedup_by_key(|(take, _)| *take);
        for (take, file) in takes {
            if !project.dir.join(file).is_file() {
                continue;
            }
            let checks: Vec<&Snapshot> = ledger
                .events
                .iter()
                .filter(|e| e.line == line.id && e.take == take)
                .filter_map(|e| match &e.kind {
                    EventKind::Checked(checked) => Some(&checked.snapshot),
                    _ => None,
                })
                .collect();
            if checks.iter().any(|snapshot| snapshot.matches(&current)) {
                continue;
            }
            found.push(Unchecked {
                take: TakeRef {
                    line: line.id.clone(),
                    take,
                },
                stale: !checks.is_empty(),
            });
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use super::*;
    use crate::edit::{Field, set_line_field};
    use crate::ledger::{Verdict, Word, read};
    use crate::project::FilmInfo;
    use crate::state::fold;
    use crate::takes::{RecordedInput, begin_take, commit_take, write_take_wav};
    use crate::testutil::{RATE, burst, noise};

    const SCRIPT: &str =
        "[[line]]\nid = \"hook\"\nstart = 10.0\nend = 13.0\ntext = \"Nobody misses a birthday.\"\n";

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("p.takewise");
        let info = FilmInfo {
            duration_s: 51.5,
            size: 1,
            modified: "2026-11-22T20:05:11Z".to_owned(),
        };
        Project::init(&dir, Path::new("film.mp4"), info).unwrap();
        fs::write(dir.join("script.toml"), SCRIPT).unwrap();
        (tmp, dir)
    }

    /// A 5 s take from 2 s before the line with speech at 2.0..3.5 s.
    fn take(dir: &Path) -> TakeRef {
        let project = Project::open(dir).unwrap();
        let input = RecordedInput {
            sample_rate: RATE,
            device: "Test Mic".to_owned(),
            channel: 1,
            line_offset_s: -2.0,
            snapshot: project.snapshot("hook").unwrap(),
            dropouts: None,
            film_shift_ms: None,
        };
        let partial = begin_take(dir, "hook", &input).unwrap();
        let mut audio = noise(5.0);
        burst(&mut audio, 2.0, 3.5);
        write_take_wav(&partial.wav, &audio, RATE).unwrap();
        commit_take(dir, partial).unwrap()
    }

    fn word(text: &str, start: f64) -> Word {
        Word {
            text: text.to_owned(),
            start,
            end: start + 0.3,
            confidence: Some(0.9),
        }
    }

    /// Hears the line on time, plus "Thank you." in the silence after it;
    /// runs `during` while "transcribing".
    struct Fake {
        during: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                during: Mutex::new(None),
            }
        }

        fn during(f: impl FnOnce() + Send + 'static) -> Self {
            Self {
                during: Mutex::new(Some(Box::new(f))),
            }
        }
    }

    impl Transcriber for Fake {
        fn transcribe(&self, _: &Path) -> Transcript {
            if let Some(f) = self.during.lock().unwrap().take() {
                f();
            }
            Transcript::Words(vec![
                word("Nobody", 2.1),
                word("misses", 2.5),
                word("a", 2.8),
                word("birthday.", 3.0),
                word("Thank", 4.6),
                word("you.", 4.8),
            ])
        }
        fn model(&self) -> String {
            "ggml-fake.bin".to_owned()
        }
    }

    fn checked_events(dir: &Path) -> Vec<Checked> {
        read(dir)
            .unwrap()
            .events
            .into_iter()
            .filter_map(|e| match e.kind {
                EventKind::Checked(checked) => Some(checked),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn check_appends_checked_event() {
        let (_tmp, dir) = setup();
        let t = take(&dir);
        let checked = check_take(&dir, &t, &Fake::new(), "0.1.0").unwrap();
        // The hallucinated "Thank you." lies beyond the speech and is dropped.
        assert_eq!(checked.verdict, Verdict::Keep, "{:?}", checked.reasons);
        assert_eq!(checked.transcript.as_ref().map(Vec::len), Some(4));
        assert_eq!(checked.checker, "takewise 0.1.0 whisper ggml-fake.bin");
        assert_eq!(checked.snapshot.text, "Nobody misses a birthday.");
        let (start, end) = checked.speech.unwrap();
        assert!((start - 2.0).abs() < 0.02 && (end - 3.5).abs() < 0.02);

        assert_eq!(checked_events(&dir), vec![checked]);
        let project = Project::open(&dir).unwrap();
        let state = fold(&project, &read(&dir).unwrap());
        let take = &state.lines[0].takes[0];
        assert_eq!(take.effective, Some(Verdict::Keep));
        assert!(!take.stale_check);
        assert_eq!(unchecked_takes(&project, &read(&dir).unwrap()), []);
    }

    #[test]
    fn check_without_transcription_listens() {
        struct Broken;
        impl Transcriber for Broken {
            fn transcribe(&self, _: &Path) -> Transcript {
                Transcript::Failed("whisper-cli exited with status 1".to_owned())
            }
            fn model(&self) -> String {
                "ggml-large-v3-turbo.bin".to_owned()
            }
        }
        let (_tmp, dir) = setup();
        let t = take(&dir);
        let checked = check_take(&dir, &t, &Broken, "0.1.0").unwrap();
        assert_eq!(checked.verdict, Verdict::Listen);
        assert_eq!(checked.reasons[0].code, "words-not-checked");
        assert!(checked.reasons[0].message.contains("status 1"));
    }

    #[test]
    fn check_racing_an_edit_is_stale() {
        let (_tmp, dir) = setup();
        let t = take(&dir);
        let edit_dir = dir.clone();
        let fake = Fake::during(move || {
            set_line_field(
                &edit_dir,
                "hook",
                Field::Text("Nobody forgets a birthday.".to_owned()),
                None,
            )
            .unwrap();
        });
        let checked = check_take(&dir, &t, &fake, "0.1.0").unwrap();
        // Checked against the text it started with, not the edit.
        assert_eq!(checked.snapshot.text, "Nobody misses a birthday.");
        assert_eq!(checked.verdict, Verdict::Keep);

        let project = Project::open(&dir).unwrap();
        let ledger = read(&dir).unwrap();
        let take = &fold(&project, &ledger).lines[0].takes[0];
        assert!(take.stale_check);
        assert_eq!(take.effective, None, "a stale check does not count");
        assert_eq!(
            unchecked_takes(&project, &ledger),
            vec![Unchecked {
                take: t,
                stale: true
            }]
        );
    }

    #[test]
    fn check_of_an_unknown_take_fails() {
        let (_tmp, dir) = setup();
        let missing = TakeRef {
            line: "hook".to_owned(),
            take: 7,
        };
        let err = check_take(&dir, &missing, &Fake::new(), "0.1.0").unwrap_err();
        assert_eq!(err.code(), "unknown-take");
        assert_eq!(checked_events(&dir), []);
    }

    #[test]
    fn unchecked_takes_skips_missing_files_and_matching_checks() {
        let (_tmp, dir) = setup();
        let first = take(&dir);
        let second = take(&dir);
        let third = take(&dir);
        check_take(&dir, &second, &Fake::new(), "0.1.0").unwrap();
        fs::remove_file(dir.join("takes/hook/003.wav")).unwrap();
        let project = Project::open(&dir).unwrap();
        let unchecked = unchecked_takes(&project, &read(&dir).unwrap());
        assert_eq!(
            unchecked,
            vec![Unchecked {
                take: first,
                stale: false
            }]
        );
        let _ = third;
    }
}
