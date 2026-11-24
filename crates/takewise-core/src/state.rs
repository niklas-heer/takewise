//! Folding the ledger into the state of every line and take.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Serialize, Serializer};

use crate::ledger::{
    Checked, EventKind, Judged, Ledger, PassagePiece, Recorded, Snapshot, Verdict,
};
use crate::project::Project;
use crate::script::{Problem, validate};
use crate::takes::{TakeRef, interrupted_commit, scan};

/// Everything `status` shows, derived from the script, the ledger and the
/// files under `takes/`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct State {
    pub lines: Vec<LineState>,
    /// Recorded takes whose line id is no longer in the script.
    pub orphans: Vec<TakeRef>,
    /// Leftover `.partial` files, relative to the project: of takes, and
    /// of passages under `takes/.passages/`.
    pub partials: Vec<PathBuf>,
    /// `NNN.wav` files without a `recorded` event, relative to the project.
    pub unadopted: Vec<PathBuf>,
    /// What blocks a finished narration; these fail `status --strict`:
    /// validation problems and missing chosen take files.
    pub problems: Vec<Problem>,
    /// Worth knowing but not blocking: crash leftovers (torn or bad ledger
    /// lines, partial and unadopted files), stale chosen takes, chosen takes
    /// whose speech overlaps, missing files of takes that are not chosen.
    pub notices: Vec<Problem>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LineState {
    pub id: String,
    pub text: String,
    pub start: f64,
    /// The resolved slot end.
    pub end: f64,
    /// The passage the line belongs to, named by the id of its first line:
    /// consecutive lines less than `[record] passage_gap_s` apart, which
    /// can be recorded in one go.
    pub passage: String,
    pub takes: Vec<TakeState>,
    /// The latest choice: take number and who chose it.
    #[serde(serialize_with = "serialize_chosen")]
    pub chosen: Option<(u32, String)>,
    /// The top-ranked take; proposed, never chosen automatically.
    pub proposed: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TakeState {
    pub take: u32,
    pub file: String,
    pub line_offset_s: f64,
    /// `None` means unchecked.
    pub effective: Option<Verdict>,
    /// `check`, or `judge:<by>`.
    pub verdict_source: Option<String>,
    /// What explains the effective verdict: the check's first reason or the
    /// judgment's reason; `None` for a `keep` check and when unchecked.
    pub reason: Option<String>,
    /// The latest check or judgment was made against an older text or slot.
    pub stale_check: bool,
    /// Speech start and end in seconds from the take's first sample.
    pub speech: Option<(f64, f64)>,
    /// Where the words start in seconds from the take's first sample, after
    /// a breath set apart from them, as export places them; `None` when
    /// the latest check did not store it (the speech start stands in).
    pub speech_onset: Option<f64>,
    /// The take's WAV is not on disk.
    pub missing: bool,
    /// The take was recorded against different text than the line has now.
    pub recorded_against_old_text: bool,
    /// The latest check found samples at or above the clipping threshold;
    /// `None` when unchecked.
    pub clipped: Option<bool>,
    /// The piece of a passage this take was cut from; absent on a take
    /// recorded line by line. The passage's audio is
    /// `takes/.passages/<id>.wav`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passage: Option<PassagePiece>,
}

// serde's `serialize_with` hands the field over by reference.
#[allow(clippy::ref_option)]
fn serialize_chosen<S: Serializer>(
    chosen: &Option<(u32, String)>,
    s: S,
) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Choice<'a> {
        take: u32,
        by: &'a str,
    }
    chosen
        .as_ref()
        .map(|(take, by)| Choice { take: *take, by })
        .serialize(s)
}

impl Project {
    /// The script's passages by `[record] passage_gap_s`, as index ranges
    /// in script order (see [`crate::Script::passages`]).
    pub fn passages(&self) -> Vec<std::ops::Range<usize>> {
        self.script
            .passages(self.film_duration(), self.config.record.passage_gap_s)
    }

    /// The passage line `idx` belongs to: what a passage take of that line
    /// records, from `idx` to the passage's end.
    pub fn passage_of(&self, idx: usize) -> Option<std::ops::Range<usize>> {
        self.passages()
            .into_iter()
            .find(|passage| passage.contains(&idx))
    }

    /// The line as it is now, with its resolved slot end.
    pub fn snapshot(&self, line_id: &str) -> Option<Snapshot> {
        let idx = self.script.position(line_id)?;
        let line = self.script.lines.get(idx)?;
        Some(Snapshot {
            text: line.text.clone(),
            start: line.start,
            end: self.script.slot_end(idx, self.film_duration()),
        })
    }
}

/// Ranking inputs for one take.
struct Rank {
    take: u32,
    /// The take's WAV is not on disk.
    missing: bool,
    verdict: u8,
    diffs: usize,
    overrun: f64,
    score: Option<f64>,
}

impl Rank {
    /// `Less` means `self` ranks ahead: takes on disk before missing ones;
    /// then keep, then listen and unchecked equally, then retake; then fewer
    /// word differences; then smaller overrun; then higher outside score;
    /// then newer.
    fn cmp(&self, other: &Rank) -> Ordering {
        self.missing
            .cmp(&other.missing)
            .then(self.verdict.cmp(&other.verdict))
            .then(self.diffs.cmp(&other.diffs))
            .then(self.overrun.total_cmp(&other.overrun))
            .then(match (self.score, other.score) {
                (Some(a), Some(b)) => b.total_cmp(&a),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            })
            .then(other.take.cmp(&self.take))
    }
}

fn fold_take(
    project: &Project,
    ledger: &Ledger,
    line: &str,
    take: u32,
    rec: &Recorded,
    current: &Snapshot,
) -> (TakeState, Rank) {
    let mut latest_checked: Option<&Checked> = None;
    let mut matching_checked: Option<&Checked> = None;
    let mut latest_judged: Option<&Judged> = None;
    let mut matching_judged: Option<&Judged> = None;
    for event in ledger
        .events
        .iter()
        .filter(|e| e.line == line && e.take == take)
    {
        match &event.kind {
            EventKind::Checked(checked) => {
                latest_checked = Some(checked);
                if checked.snapshot.matches(current) {
                    matching_checked = Some(checked);
                }
            }
            EventKind::Judged(judged) => {
                latest_judged = Some(judged);
                if judged.snapshot.matches(current) {
                    matching_judged = Some(judged);
                }
            }
            EventKind::Recorded(_) | EventKind::Chosen(_) => {}
        }
    }
    let (effective, verdict_source, reason) = match (matching_judged, matching_checked) {
        (Some(judged), _) => (
            Some(judged.verdict),
            Some(format!("judge:{}", judged.by)),
            Some(judged.reason.clone()),
        ),
        (None, Some(checked)) => (
            Some(checked.verdict),
            Some("check".to_owned()),
            checked.reasons.first().map(|r| r.message.clone()),
        ),
        (None, None) => (None, None, None),
    };
    let stale_check = latest_checked.is_some_and(|c| !c.snapshot.matches(current))
        || latest_judged.is_some_and(|j| !j.snapshot.matches(current));
    let state = TakeState {
        take,
        file: rec.file.clone(),
        line_offset_s: rec.line_offset_s,
        effective,
        verdict_source,
        reason,
        stale_check,
        speech: latest_checked.and_then(|c| c.speech),
        speech_onset: latest_checked.and_then(|c| c.speech_onset_s),
        clipped: latest_checked.map(|c| c.clipped_samples > 0),
        missing: !project.dir.join(&rec.file).is_file(),
        recorded_against_old_text: rec.snapshot.text != current.text,
        passage: rec.passage.clone(),
    };
    let rank = Rank {
        take,
        missing: state.missing,
        verdict: match effective {
            Some(Verdict::Keep) => 0,
            Some(Verdict::Listen) | None => 1,
            Some(Verdict::Retake) => 2,
        },
        diffs: matching_checked.map_or(0, |c| c.word_diffs.len()),
        overrun: matching_checked
            .and_then(|c| c.overrun_s)
            .unwrap_or(0.0)
            .max(0.0),
        score: matching_judged.and_then(|j| j.score),
    };
    (state, rank)
}

/// Folds the ledger over the current script: effective verdicts, staleness,
/// the proposed and chosen take per line, orphans, problems and notices.
pub fn fold(project: &Project, ledger: &Ledger) -> State {
    let mut problems = validate(&project.script, project.film_duration());
    let mut notices = Vec::new();
    let mut lines = Vec::with_capacity(project.script.lines.len());
    let passages = project.passages();
    for (idx, line) in project.script.lines.iter().enumerate() {
        let Some(current) = project.snapshot(&line.id) else {
            continue;
        };
        let passage = passages
            .iter()
            .find(|passage| passage.contains(&idx))
            .and_then(|passage| project.script.lines.get(passage.start))
            .map_or_else(|| line.id.clone(), |first| first.id.clone());
        let mut seen = HashSet::new();
        let mut takes = Vec::new();
        let mut ranks = Vec::new();
        let mut choices = Vec::new();
        for event in ledger.events.iter().filter(|e| e.line == line.id) {
            match &event.kind {
                EventKind::Recorded(rec) if seen.insert(event.take) => {
                    let (take, rank) =
                        fold_take(project, ledger, &line.id, event.take, rec, &current);
                    takes.push(take);
                    ranks.push(rank);
                }
                EventKind::Chosen(choice) => choices.push((event.take, choice.by.clone())),
                _ => {}
            }
        }
        let chosen = latest_valid_choice(&line.id, choices, &seen, &mut notices);
        takes.sort_by_key(|t| t.take);
        // A take whose file is missing is never proposed.
        let proposed = ranks
            .iter()
            .filter(|r| !r.missing)
            .min_by(|a, b| a.cmp(b))
            .map(|r| r.take);
        let chosen_take = chosen.as_ref().map(|(take, _)| *take);
        take_findings(&line.id, &takes, chosen_take, &mut problems, &mut notices);
        lines.push(LineState {
            id: line.id.clone(),
            text: current.text.clone(),
            start: current.start,
            end: current.end,
            passage,
            takes,
            chosen,
            proposed,
        });
    }

    speech_overlaps(&lines, &mut notices);

    let ids: HashSet<&str> = project.script.lines.iter().map(|l| l.id.as_str()).collect();
    let mut orphans: Vec<TakeRef> = ledger
        .events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::Recorded(_)) && !ids.contains(e.line.as_str()))
        .map(|e| TakeRef {
            line: e.line.clone(),
            take: e.take,
        })
        .collect();
    orphans.sort();
    orphans.dedup();

    let (mut partials, unadopted) = recovery_notices(project, ledger, &mut notices);
    partials.extend(crate::passage::notices(project, ledger, &mut notices));

    State {
        lines,
        orphans,
        partials,
        unadopted,
        problems,
        notices,
    }
}

/// The latest choice of a recorded take. A choice of a take without a
/// (parseable) `recorded` event is ignored and reported as a notice.
fn latest_valid_choice(
    line: &str,
    choices: Vec<(u32, String)>,
    recorded: &HashSet<u32>,
    notices: &mut Vec<Problem>,
) -> Option<(u32, String)> {
    let mut chosen = None;
    let mut reported = HashSet::new();
    for (take, by) in choices {
        if recorded.contains(&take) {
            chosen = Some((take, by));
        } else if reported.insert(take) {
            notices.push(Problem::new(
                Some(line),
                "choice-without-take",
                format!("take {take} was chosen but has no recorded event; the choice is ignored"),
            ));
        }
    }
    chosen
}

/// A missing chosen take file is a problem; a missing other take, a stale
/// chosen take and one recorded against older text are notices.
fn take_findings(
    line: &str,
    takes: &[TakeState],
    chosen: Option<u32>,
    problems: &mut Vec<Problem>,
    notices: &mut Vec<Problem>,
) {
    for take in takes {
        let is_chosen = chosen == Some(take.take);
        if take.missing {
            let (list, whose) = if is_chosen {
                (&mut *problems, "the chosen take")
            } else {
                (&mut *notices, "take")
            };
            list.push(Problem::new(
                Some(line),
                "missing-file",
                format!("{whose} {} ({}) is missing", take.take, take.file),
            ));
        }
        if is_chosen && take.recorded_against_old_text {
            notices.push(Problem::new(
                Some(line),
                "recorded-against-old-text",
                format!(
                    "the chosen take {} was recorded against different text than the line has now",
                    take.take
                ),
            ));
        }
        if is_chosen && take.stale_check {
            notices.push(Problem::new(
                Some(line),
                "stale-chosen",
                format!(
                    "the chosen take {} was checked or judged against an older text or slot",
                    take.take
                ),
            ));
        }
    }
}

/// Chosen takes of two lines whose speech (from their latest checks)
/// overlaps in the film, which export refuses: a notice naming both, so the
/// narrator learns before exporting. Like export, the speech starts at the
/// words' onset, after a breath set apart from them.
fn speech_overlaps(lines: &[LineState], notices: &mut Vec<Problem>) {
    let spans: Vec<(&str, f64, f64)> = lines
        .iter()
        .filter_map(|line| {
            let (chosen, _) = line.chosen.as_ref()?;
            let take = line.takes.iter().find(|t| t.take == *chosen)?;
            let (energy_start, end) = take.speech?;
            let start = take.speech_onset.unwrap_or(energy_start);
            let at = line.start + take.line_offset_s;
            Some((line.id.as_str(), at + start, at + end))
        })
        .collect();
    for (i, &(a, a_start, a_end)) in spans.iter().enumerate() {
        for &(b, b_start, b_end) in spans.iter().skip(i + 1) {
            if a_start < b_end && b_start < a_end {
                notices.push(Problem::new(
                    Some(a),
                    "speech-overlap",
                    format!(
                        "the speech of the chosen takes of {a} and {b} overlaps from {:.2} s to {:.2} s of the film; export refuses them",
                        a_start.max(b_start),
                        a_end.min(b_end)
                    ),
                ));
            }
        }
    }
}

/// Reports what a crash can leave behind: unparseable ledger lines, partial
/// files, takes without a `recorded` event, and metadata without audio.
/// Returns the partial files and the unadopted WAVs.
fn recovery_notices(
    project: &Project,
    ledger: &Ledger,
    notices: &mut Vec<Problem>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    if ledger.torn_tail {
        notices.push(Problem::new(
            None,
            "torn-ledger",
            "the last line of takes.jsonl is incomplete (a crash while writing); it is ignored",
        ));
    }
    for line in &ledger.bad_lines {
        notices.push(Problem::new(
            None,
            "bad-ledger-line",
            format!("takes.jsonl line {line} does not parse; it is ignored"),
        ));
    }

    let (partials, unadopted) = match scan(&project.dir) {
        Ok(files) => {
            let unadopted: Vec<PathBuf> = files
                .unlogged_wavs(ledger)
                .map(|f| f.path.clone())
                .collect();
            for meta in files.unlogged_metadata(ledger) {
                let (code, message) = if interrupted_commit(&project.dir, meta).is_some() {
                    (
                        "interrupted-commit",
                        format!(
                            "take {} was interrupted while being saved; run `status --repair` to finish it",
                            meta.take
                        ),
                    )
                } else {
                    (
                        "metadata-without-audio",
                        format!(
                            "{} has no audio and no recorded event; it is kept",
                            meta.path.display()
                        ),
                    )
                };
                notices.push(Problem::new(Some(&meta.line), code, message));
            }
            (files.partials, unadopted)
        }
        Err(err) => {
            notices.push(Problem::new(None, "scan-failed", err.to_string()));
            (Vec::new(), Vec::new())
        }
    };
    for path in &partials {
        notices.push(Problem::new(
            None,
            "partial-take",
            format!(
                "{} is left over from an interrupted recording; it is kept",
                path.display()
            ),
        ));
    }
    for path in &unadopted {
        notices.push(Problem::new(
            None,
            "unadopted-take",
            format!(
                "{} has no recorded event; run `status --repair`",
                path.display()
            ),
        ));
    }
    (partials, unadopted)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::edit::{Field, set_line_field};
    use crate::ledger::{
        Checked, Chosen, Event, EventKind, Judged, Reason, Recorded, Verdict, WordDiff,
        WordDiffKind, append, read,
    };
    use crate::project::FilmInfo;

    fn setup(script: &str) -> (tempfile::TempDir, Project) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("p.takewise");
        let info = FilmInfo {
            duration_s: 51.5,
            size: 1,
            modified: "2026-11-22T20:05:11Z".to_owned(),
        };
        Project::init(&dir, Path::new("film.mp4"), info).unwrap();
        fs::write(dir.join("script.toml"), script).unwrap();
        let project = Project::open(&dir).unwrap();
        (tmp, project)
    }

    const TWO_LINES: &str = r#"
[[line]]
id = "hook"
start = 10.0
text = "Hello there."

[[line]]
id = "today"
start = 20.0
text = "Today we start."
"#;

    fn reopen(project: &Project) -> Project {
        Project::open(&project.dir).unwrap()
    }

    fn snap(project: &Project, line: &str) -> Snapshot {
        project.snapshot(line).unwrap()
    }

    /// A `recorded` event and its WAV.
    fn recorded(project: &Project, line: &str, take: u32) {
        let path = project.dir.join(format!("takes/{line}/{take:03}.wav"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::takes::write_take_wav(&path, &[0; 10], 48_000).unwrap();
        recorded_event(project, line, take);
    }

    /// A `recorded` event only, as if its WAV went missing.
    fn recorded_event(project: &Project, line: &str, take: u32) {
        let event = Event::new(
            line,
            take,
            EventKind::Recorded(Recorded {
                file: format!("takes/{line}/{take:03}.wav"),
                sample_rate: 48_000,
                samples: 48_000,
                device: "Test Mic".to_owned(),
                channel: 1,
                line_offset_s: -2.0,
                snapshot: snap(project, line),
                recovered: false,
                dropouts: None,
                film_shift_ms: None,
                passage: None,
            }),
        );
        append(&project.dir, &event).unwrap();
    }

    /// A `keep` check of take 1 with speech at 2..3 s of the take.
    fn check_of(project: &Project, line: &str) -> Checked {
        Checked {
            snapshot: snap(project, line),
            transcript: None,
            word_diffs: Vec::new(),
            speech: Some((2.0, 3.0)),
            speech_onset_s: None,
            speech_level_dbfs: None,
            overrun_s: None,
            peak_dbfs: -6.0,
            clipped_samples: 0,
            noise_floor_dbfs: -70.0,
            verdict: Verdict::Keep,
            reasons: Vec::new(),
            checker: "test".to_owned(),
        }
    }

    fn checked(
        project: &Project,
        line: &str,
        take: u32,
        verdict: Verdict,
        diffs: usize,
        overrun: Option<f64>,
    ) {
        let diff = WordDiff {
            kind: WordDiffKind::Substituted,
            expected: Some("a".to_owned()),
            heard: Some("b".to_owned()),
            at: None,
        };
        let event = Event::new(
            line,
            take,
            EventKind::Checked(Checked {
                snapshot: snap(project, line),
                transcript: None,
                word_diffs: vec![diff; diffs],
                speech: Some((2.0, 3.0)),
                speech_onset_s: None,
                speech_level_dbfs: None,
                overrun_s: overrun,
                peak_dbfs: -6.0,
                clipped_samples: 0,
                noise_floor_dbfs: -70.0,
                verdict,
                reasons: Vec::new(),
                checker: "test".to_owned(),
            }),
        );
        append(&project.dir, &event).unwrap();
    }

    fn judged(project: &Project, line: &str, take: u32, verdict: Verdict, score: Option<f64>) {
        let event = Event::new(
            line,
            take,
            EventKind::Judged(Judged {
                snapshot: snap(project, line),
                by: "claude-opus-5-5".to_owned(),
                verdict,
                score,
                reason: "listened".to_owned(),
            }),
        );
        append(&project.dir, &event).unwrap();
    }

    fn chosen(project: &Project, line: &str, take: u32, by: &str) {
        let event = Event::new(line, take, EventKind::Chosen(Chosen { by: by.to_owned() }));
        append(&project.dir, &event).unwrap();
    }

    fn fold_now(project: &Project) -> State {
        let project = reopen(project);
        fold(&project, &read(&project.dir).unwrap())
    }

    fn line<'a>(state: &'a State, id: &str) -> &'a LineState {
        state.lines.iter().find(|l| l.id == id).unwrap()
    }

    #[test]
    fn fold_lists_takes_per_line() {
        let (_tmp, project) = setup(TWO_LINES);
        recorded(&project, "hook", 1);
        recorded(&project, "hook", 2);
        let state = fold_now(&project);
        let hook = line(&state, "hook");
        assert_eq!(
            hook.takes.iter().map(|t| t.take).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(hook.takes[0].file, "takes/hook/001.wav");
        assert!((hook.takes[0].line_offset_s + 2.0).abs() < f64::EPSILON);
        assert_eq!(line(&state, "today").takes, []);
        assert_eq!(line(&state, "today").proposed, None);
        assert!(state.problems.is_empty(), "{:?}", state.problems);
    }

    #[test]
    fn fold_judged_overrides_checked() {
        let (_tmp, project) = setup(TWO_LINES);
        recorded(&project, "hook", 1);
        checked(&project, "hook", 1, Verdict::Keep, 0, None);
        judged(&project, "hook", 1, Verdict::Retake, Some(0.2));
        let state = fold_now(&project);
        let take = &line(&state, "hook").takes[0];
        assert_eq!(take.effective, Some(Verdict::Retake));
        assert_eq!(
            take.verdict_source.as_deref(),
            Some("judge:claude-opus-5-5")
        );
        assert!(!take.stale_check);
        assert_eq!(take.speech, Some((2.0, 3.0)));
    }

    #[test]
    fn judged_stale_after_text_edit() {
        let (_tmp, project) = setup(TWO_LINES);
        recorded(&project, "hook", 1);
        checked(&project, "hook", 1, Verdict::Keep, 0, None);
        judged(&project, "hook", 1, Verdict::Keep, None);
        set_line_field(
            &project.dir,
            "hook",
            Field::Text("Hello, there.".to_owned()),
            None,
        )
        .unwrap();
        let state = fold_now(&project);
        let take = &line(&state, "hook").takes[0];
        assert_eq!(take.effective, None);
        assert_eq!(take.verdict_source, None);
        assert!(take.stale_check);
    }

    #[test]
    fn judgment_survives_json_roundtrip_of_awkward_times() {
        // Times like these come from `M` (mark start at the playhead).
        let script = "[[line]]\nid = \"hook\"\nstart = 1.1372262625247673\ntext = \"Hello there.\"\n\n[[line]]\nid = \"today\"\nstart = 37.903460614113676\ntext = \"Today we start.\"\n";
        let (_tmp, project) = setup(script);
        for id in ["hook", "today"] {
            recorded(&project, id, 1);
            let judgment = crate::takes::Judgment {
                by: "claude".to_owned(),
                verdict: Verdict::Keep,
                score: None,
                reason: "fine".to_owned(),
            };
            crate::takes::judge_take(&project.dir, id, 1, judgment).unwrap();
        }
        let state = fold_now(&project);
        for id in ["hook", "today"] {
            let take = &line(&state, id).takes[0];
            assert!(!take.stale_check, "{id}: {take:?}");
            assert_eq!(take.effective, Some(Verdict::Keep), "{id}");
        }
    }

    #[test]
    fn snapshot_times_match_within_a_microsecond() {
        let a = Snapshot {
            text: "Hello.".to_owned(),
            start: 1.0,
            end: 2.0,
        };
        let close = Snapshot {
            start: 1.0 + 5e-7,
            end: 2.0 - 5e-7,
            ..a.clone()
        };
        assert!(a.matches(&close));
        let moved = Snapshot {
            start: 1.000_01,
            ..a.clone()
        };
        assert!(!a.matches(&moved));
        let edited = Snapshot {
            text: "Hello!".to_owned(),
            ..a.clone()
        };
        assert!(!a.matches(&edited));
    }

    #[test]
    fn check_stale_after_slot_change() {
        let (_tmp, project) = setup(TWO_LINES);
        recorded(&project, "hook", 1);
        checked(&project, "hook", 1, Verdict::Keep, 0, None);
        assert!(!line(&fold_now(&project), "hook").takes[0].stale_check);
        // Moving the next line changes hook's resolved slot end.
        set_line_field(&project.dir, "today", Field::Start(21.0), None).unwrap();
        let state = fold_now(&project);
        let take = &line(&state, "hook").takes[0];
        assert!(take.stale_check);
        assert_eq!(take.effective, None);

        // A fresh check against the new slot clears it.
        checked(&reopen(&project), "hook", 1, Verdict::Listen, 0, None);
        let state = fold_now(&project);
        let take = &line(&state, "hook").takes[0];
        assert!(!take.stale_check);
        assert_eq!(take.effective, Some(Verdict::Listen));
        assert_eq!(take.verdict_source.as_deref(), Some("check"));
    }

    #[test]
    fn effective_verdict_carries_its_first_reason() {
        let (_tmp, project) = setup(TWO_LINES);
        recorded(&project, "hook", 1);
        assert_eq!(line(&fold_now(&project), "hook").takes[0].reason, None);
        let event = Event::new(
            "hook",
            1,
            EventKind::Checked(Checked {
                snapshot: snap(&project, "hook"),
                transcript: Some(Vec::new()),
                word_diffs: Vec::new(),
                speech: Some((2.0, 3.0)),
                speech_onset_s: None,
                speech_level_dbfs: None,
                overrun_s: Some(0.4),
                peak_dbfs: -6.0,
                clipped_samples: 0,
                noise_floor_dbfs: -70.0,
                verdict: Verdict::Retake,
                reasons: vec![
                    Reason {
                        code: "overrun".to_owned(),
                        message: "speech ends 0.40 s after the slot".to_owned(),
                    },
                    Reason {
                        code: "possible-mishearing".to_owned(),
                        message: "heard \u{201c}b\u{201d} for \u{201c}a\u{201d}".to_owned(),
                    },
                ],
                checker: "test".to_owned(),
            }),
        );
        append(&project.dir, &event).unwrap();
        let state = fold_now(&project);
        assert_eq!(
            line(&state, "hook").takes[0].reason.as_deref(),
            Some("speech ends 0.40 s after the slot")
        );
        // A matching judgment speaks for the take instead.
        judged(&project, "hook", 1, Verdict::Keep, None);
        let state = fold_now(&project);
        assert_eq!(
            line(&state, "hook").takes[0].reason.as_deref(),
            Some("listened")
        );
        // A stale verdict gives no reason.
        set_line_field(&project.dir, "hook", Field::Text("Hi.".to_owned()), None).unwrap();
        assert_eq!(line(&fold_now(&project), "hook").takes[0].reason, None);
    }

    #[test]
    fn unchecked_ranks_with_listen() {
        let (_tmp, project) = setup(TWO_LINES);
        recorded(&project, "hook", 1);
        recorded(&project, "hook", 2);
        recorded(&project, "hook", 3);
        checked(&project, "hook", 1, Verdict::Listen, 0, None);
        checked(&project, "hook", 3, Verdict::Retake, 0, None);
        // take 2 is unchecked: ties with the listen take, and the newer one wins.
        assert_eq!(line(&fold_now(&project), "hook").proposed, Some(2));
        assert_eq!(line(&fold_now(&project), "hook").takes[1].effective, None);
    }

    #[test]
    fn ranking_prefers_keep_then_fewer_diffs_then_newest() {
        let (_tmp, project) = setup(TWO_LINES);
        for take in 1..=5 {
            recorded(&project, "hook", take);
        }
        checked(&project, "hook", 1, Verdict::Keep, 1, None);
        checked(&project, "hook", 2, Verdict::Keep, 0, Some(0.1));
        checked(&project, "hook", 3, Verdict::Keep, 0, Some(0.1));
        checked(&project, "hook", 4, Verdict::Listen, 0, None);
        checked(&project, "hook", 5, Verdict::Retake, 0, None);
        assert_eq!(line(&fold_now(&project), "hook").proposed, Some(3));

        // A smaller overrun beats newer.
        checked(&project, "hook", 2, Verdict::Keep, 0, Some(0.0));
        assert_eq!(line(&fold_now(&project), "hook").proposed, Some(2));

        // An outside score breaks the remaining tie.
        checked(&project, "hook", 3, Verdict::Keep, 0, Some(0.0));
        judged(&project, "hook", 2, Verdict::Keep, Some(0.9));
        judged(&project, "hook", 3, Verdict::Keep, Some(0.4));
        assert_eq!(line(&fold_now(&project), "hook").proposed, Some(2));
    }

    #[test]
    fn takes_with_missing_files_rank_last_and_are_never_proposed() {
        let (_tmp, project) = setup(TWO_LINES);
        recorded_event(&project, "hook", 1);
        recorded(&project, "hook", 2);
        recorded_event(&project, "hook", 3);
        checked(&project, "hook", 1, Verdict::Keep, 0, None);
        checked(&project, "hook", 2, Verdict::Retake, 0, None);
        checked(&project, "hook", 3, Verdict::Keep, 0, None);
        assert_eq!(line(&fold_now(&project), "hook").proposed, Some(2));

        recorded_event(&project, "today", 1);
        checked(&project, "today", 1, Verdict::Keep, 0, None);
        assert_eq!(line(&fold_now(&project), "today").proposed, None);
    }

    #[test]
    fn fold_latest_choice_wins() {
        let (_tmp, project) = setup(TWO_LINES);
        recorded(&project, "hook", 1);
        recorded(&project, "hook", 2);
        chosen(&project, "hook", 2, "narrator");
        chosen(&project, "hook", 1, "claude");
        let state = fold_now(&project);
        assert_eq!(line(&state, "hook").chosen, Some((1, "claude".to_owned())));
    }

    #[test]
    fn choice_without_a_recorded_take_is_ignored() {
        let (_tmp, project) = setup(TWO_LINES);
        recorded(&project, "hook", 1);
        chosen(&project, "hook", 1, "narrator");
        // A choice of a take that was never recorded.
        chosen(&project, "hook", 3, "narrator");
        // A choice whose recorded row is malformed.
        let ledger = project.dir.join("takes.jsonl");
        let mut text = fs::read_to_string(&ledger).unwrap();
        text.push_str(
            "{\"v\":1,\"at\":\"2026-11-22T20:00:00Z\",\"kind\":\"recorded\",\"line\":\"today\",\"take\":1}\n",
        );
        fs::write(&ledger, text).unwrap();
        chosen(&project, "today", 1, "claude");

        let state = fold_now(&project);
        assert_eq!(
            line(&state, "hook").chosen,
            Some((1, "narrator".to_owned())),
            "the earlier valid choice stands"
        );
        assert_eq!(line(&state, "today").chosen, None);
        let codes: Vec<(Option<&str>, &str)> = state
            .notices
            .iter()
            .map(|p| (p.line.as_deref(), p.code))
            .collect();
        assert!(
            codes.contains(&(Some("hook"), "choice-without-take")),
            "{codes:?}"
        );
        assert!(
            codes.contains(&(Some("today"), "choice-without-take")),
            "{codes:?}"
        );
        assert!(
            !state
                .problems
                .iter()
                .any(|p| p.code == "choice-without-take"),
            "{:?}",
            state.problems
        );
    }

    #[test]
    fn stale_chosen_take_is_a_notice() {
        let (_tmp, project) = setup(TWO_LINES);
        recorded(&project, "hook", 1);
        fs::create_dir_all(project.dir.join("takes/hook")).unwrap();
        crate::takes::write_take_wav(&project.dir.join("takes/hook/001.wav"), &[0; 10], 48_000)
            .unwrap();
        checked(&project, "hook", 1, Verdict::Keep, 0, None);
        chosen(&project, "hook", 1, "narrator");
        set_line_field(&project.dir, "today", Field::Start(21.0), None).unwrap();
        let state = fold_now(&project);
        assert!(state.problems.is_empty(), "{:?}", state.problems);
        assert!(
            state
                .notices
                .iter()
                .any(|n| n.code == "stale-chosen" && n.line.as_deref() == Some("hook")),
            "{:?}",
            state.notices
        );
    }

    #[test]
    fn overlapping_speech_of_chosen_takes_is_a_notice() {
        // Each take's speech is 2..3 s of a take from 2 s before its line:
        // 10..11 s and 10.5..11.5 s of film.
        let script = "[[line]]\nid = \"one\"\nstart = 10.0\ntext = \"A.\"\n\n[[line]]\nid = \"two\"\nstart = 10.5\ntext = \"B.\"\n";
        let (_tmp, project) = setup(script);
        for line in ["one", "two"] {
            recorded(&project, line, 1);
            checked(&project, line, 1, Verdict::Keep, 0, None);
        }
        let overlaps = |state: &State| -> Vec<Problem> {
            state
                .notices
                .iter()
                .filter(|n| n.code == "speech-overlap")
                .cloned()
                .collect()
        };
        // Only chosen takes count.
        chosen(&project, "one", 1, "narrator");
        assert_eq!(overlaps(&fold_now(&project)), []);
        chosen(&project, "two", 1, "narrator");
        let found = overlaps(&fold_now(&project));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].line.as_deref(), Some("one"));
        assert!(found[0].message.contains("two"), "{found:?}");

        // Speech apart, margins or not, is fine.
        let (_tmp, project) = setup(TWO_LINES);
        for line in ["hook", "today"] {
            recorded(&project, line, 1);
            checked(&project, line, 1, Verdict::Keep, 0, None);
            chosen(&project, line, 1, "narrator");
        }
        assert_eq!(overlaps(&fold_now(&project)), []);
    }

    #[test]
    fn speech_overlap_starts_after_a_breath_as_export_does() {
        // Line two's energy speech runs from 10.5 s, but its words start
        // after a breath at 11.1 s, after line one's speech ended at 11 s:
        // export accepts the pair, so status names no overlap.
        let script = "[[line]]\nid = \"one\"\nstart = 10.0\ntext = \"A.\"\n\n[[line]]\nid = \"two\"\nstart = 10.5\ntext = \"B.\"\n";
        let (_tmp, project) = setup(script);
        for line in ["one", "two"] {
            recorded(&project, line, 1);
            chosen(&project, line, 1, "narrator");
        }
        checked(&project, "one", 1, Verdict::Keep, 0, None);
        let event = Event::new(
            "two",
            1,
            EventKind::Checked(Checked {
                speech_onset_s: Some(2.6),
                ..check_of(&project, "two")
            }),
        );
        append(&project.dir, &event).unwrap();
        let state = fold_now(&project);
        assert!(
            !state.notices.iter().any(|n| n.code == "speech-overlap"),
            "{:?}",
            state.notices
        );
        assert_eq!(line(&state, "two").takes[0].speech_onset, Some(2.6));
    }

    #[test]
    fn chosen_take_recorded_against_old_text_is_a_notice() {
        let (_tmp, project) = setup(TWO_LINES);
        recorded(&project, "hook", 1);
        fs::create_dir_all(project.dir.join("takes/hook")).unwrap();
        crate::takes::write_take_wav(&project.dir.join("takes/hook/001.wav"), &[0; 10], 48_000)
            .unwrap();
        chosen(&project, "hook", 1, "narrator");
        assert_eq!(fold_now(&project).notices, []);
        set_line_field(&project.dir, "hook", Field::Text("Hi.".to_owned()), None).unwrap();
        let state = fold_now(&project);
        assert!(line(&state, "hook").takes[0].recorded_against_old_text);
        assert!(state.problems.is_empty(), "{:?}", state.problems);
        assert!(
            state
                .notices
                .iter()
                .any(|n| n.code == "recorded-against-old-text"
                    && n.line.as_deref() == Some("hook")),
            "{:?}",
            state.notices
        );
    }

    #[test]
    fn orphan_after_id_change() {
        let (_tmp, project) = setup(TWO_LINES);
        recorded(&project, "hook", 1);
        fs::write(
            project.dir.join("script.toml"),
            TWO_LINES.replace("\"hook\"", "\"opening\""),
        )
        .unwrap();
        let state = fold_now(&project);
        assert_eq!(
            state.orphans,
            vec![TakeRef {
                line: "hook".to_owned(),
                take: 1
            }]
        );
        assert_eq!(line(&state, "opening").takes, []);
    }

    #[test]
    fn fold_names_each_lines_passage_and_each_takes_piece() {
        // Two sentences 0.5 s apart, then one after a pause of 2 s.
        const SCRIPT: &str = r#"
[[line]]
id = "one"
start = 10.0
end = 13.0
text = "One."

[[line]]
id = "two"
start = 13.5
end = 16.0
text = "Two."

[[line]]
id = "three"
start = 18.0
text = "Three."
"#;
        let (_tmp, project) = setup(SCRIPT);
        recorded(&project, "one", 1);
        let piece = PassagePiece {
            id: "0d9c5a0e-6a0c-4e52-9d33-3c1f5a2b7e10".to_owned(),
            index: 1,
            count: 2,
            from_sample: 297_600,
            to_sample: 480_000,
            preroll_floor_dbfs: -64.5,
        };
        let event = Event::new(
            "two",
            1,
            EventKind::Recorded(Recorded {
                file: "takes/two/001.wav".to_owned(),
                sample_rate: 48_000,
                samples: 182_400,
                device: "Test Mic".to_owned(),
                channel: 1,
                line_offset_s: -0.3,
                snapshot: snap(&project, "two"),
                recovered: false,
                dropouts: None,
                film_shift_ms: None,
                passage: Some(piece.clone()),
            }),
        );
        append(&project.dir, &event).unwrap();

        let state = fold_now(&project);
        let passages: Vec<&str> = state.lines.iter().map(|l| l.passage.as_str()).collect();
        assert_eq!(passages, ["one", "one", "three"]);
        assert_eq!(line(&state, "one").takes[0].passage, None);
        assert_eq!(line(&state, "two").takes[0].passage, Some(piece));

        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["lines"][1]["passage"], "one");
        let take = &json["lines"][1]["takes"][0];
        assert_eq!(take["passage"]["index"], 1);
        assert_eq!(take["passage"]["count"], 2);
        assert_eq!(take["passage"]["from_sample"], 297_600);
        assert_eq!(take["passage"]["to_sample"], 480_000);
        assert_eq!(take["passage"]["preroll_floor_dbfs"], -64.5);
        assert!(
            json["lines"][0]["takes"][0].get("passage").is_none(),
            "a take recorded line by line has none"
        );

        // `[record] passage_gap_s` moves the limit.
        let config = fs::read_to_string(project.dir.join("project.toml")).unwrap();
        fs::write(
            project.dir.join("project.toml"),
            config.replace("passage_gap_s = 1.0", "passage_gap_s = 0.25"),
        )
        .unwrap();
        let state = fold_now(&reopen(&project));
        let passages: Vec<&str> = state.lines.iter().map(|l| l.passage.as_str()).collect();
        assert_eq!(passages, ["one", "two", "three"]);
    }

    #[test]
    fn fold_reports_interrupted_commits_and_lone_metadata() {
        let (_tmp, project) = setup(TWO_LINES);
        let input = crate::takes::RecordedInput {
            sample_rate: 48_000,
            device: "Test Mic".to_owned(),
            channel: 1,
            line_offset_s: -2.0,
            snapshot: snap(&project, "hook"),
            dropouts: None,
            film_shift_ms: None,
            passage: None,
        };
        let partial = crate::takes::begin_take(&project.dir, "hook", &input).unwrap();
        crate::takes::write_take_wav(&partial.wav, &[1; 10], 48_000).unwrap();
        fs::rename(&partial.meta, project.dir.join("takes/hook/001.json")).unwrap();
        fs::create_dir_all(project.dir.join("takes/today")).unwrap();
        fs::write(project.dir.join("takes/today/001.json"), "{}").unwrap();

        let state = fold_now(&project);
        assert!(state.problems.is_empty(), "{:?}", state.problems);
        let codes: Vec<(Option<&str>, &str)> = state
            .notices
            .iter()
            .map(|p| (p.line.as_deref(), p.code))
            .collect();
        assert!(
            codes.contains(&(Some("hook"), "interrupted-commit")),
            "{codes:?}"
        );
        assert!(
            codes.contains(&(Some("today"), "metadata-without-audio")),
            "{codes:?}"
        );
    }

    #[test]
    fn fold_reports_ledger_and_file_problems() {
        let (_tmp, project) = setup(TWO_LINES);
        recorded(&project, "hook", 1);
        let ledger_path = project.dir.join("takes.jsonl");
        let mut text = fs::read_to_string(&ledger_path).unwrap();
        text.push_str("{\"v\":1,\"at\"");
        fs::write(&ledger_path, text).unwrap();
        let partial = crate::takes::begin_take(
            &project.dir,
            "hook",
            &crate::takes::RecordedInput {
                sample_rate: 48_000,
                device: "Test Mic".to_owned(),
                channel: 1,
                line_offset_s: -2.0,
                snapshot: snap(&project, "hook"),
                dropouts: None,
                film_shift_ms: None,
                passage: None,
            },
        )
        .unwrap();
        fs::create_dir_all(project.dir.join("takes/today")).unwrap();
        fs::write(project.dir.join("takes/today/001.wav"), b"x").unwrap();

        let state = fold_now(&project);
        assert!(
            state.problems.is_empty(),
            "crash leftovers are notices: {:?}",
            state.problems
        );
        let codes: Vec<&str> = state.notices.iter().map(|p| p.code).collect();
        assert!(codes.contains(&"torn-ledger"), "{codes:?}");
        assert!(codes.contains(&"partial-take"), "{codes:?}");
        assert!(codes.contains(&"unadopted-take"), "{codes:?}");
        assert_eq!(
            state.partials,
            vec![
                partial
                    .meta
                    .strip_prefix(&project.dir)
                    .unwrap()
                    .to_path_buf()
            ]
        );
        assert_eq!(
            state.unadopted,
            vec![std::path::PathBuf::from("takes/today/001.wav")]
        );
    }
}
