//! Folding the ledger into the state of every line and take.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Serialize, Serializer};

use crate::ledger::{Checked, EventKind, Judged, Ledger, Recorded, Snapshot, Verdict};
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
    /// Leftover `.partial` files, relative to the project.
    pub partials: Vec<PathBuf>,
    /// `NNN.wav` files without a `recorded` event, relative to the project.
    pub unadopted: Vec<PathBuf>,
    /// What blocks a finished narration; these fail `status --strict`:
    /// validation problems and missing chosen take files.
    pub problems: Vec<Problem>,
    /// Worth knowing but not blocking: crash leftovers (torn or bad ledger
    /// lines, partial and unadopted files), stale chosen takes, missing
    /// files of takes that are not chosen.
    pub notices: Vec<Problem>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LineState {
    pub id: String,
    pub text: String,
    pub start: f64,
    /// The resolved slot end.
    pub end: f64,
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
    /// The latest check or judgment was made against an older text or slot.
    pub stale_check: bool,
    /// Speech start and end in seconds from the take's first sample.
    pub speech: Option<(f64, f64)>,
    /// The take's WAV is not on disk.
    pub missing: bool,
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
    verdict: u8,
    diffs: usize,
    overrun: f64,
    score: Option<f64>,
}

impl Rank {
    /// `Less` means `self` ranks ahead: keep, then listen and unchecked
    /// equally, then retake; then fewer word differences; then smaller
    /// overrun; then higher outside score; then newer.
    fn cmp(&self, other: &Rank) -> Ordering {
        self.verdict
            .cmp(&other.verdict)
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
    let (effective, verdict_source) = match (matching_judged, matching_checked) {
        (Some(judged), _) => (Some(judged.verdict), Some(format!("judge:{}", judged.by))),
        (None, Some(checked)) => (Some(checked.verdict), Some("check".to_owned())),
        (None, None) => (None, None),
    };
    let stale_check = latest_checked.is_some_and(|c| !c.snapshot.matches(current))
        || latest_judged.is_some_and(|j| !j.snapshot.matches(current));
    let state = TakeState {
        take,
        file: rec.file.clone(),
        line_offset_s: rec.line_offset_s,
        effective,
        verdict_source,
        stale_check,
        speech: latest_checked.and_then(|c| c.speech),
        missing: !project.dir.join(&rec.file).is_file(),
    };
    let rank = Rank {
        take,
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
    for line in &project.script.lines {
        let Some(current) = project.snapshot(&line.id) else {
            continue;
        };
        let mut seen = HashSet::new();
        let mut takes = Vec::new();
        let mut ranks = Vec::new();
        let mut chosen = None;
        for event in ledger.events.iter().filter(|e| e.line == line.id) {
            match &event.kind {
                EventKind::Recorded(rec) if seen.insert(event.take) => {
                    let (take, rank) =
                        fold_take(project, ledger, &line.id, event.take, rec, &current);
                    takes.push(take);
                    ranks.push(rank);
                }
                EventKind::Chosen(choice) => chosen = Some((event.take, choice.by.clone())),
                _ => {}
            }
        }
        takes.sort_by_key(|t| t.take);
        let proposed = ranks.iter().min_by(|a, b| a.cmp(b)).map(|r| r.take);
        let chosen_take = chosen.as_ref().map(|(take, _)| *take);
        take_findings(&line.id, &takes, chosen_take, &mut problems, &mut notices);
        lines.push(LineState {
            id: line.id.clone(),
            text: current.text.clone(),
            start: current.start,
            end: current.end,
            takes,
            chosen,
            proposed,
        });
    }

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

    let (partials, unadopted) = recovery_notices(project, ledger, &mut notices);

    State {
        lines,
        orphans,
        partials,
        unadopted,
        problems,
        notices,
    }
}

/// A missing chosen take file is a problem; a missing other take and a
/// stale chosen take are notices.
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
        Checked, Chosen, Event, EventKind, Judged, Recorded, Verdict, WordDiff, WordDiffKind,
        append, read,
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

    fn recorded(project: &Project, line: &str, take: u32) {
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
            }),
        );
        append(&project.dir, &event).unwrap();
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
        assert!(line(&state, "today").takes.is_empty());
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
        assert!(line(&state, "opening").takes.is_empty());
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
