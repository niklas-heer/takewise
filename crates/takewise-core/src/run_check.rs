//! Running the checks on a recorded take and recording the result.
//!
//! [`check_take`] reads the take and its `recorded` event, takes the line's
//! snapshot from the script as it is when the check starts, runs the energy
//! analysis and the transcriber, applies the verdict rules and appends the
//! `checked` event under the project lock. A script edited while the check
//! runs leaves the event with the older snapshot, so the fold shows it as
//! stale instead of it vouching for text it never saw.
//!
//! When only the line's slot moved since an earlier check of the take (the
//! same text, transcribed by the same model with the same preprocessing),
//! the verdict is recomputed from that check's words and a fresh energy
//! analysis, without transcribing again.

use std::path::Path;

use serde::Serialize;

use crate::check::{Transcript, build_checked, rebuild_checked, words_not_checked};
use crate::error::{Error, Result};
use crate::ledger::{self, Checked, Event, EventKind, Ledger, Snapshot};
use crate::project::Project;
use crate::speech::analyze;
use crate::takes::{TakeRef, read_take_wav};
use crate::whisper::{PREPROCESSING, Transcriber, WhisperCli, keep_within};

/// The `checker` field of a `checked` event: the Takewise version, the
/// whisper model and how the audio is prepared before transcription (see
/// [`PREPROCESSING`]), such as `takewise 0.1.0 whisper
/// ggml-large-v3-turbo.bin pre=hp60+norm`.
pub fn checker(version: &str, model: &str) -> String {
    format!("takewise {version} whisper {model} pre={PREPROCESSING}")
}

/// What a check's words depend on, from its `checker`: the model and the
/// preprocessing, `none` for a checker from before it was named.
fn transcription_key(checker: &str) -> Option<(&str, &str)> {
    let (_, rest) = checker.split_once(" whisper ")?;
    Some(match rest.split_once(" pre=") {
        Some((model, pre)) => (model, pre),
        None => (rest, "none"),
    })
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
    let by = checker(version, &transcriber.model());
    let reused = moved_slot_check(&ledger, take, &snapshot, &by).and_then(|previous| {
        rebuild_checked(&snapshot, recorded.line_offset_s, &analysis, previous, &by)
    });
    let checked = reused.unwrap_or_else(|| {
        let transcript = match transcriber.transcribe(&wav) {
            Transcript::Words(words) => Transcript::Words(keep_within(words, analysis.speech)),
            failed @ Transcript::Failed(_) => failed,
        };
        build_checked(
            &snapshot,
            recorded.line_offset_s,
            &analysis,
            &transcript,
            &by,
        )
    });
    // A failure exactly like the take's newest check adds nothing: it is
    // reported, not recorded again.
    let repeated = checked.transcript.is_none() && latest_check(&ledger, take) == Some(&checked);
    if !repeated {
        let event = Event::new(&take.line, take.take, EventKind::Checked(checked.clone()));
        ledger::append(dir, &event)?;
    }
    Ok(checked)
}

/// The take's newest `checked` event, which the fold shows.
fn latest_check<'a>(ledger: &'a Ledger, take: &TakeRef) -> Option<&'a Checked> {
    ledger
        .events
        .iter()
        .rev()
        .filter(|e| e.line == take.line && e.take == take.take)
        .find_map(|e| match &e.kind {
            EventKind::Checked(check) => Some(check),
            _ => None,
        })
}

/// The take's latest check whose words can stand for a check against
/// `snapshot`: the same text against another slot, with words transcribed
/// the way the checker `by` would. `None` when a check against `snapshot`
/// exists (checking it again transcribes again) or there is no such check.
fn moved_slot_check<'a>(
    ledger: &'a Ledger,
    take: &TakeRef,
    snapshot: &Snapshot,
    by: &str,
) -> Option<&'a Checked> {
    let key = transcription_key(by)?;
    let checks: Vec<&Checked> = ledger
        .events
        .iter()
        .filter(|e| e.line == take.line && e.take == take.take)
        .filter_map(|e| match &e.kind {
            EventKind::Checked(check) => Some(check),
            _ => None,
        })
        .collect();
    if checks.iter().any(|c| c.snapshot.matches(snapshot)) {
        return None;
    }
    checks.into_iter().rev().find(|c| {
        c.snapshot.text == snapshot.text
            && c.transcript.is_some()
            && transcription_key(&c.checker) == Some(key)
    })
}

/// A take without a current check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unchecked {
    pub take: TakeRef,
    /// It was checked before, but against another text or slot, or its
    /// words were transcribed by another model or preprocessing.
    pub stale: bool,
}

/// The takes of the script's lines without a current check, in script and
/// take order. A take's check is the newest `checked` event matching the
/// line as it is now, as the fold shows it; it is current when its words
/// were checked by the configured model with the current preprocessing
/// ([`PREPROCESSING`]). One checked by another model or preprocessing is
/// listed (as stale) only while transcription can be set up, so a missing
/// model never turns its words into a failure. One whose transcription
/// failed (`words-not-checked`) is
/// listed only when checking again could end differently: transcription
/// can be set up now, it failed for another reason than the setup fails
/// for now, or it timed out. Takes whose file is missing are left out:
/// there is nothing to check.
pub fn unchecked_takes(project: &Project, ledger: &Ledger) -> Vec<Unchecked> {
    let setup = WhisperCli::discover(&project.config.check).err();
    select_unchecked(
        project,
        ledger,
        setup.map(|err| err.to_string()).as_deref(),
        (&project.config.check.whisper_model, PREPROCESSING),
    )
}

/// Whether checking a take again could change its failed check: `setup` is
/// why transcription cannot be set up now (`None` when it can).
fn failure_could_change(failed: &Checked, setup: Option<&str>) -> bool {
    let timed_out = failed
        .reasons
        .iter()
        .any(|r| r.code == "words-not-checked" && r.message.contains(" timed out after "));
    match setup {
        None => true,
        Some(error) => timed_out || !failed.reasons.contains(&words_not_checked(error)),
    }
}

/// The takes of the script's lines (with their file on disk) whose check
/// against the line as it is now failed to transcribe (`words-not-checked`),
/// in script and take order. When [`unchecked_takes`] leaves them out,
/// transcription cannot be set up, and that is what to report.
pub fn words_not_checked_takes(project: &Project, ledger: &Ledger) -> Vec<TakeRef> {
    let mut found = Vec::new();
    for line in &project.script.lines {
        let Some(current) = project.snapshot(&line.id) else {
            continue;
        };
        let mut takes: Vec<TakeRef> = Vec::new();
        for event in ledger.events.iter().filter(|e| e.line == line.id) {
            let EventKind::Recorded(rec) = &event.kind else {
                continue;
            };
            if !project.dir.join(&rec.file).is_file() {
                continue;
            }
            let newest = ledger
                .events
                .iter()
                .rev()
                .filter(|e| e.line == line.id && e.take == event.take)
                .find_map(|e| match &e.kind {
                    EventKind::Checked(c) if c.snapshot.matches(&current) => Some(c),
                    _ => None,
                });
            if newest.is_some_and(|c| c.transcript.is_none()) {
                takes.push(TakeRef {
                    line: line.id.clone(),
                    take: event.take,
                });
            }
        }
        takes.sort();
        takes.dedup();
        found.extend(takes);
    }
    found
}

/// [`unchecked_takes`] with why transcription cannot be set up now, `None`
/// when it can, and the current transcription key (model, preprocessing).
fn select_unchecked(
    project: &Project,
    ledger: &Ledger,
    setup: Option<&str>,
    current_key: (&str, &str),
) -> Vec<Unchecked> {
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
            let checks: Vec<&Checked> = ledger
                .events
                .iter()
                .filter(|e| e.line == line.id && e.take == take)
                .filter_map(|e| match &e.kind {
                    EventKind::Checked(checked) => Some(checked),
                    _ => None,
                })
                .collect();
            let newest = checks.iter().rev().find(|c| c.snapshot.matches(&current));
            let outdated = newest.is_some_and(|c| {
                c.transcript.is_some()
                    && setup.is_none()
                    && transcription_key(&c.checker) != Some(current_key)
            });
            // A failed transcription leaves `transcript` empty.
            let current_enough =
                newest.is_some_and(|c| c.transcript.is_some() || !failure_could_change(c, setup));
            if current_enough && !outdated {
                continue;
            }
            found.push(Unchecked {
                take: TakeRef {
                    line: line.id.clone(),
                    take,
                },
                stale: outdated || (!checks.is_empty() && newest.is_none()),
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

    /// The transcription key of [`Fake`] and [`Counting`]'s default model.
    const FAKE: (&str, &str) = ("ggml-fake.bin", PREPROCESSING);

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
    fn checker_tag_is_hp60_norm() {
        // Whisper hears a high-passed, normalized copy; checks made before
        // (`pre=none`) are transcribed again.
        assert_eq!(
            checker("0.1.0", "ggml-large-v3-turbo.bin"),
            "takewise 0.1.0 whisper ggml-large-v3-turbo.bin pre=hp60+norm"
        );
        assert_ne!(
            transcription_key(&checker("0.1.0", "ggml-a.bin")),
            transcription_key("takewise 0.1.0 whisper ggml-a.bin pre=none")
        );
    }

    #[test]
    fn check_appends_checked_event() {
        let (_tmp, dir) = setup();
        let t = take(&dir);
        let checked = check_take(&dir, &t, &Fake::new(), "0.1.0").unwrap();
        // The hallucinated "Thank you." lies beyond the speech and is dropped.
        assert_eq!(checked.verdict, Verdict::Keep, "{:?}", checked.reasons);
        assert_eq!(checked.transcript.as_ref().map(Vec::len), Some(4));
        assert_eq!(
            checked.checker,
            "takewise 0.1.0 whisper ggml-fake.bin pre=hp60+norm"
        );
        assert_eq!(checked.snapshot.text, "Nobody misses a birthday.");
        let (start, end) = checked.speech.unwrap();
        assert!((start - 2.0).abs() < 0.02 && (end - 3.5).abs() < 0.02);

        assert_eq!(checked_events(&dir), vec![checked]);
        let project = Project::open(&dir).unwrap();
        let state = fold(&project, &read(&dir).unwrap());
        let take = &state.lines[0].takes[0];
        assert_eq!(take.effective, Some(Verdict::Keep));
        assert!(!take.stale_check);
        assert_eq!(
            select_unchecked(&project, &read(&dir).unwrap(), None, FAKE),
            []
        );
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

        // Such a check is not current: with transcription set up, the take
        // is checked again.
        let project = Project::open(&dir).unwrap();
        assert_eq!(
            select_unchecked(&project, &read(&dir).unwrap(), None, FAKE),
            vec![Unchecked {
                take: t.clone(),
                stale: false
            }]
        );
        check_take(&dir, &t, &Fake::new(), "0.1.0").unwrap();
        assert_eq!(
            select_unchecked(&project, &read(&dir).unwrap(), None, FAKE),
            []
        );
    }

    /// Fails every transcription with `reason`.
    struct Failing(String);

    impl Transcriber for Failing {
        fn transcribe(&self, _: &Path) -> Transcript {
            Transcript::Failed(self.0.clone())
        }
        fn model(&self) -> String {
            "ggml-large-v3-turbo.bin".to_owned()
        }
    }

    #[test]
    fn a_failed_check_is_rerun_only_when_the_failure_could_change() {
        let missing = "whisper-cli not found on PATH or in /opt/homebrew/bin; install it with `brew install whisper-cpp`";
        let (_tmp, dir) = setup();
        let t = take(&dir);
        check_take(&dir, &t, &Failing(missing.to_owned()), "0.1.0").unwrap();
        let project = Project::open(&dir).unwrap();
        let ledger = read(&dir).unwrap();
        let listed = vec![Unchecked {
            take: t.clone(),
            stale: false,
        }];
        // The tools are still missing: checking again would fail the same way.
        assert_eq!(select_unchecked(&project, &ledger, Some(missing), FAKE), []);
        // Now set up, or failing for another reason: checked again.
        assert_eq!(select_unchecked(&project, &ledger, None, FAKE), listed);
        assert_eq!(
            select_unchecked(&project, &ledger, Some("whisper model x not found"), FAKE),
            listed
        );

        // A timeout may pass, whatever the setup.
        let (_tmp, dir) = setup();
        let t = take(&dir);
        let timeout = "/opt/homebrew/bin/whisper-cli timed out after 120 s and was stopped";
        check_take(&dir, &t, &Failing(timeout.to_owned()), "0.1.0").unwrap();
        let project = Project::open(&dir).unwrap();
        assert_eq!(
            select_unchecked(&project, &read(&dir).unwrap(), Some(missing), FAKE),
            vec![Unchecked {
                take: t,
                stale: false
            }]
        );
    }

    #[test]
    fn an_identical_failed_check_is_not_appended_again() {
        let (_tmp, dir) = setup();
        let t = take(&dir);
        let missing = Failing("whisper-cli not found".to_owned());
        let first = check_take(&dir, &t, &missing, "0.1.0").unwrap();
        let again = check_take(&dir, &t, &missing, "0.1.0").unwrap();
        assert_eq!(again, first, "the failure is still reported");
        assert_eq!(checked_events(&dir), vec![first.clone()]);
        // Another reason is news.
        let other = check_take(&dir, &t, &Failing("crashed".to_owned()), "0.1.0").unwrap();
        assert_eq!(checked_events(&dir), vec![first, other]);
    }

    #[test]
    fn a_failed_recheck_after_a_success_is_listed_like_the_state_shows_it() {
        let (_tmp, dir) = setup();
        let t = take(&dir);
        check_take(&dir, &t, &Fake::new(), "0.1.0").unwrap();
        check_take(&dir, &t, &Failing("crashed".to_owned()), "0.1.0").unwrap();
        let project = Project::open(&dir).unwrap();
        let ledger = read(&dir).unwrap();
        let take = &fold(&project, &ledger).lines[0].takes[0];
        assert_eq!(take.effective, Some(Verdict::Listen), "the newest check");
        assert_eq!(
            select_unchecked(&project, &ledger, None, FAKE),
            vec![Unchecked {
                take: t,
                stale: false
            }]
        );
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

    /// Counts its transcriptions; hears the line on time.
    struct Counting {
        model: &'static str,
        calls: Mutex<u32>,
    }

    impl Counting {
        fn new(model: &'static str) -> Self {
            Self {
                model,
                calls: Mutex::new(0),
            }
        }

        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    impl Transcriber for Counting {
        fn transcribe(&self, path: &Path) -> Transcript {
            *self.calls.lock().unwrap() += 1;
            Fake::new().transcribe(path)
        }
        fn model(&self) -> String {
            self.model.to_owned()
        }
    }

    #[test]
    fn a_moved_slot_reuses_the_transcript() {
        let (_tmp, dir) = setup();
        let t = take(&dir);
        let whisper = Counting::new("ggml-fake.bin");
        assert_eq!(
            check_take(&dir, &t, &whisper, "0.1.0").unwrap().verdict,
            Verdict::Keep
        );
        assert_eq!(whisper.calls(), 1);

        // A 1 s slot: the speech now ends 0.5 s after it. The verdict is
        // recomputed from the stored words, without transcribing.
        set_line_field(&dir, "hook", Field::End(Some(11.0)), None).unwrap();
        let checked = check_take(&dir, &t, &whisper, "0.1.1").unwrap();
        assert_eq!(whisper.calls(), 1);
        assert_eq!(checked.verdict, Verdict::Retake);
        assert_eq!(checked.reasons[0].code, "overrun");
        assert_eq!(checked.snapshot.end, 11.0);
        assert_eq!(checked.transcript.as_ref().map(Vec::len), Some(4));
        assert_eq!(
            checked.checker,
            "takewise 0.1.1 whisper ggml-fake.bin pre=hp60+norm"
        );
        let project = Project::open(&dir).unwrap();
        assert_eq!(
            select_unchecked(&project, &read(&dir).unwrap(), None, FAKE),
            []
        );

        // A changed text, or another model, transcribes again.
        set_line_field(&dir, "hook", Field::Start(9.0), None).unwrap();
        let other = Counting::new("ggml-other.bin");
        check_take(&dir, &t, &other, "0.1.1").unwrap();
        assert_eq!(other.calls(), 1);
        set_line_field(
            &dir,
            "hook",
            Field::Text("Nobody forgets a birthday.".to_owned()),
            None,
        )
        .unwrap();
        let checked = check_take(&dir, &t, &whisper, "0.1.1").unwrap();
        assert_eq!(whisper.calls(), 2);
        assert_eq!(checked.verdict, Verdict::Listen);
    }

    #[test]
    fn a_reused_transcript_keeps_the_sound_finding() {
        struct Coughing;
        impl Transcriber for Coughing {
            fn transcribe(&self, _: &Path) -> Transcript {
                Transcript::Words(vec![
                    word("Nobody", 2.1),
                    word("(coughs)", 2.4),
                    word("misses", 2.7),
                    word("a", 2.9),
                    word("birthday.", 3.1),
                ])
            }
            fn model(&self) -> String {
                "ggml-fake.bin".to_owned()
            }
        }
        let (_tmp, dir) = setup();
        let t = take(&dir);
        let checked = check_take(&dir, &t, &Coughing, "0.1.0").unwrap();
        assert_eq!(checked.verdict, Verdict::Listen);
        set_line_field(&dir, "hook", Field::End(Some(14.0)), None).unwrap();
        let reused = check_take(&dir, &t, &Fake::new(), "0.1.0").unwrap();
        assert_eq!(reused.verdict, Verdict::Listen);
        assert_eq!(reused.reasons, checked.reasons);
    }

    #[test]
    fn transcription_key_names_model_and_preprocessing() {
        let key = transcription_key;
        assert_eq!(
            key("takewise 0.1.0 whisper ggml-a.bin"),
            Some(("ggml-a.bin", "none"))
        );
        assert_eq!(
            key("takewise 0.1.0 whisper ggml-a.bin pre=hp80+norm"),
            Some(("ggml-a.bin", "hp80+norm"))
        );
        // Another preprocessing or model transcribes again.
        assert_ne!(
            key("takewise 0.1.0 whisper ggml-a.bin pre=hp80+norm"),
            key("takewise 0.1.0 whisper ggml-a.bin")
        );
        assert_eq!(
            key(&checker("0.1.0", "ggml-a.bin")),
            Some(("ggml-a.bin", PREPROCESSING))
        );
        assert_eq!(key("hand-written"), None);
    }

    #[test]
    fn a_check_by_another_model_or_preprocessing_is_not_current() {
        let (_tmp, dir) = setup();
        let t = take(&dir);
        check_take(&dir, &t, &Fake::new(), "0.1.0").unwrap();
        let project = Project::open(&dir).unwrap();
        let ledger = read(&dir).unwrap();
        let fake = ("ggml-fake.bin", PREPROCESSING);
        assert_eq!(select_unchecked(&project, &ledger, None, fake), []);
        let stale = vec![Unchecked {
            take: t,
            stale: true,
        }];
        // Another model, or another preprocessing, checks it again.
        let other_model = ("ggml-other.bin", PREPROCESSING);
        assert_eq!(
            select_unchecked(&project, &ledger, None, other_model),
            stale
        );
        let other_pre = ("ggml-fake.bin", "hp80+norm");
        assert_eq!(select_unchecked(&project, &ledger, None, other_pre), stale);
        // Not while transcription cannot be set up: checking again would
        // only replace the words with a failure.
        let missing = Some("whisper model x not found");
        assert_eq!(
            select_unchecked(&project, &ledger, missing, other_model),
            []
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
        let unchecked = select_unchecked(&project, &read(&dir).unwrap(), None, FAKE);
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
