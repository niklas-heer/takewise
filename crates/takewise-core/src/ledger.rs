//! The append-only take ledger, `takes.jsonl`.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, io};
use crate::fsutil::{lock, rename_temp, sync_parent, write_temp};

pub const LEDGER_FILE: &str = "takes.jsonl";

/// One ledger line: `{"v":1,"at":…,"kind":…,"line":…,"take":…,…}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub v: u32,
    pub at: jiff::Timestamp,
    #[serde(flatten)]
    pub kind: EventKind,
    pub line: String,
    pub take: u32,
}

impl Event {
    /// A version-1 event stamped with the current time.
    pub fn new(line: &str, take: u32, kind: EventKind) -> Self {
        Self {
            v: 1,
            at: jiff::Timestamp::now(),
            kind,
            line: line.to_owned(),
            take,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum EventKind {
    Recorded(Recorded),
    Checked(Checked),
    Judged(Judged),
    Chosen(Chosen),
}

/// The line as it was when a take was recorded, checked or judged; `end` is
/// the resolved slot end.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub text: String,
    pub start: f64,
    pub end: f64,
}

/// How far apart two snapshot times may be and still count as the same.
pub const SNAPSHOT_TIME_TOLERANCE_S: f64 = 1e-6;

impl Snapshot {
    /// The same text exactly and the same start and end within a microsecond,
    /// so that times surviving a JSON or TOML round trip still match.
    pub fn matches(&self, other: &Snapshot) -> bool {
        self.text == other.text
            && (self.start - other.start).abs() <= SNAPSHOT_TIME_TOLERANCE_S
            && (self.end - other.end).abs() <= SNAPSHOT_TIME_TOLERANCE_S
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recorded {
    /// Relative to the project directory, such as `takes/hook/001.wav`.
    pub file: String,
    pub sample_rate: u32,
    pub samples: u64,
    pub device: String,
    pub channel: u16,
    /// Film time of the take's first sample minus the line's start.
    pub line_offset_s: f64,
    pub snapshot: Snapshot,
    #[serde(default)]
    pub recovered: bool,
    /// Gaps in the captured audio (filled with silence), when counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropouts: Option<u32>,
    /// The largest distance in milliseconds between where the film really
    /// was during the take and where the take's contiguous audio places it,
    /// when the film stalled or changed rate after it started by more than
    /// 5 ms. Absent otherwise, and on takes recorded before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub film_shift_ms: Option<f64>,
    /// The piece of a passage this take is, when it was cut from one.
    /// Absent on takes recorded line by line; readers from before passages
    /// ignore it and see an ordinary take.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passage: Option<PassagePiece>,
}

/// Which piece of which passage a take is: several lines recorded in one
/// go into `takes/.passages/<id>.wav` and cut into one take per line (see
/// [`crate::passage`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassagePiece {
    /// The passage's id (a uuid): its files are `takes/.passages/<id>.wav`
    /// and `<id>.json`.
    pub id: String,
    /// The piece's place among the passage's pieces, from 0.
    pub index: u32,
    /// How many pieces the passage was cut into.
    pub count: u32,
    /// The take's first sample in the passage file.
    pub from_sample: u64,
    /// One past the take's last sample in the passage file; the next
    /// piece's `from_sample`.
    pub to_sample: u64,
    /// The noise floor of the passage's pre-roll in dBFS, which stands in
    /// for the floor a piece has no pre-roll of its own to measure.
    pub preroll_floor_dbfs: f64,
}

impl PassagePiece {
    /// Whether this is the passage's first piece, which holds its pre-roll.
    pub fn is_first(&self) -> bool {
        self.index == 0
    }

    /// Whether this is the passage's last piece, which holds its tail.
    pub fn is_last(&self) -> bool {
        self.index + 1 >= self.count
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checked {
    pub snapshot: Snapshot,
    pub transcript: Option<Vec<Word>>,
    pub word_diffs: Vec<WordDiff>,
    /// Speech start and end in seconds from the take's first sample.
    pub speech: Option<(f64, f64)>,
    /// Where the words start, in seconds from the take's first sample, as
    /// the early-start rule and export measure it: after a breath or a
    /// noise set apart from the first word by a gap, otherwise the energy
    /// speech start. Absent without speech and on checks from before it
    /// was stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_onset_s: Option<f64>,
    /// The RMS level of the speech range in dBFS, which the noise rule
    /// compares with `noise_floor_dbfs`. Absent without speech and on
    /// checks from before it was stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_level_dbfs: Option<f64>,
    pub overrun_s: Option<f64>,
    pub peak_dbfs: f64,
    pub clipped_samples: u64,
    pub noise_floor_dbfs: f64,
    pub verdict: Verdict,
    pub reasons: Vec<Reason>,
    pub checker: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub start: f64,
    pub end: f64,
    pub confidence: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WordDiff {
    pub kind: WordDiffKind,
    pub expected: Option<String>,
    pub heard: Option<String>,
    pub at: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WordDiffKind {
    Missing,
    Extra,
    Substituted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reason {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Keep,
    Retake,
    Listen,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Keep => "keep",
            Verdict::Retake => "retake",
            Verdict::Listen => "listen",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Judged {
    pub snapshot: Snapshot,
    pub by: String,
    pub verdict: Verdict,
    pub score: Option<f64>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chosen {
    pub by: String,
}

/// The parsed ledger. Unparseable lines are skipped and reported, never fatal.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ledger {
    pub events: Vec<Event>,
    /// The last line is unterminated and does not parse (a crash mid-append).
    pub torn_tail: bool,
    /// 1-based numbers of complete lines that do not parse.
    pub bad_lines: Vec<usize>,
}

impl Ledger {
    /// The `recorded` event of a take, if any.
    pub fn recorded(&self, line: &str, take: u32) -> Option<&Recorded> {
        self.events.iter().find_map(|event| match &event.kind {
            EventKind::Recorded(rec) if event.line == line && event.take == take => Some(rec),
            _ => None,
        })
    }

    /// The takes cut from passage `id`, as (line, take, piece), in ledger
    /// order: one per piece that has a `recorded` event.
    pub fn pieces<'a>(
        &'a self,
        id: &'a str,
    ) -> impl Iterator<Item = (&'a str, u32, &'a PassagePiece)> {
        self.events
            .iter()
            .filter_map(move |event| match &event.kind {
                EventKind::Recorded(Recorded {
                    passage: Some(piece),
                    ..
                }) if piece.id == id => Some((event.line.as_str(), event.take, piece)),
                _ => None,
            })
    }
}

/// Appends one event under the project lock.
pub fn append(dir: &Path, event: &Event) -> Result<()> {
    let _guard = lock(dir)?;
    append_locked(dir, event)
}

/// Appends one event; the caller holds the project lock. When the file does
/// not end in a newline (a torn line), the event still starts on its own line.
pub(crate) fn append_locked(dir: &Path, event: &Event) -> Result<()> {
    let path = dir.join(LEDGER_FILE);
    let mut line = String::new();
    if !ends_with_newline(&path).map_err(io(&path))? {
        line.push('\n');
    }
    line.push_str(&to_line(&path, event)?);
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .map_err(io(&path))?;
    file.write_all(line.as_bytes()).map_err(io(&path))?;
    file.sync_data().map_err(io(&path))
}

/// One event as its ledger line, with the newline.
fn to_line(path: &Path, event: &Event) -> Result<String> {
    let mut line = serde_json::to_string(event).map_err(|err| Error::Parse {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    line.push('\n');
    Ok(line)
}

/// Adds `events` to the ledger so that either all of them reach it or none
/// does; the caller holds the project lock. One event is appended as any
/// other. Several lines appended in one `write` could tear between them,
/// and a reader would take the lines before the tear as the whole batch.
/// So a batch is written into a copy of the ledger, every byte the ledger
/// has followed by the batch (each event a line of its own, the first one
/// also after a torn line); the copy is fsynced and renamed over
/// `takes.jsonl`, and the directory fsynced. A failure or a crash on the
/// way leaves the ledger as it was. Nothing in it is changed or removed.
pub(crate) fn append_all_locked(dir: &Path, events: &[Event]) -> Result<()> {
    match events {
        [] => return Ok(()),
        [event] => return append_locked(dir, event),
        _ => {}
    }
    let path = dir.join(LEDGER_FILE);
    let mut bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(io(&path)(err)),
    };
    let kept = bytes.len();
    if bytes.last().is_some_and(|last| *last != b'\n') {
        bytes.push(b'\n');
    }
    for event in events {
        bytes.extend_from_slice(to_line(&path, event)?.as_bytes());
    }
    let temp = write_temp(&path, |file| write_copy(file, &bytes, kept))?;
    rename_temp(&temp, &path)?;
    sync_parent(&path)
}

#[cfg(any(test, feature = "test-faults"))]
thread_local! {
    static TEAR_BATCHES_AFTER: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

/// Makes writing a batch of events on this thread stop once `bytes` of the
/// batch are written, as a failing disk can tear a write; `None` lets
/// batches through again. For tests.
#[cfg(any(test, feature = "test-faults"))]
pub fn tear_batches_after(bytes: Option<usize>) {
    TEAR_BATCHES_AFTER.set(bytes);
}

/// Writes the ledger's copy: `bytes`, whose first `kept` are the ledger as
/// it is and the rest the batch.
fn write_copy(file: &mut File, bytes: &[u8], kept: usize) -> std::io::Result<()> {
    #[cfg(any(test, feature = "test-faults"))]
    if let Some(torn) = TEAR_BATCHES_AFTER.get() {
        file.write_all(bytes.get(..kept + torn).unwrap_or(bytes))?;
        return Err(std::io::Error::other("the write was torn (test)"));
    }
    #[cfg(not(any(test, feature = "test-faults")))]
    let _ = kept;
    file.write_all(bytes)
}

/// True for a missing or empty file, too.
fn ends_with_newline(path: &Path) -> std::io::Result<bool> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(err) => return Err(err),
    };
    if file.metadata()?.len() == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] == b'\n')
}

/// Reads every event that parses. A missing ledger is empty.
pub fn read(dir: &Path) -> Result<Ledger> {
    let path = dir.join(LEDGER_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Ledger::default()),
        Err(err) => return Err(io(&path)(err)),
    };
    let mut ledger = Ledger::default();
    let terminated = bytes.last().is_none_or(|b| *b == b'\n');
    let lines: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    // `split` yields an empty piece after a final newline.
    let count = if terminated {
        lines.len().saturating_sub(1)
    } else {
        lines.len()
    };
    for (idx, raw) in lines.iter().take(count).enumerate() {
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Event>(raw) {
            Ok(event) => ledger.events.push(event),
            Err(_) if !terminated && idx + 1 == count => ledger.torn_tail = true,
            Err(_) => ledger.bad_lines.push(idx + 1),
        }
    }
    Ok(ledger)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;

    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            text: "Hello there.".to_owned(),
            start: 10.0,
            end: 15.0,
        }
    }

    fn recorded(take: u32) -> Event {
        Event::new(
            "hook",
            take,
            EventKind::Recorded(Recorded {
                file: format!("takes/hook/{take:03}.wav"),
                sample_rate: 48_000,
                samples: 480_000,
                device: "RODECaster Duo Main Stereo".to_owned(),
                channel: 1,
                line_offset_s: -2.0,
                snapshot: snapshot(),
                recovered: false,
                dropouts: None,
                film_shift_ms: None,
                passage: None,
            }),
        )
    }

    fn keys(value: &serde_json::Value) -> BTreeSet<String> {
        value.as_object().unwrap().keys().cloned().collect()
    }

    #[test]
    fn event_json_matches_design() {
        let event = recorded(1);
        let json = serde_json::to_value(&event).unwrap();
        let expected: BTreeSet<String> = [
            "v",
            "at",
            "kind",
            "line",
            "take",
            "file",
            "sample_rate",
            "samples",
            "device",
            "channel",
            "line_offset_s",
            "snapshot",
            "recovered",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert_eq!(keys(&json), expected);
        assert_eq!(json["v"], 1);
        assert_eq!(json["kind"], "recorded");
        assert_eq!(json["line"], "hook");
        assert_eq!(json["take"], 1);
        assert!(json["at"].as_str().unwrap().ends_with('Z'));
        let snapshot_keys: BTreeSet<String> = ["text", "start", "end"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(keys(&json["snapshot"]), snapshot_keys);

        let back: Event = serde_json::from_value(json).unwrap();
        assert_eq!(back, event);
    }

    #[test]
    fn other_events_serialize_flat_and_lowercase() {
        let judged = Event::new(
            "hook",
            1,
            EventKind::Judged(Judged {
                snapshot: snapshot(),
                by: "claude-opus-5-5".to_owned(),
                verdict: Verdict::Retake,
                score: Some(0.25),
                reason: "rushed".to_owned(),
            }),
        );
        let json = serde_json::to_value(&judged).unwrap();
        assert_eq!(json["kind"], "judged");
        assert_eq!(json["verdict"], "retake");

        let checked = Event::new(
            "hook",
            1,
            EventKind::Checked(Checked {
                snapshot: snapshot(),
                transcript: None,
                word_diffs: vec![WordDiff {
                    kind: WordDiffKind::Missing,
                    expected: Some("there".to_owned()),
                    heard: None,
                    at: Some(1.5),
                }],
                speech: Some((2.0, 3.5)),
                speech_onset_s: Some(2.25),
                speech_level_dbfs: Some(-21.5),
                overrun_s: None,
                peak_dbfs: -6.0,
                clipped_samples: 0,
                noise_floor_dbfs: -70.0,
                verdict: Verdict::Keep,
                reasons: vec![Reason {
                    code: "ok".to_owned(),
                    message: "fine".to_owned(),
                }],
                checker: "takewise 0.1.0".to_owned(),
            }),
        );
        let json = serde_json::to_value(&checked).unwrap();
        assert_eq!(json["kind"], "checked");
        assert_eq!(json["word_diffs"][0]["kind"], "missing");
        assert_eq!(json["speech"], serde_json::json!([2.0, 3.5]));
        assert_eq!(json["speech_onset_s"], 2.25);
        let back: Event = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(back, checked);
        // A check from before the onset was stored still reads.
        let mut old = json;
        old.as_object_mut().unwrap().remove("speech_onset_s");
        let back: Event = serde_json::from_value(old).unwrap();
        let EventKind::Checked(back) = back.kind else {
            unreachable!()
        };
        assert_eq!(back.speech_onset_s, None);

        let chosen = Event::new(
            "hook",
            2,
            EventKind::Chosen(Chosen {
                by: "narrator".to_owned(),
            }),
        );
        let json = serde_json::to_value(&chosen).unwrap();
        assert_eq!(json["kind"], "chosen");
        assert_eq!(json["by"], "narrator");
    }

    #[test]
    fn read_missing_ledger_is_empty() {
        let tmp = crate::testutil::project_tempdir();
        let ledger = read(tmp.path()).unwrap();
        assert_eq!(ledger.events, []);
        assert!(!ledger.torn_tail);
    }

    #[test]
    fn append_then_read() {
        let tmp = crate::testutil::project_tempdir();
        append(tmp.path(), &recorded(1)).unwrap();
        append(tmp.path(), &recorded(2)).unwrap();
        let ledger = read(tmp.path()).unwrap();
        let takes: Vec<u32> = ledger.events.iter().map(|e| e.take).collect();
        assert_eq!(takes, vec![1, 2]);
        assert_eq!(ledger.events[0].kind, recorded(1).kind);
        let text = fs::read_to_string(tmp.path().join(LEDGER_FILE)).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn append_after_torn_tail_starts_new_line() {
        let tmp = crate::testutil::project_tempdir();
        let path = tmp.path().join(LEDGER_FILE);
        let first = serde_json::to_string(&recorded(1)).unwrap();
        let torn = r#"{"v":1,"at":"2026-11-22T20:00:00Z","kind":"reco"#;
        fs::write(&path, format!("{first}\n{torn}")).unwrap();

        let ledger = read(tmp.path()).unwrap();
        assert_eq!(ledger.events.len(), 1);
        assert!(ledger.torn_tail);
        assert_eq!(ledger.bad_lines, [0_usize; 0]);

        append(tmp.path(), &recorded(2)).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1], torn, "the fragment stays, alone on its line");
        let ledger = read(tmp.path()).unwrap();
        assert_eq!(ledger.events.len(), 2);
        assert_eq!(ledger.events[1].take, 2);
        assert!(!ledger.torn_tail);
        assert_eq!(ledger.bad_lines, vec![2]);
    }

    #[test]
    fn a_piece_of_a_passage_roundtrips_and_older_readers_ignore_it() {
        /// The `recorded` event as Takewise 0.1 reads it.
        #[derive(Deserialize)]
        struct Before {
            kind: String,
            line: String,
            take: u32,
            file: String,
            line_offset_s: f64,
        }
        let mut event = recorded(2);
        let piece = PassagePiece {
            id: "0d9c5a0e-6a0c-4e52-9d33-3c1f5a2b7e10".to_owned(),
            index: 1,
            count: 3,
            from_sample: 297_600,
            to_sample: 439_200,
            preroll_floor_dbfs: -64.5,
        };
        if let EventKind::Recorded(rec) = &mut event.kind {
            rec.passage = Some(piece.clone());
        }
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.contains(r#""passage":{"id":"0d9c5a0e-6a0c-4e52-9d33-3c1f5a2b7e10","index":1,"count":3,"from_sample":297600,"to_sample":439200,"preroll_floor_dbfs":-64.5}"#),
            "{json}"
        );
        let back: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
        assert!(!piece.is_first() && !piece.is_last());

        let before: Before = serde_json::from_str(&json).unwrap();
        assert_eq!(
            (before.kind.as_str(), before.line.as_str(), before.take),
            ("recorded", "hook", 2)
        );
        assert_eq!(before.file, "takes/hook/002.wav");
        assert!((before.line_offset_s + 2.0).abs() < f64::EPSILON);

        let ledger = Ledger {
            events: vec![recorded(1), event],
            ..Ledger::default()
        };
        let pieces: Vec<_> = ledger.pieces(&piece.id).collect();
        assert_eq!(pieces, vec![("hook", 2, &piece)]);
        assert_eq!(ledger.pieces("another").count(), 0);
    }

    #[test]
    fn a_torn_batch_leaves_the_ledger_as_it_was() {
        let tmp = crate::testutil::project_tempdir();
        let dir = tmp.path();
        let path = dir.join(LEDGER_FILE);
        // An event, and a line an earlier crash tore.
        let first = recorded(1);
        append(dir, &first).unwrap();
        let mut ledger = OpenOptions::new().append(true).open(&path).unwrap();
        ledger.write_all(b"{\"v\":1,\"kind\":\"reco").unwrap();
        drop(ledger);
        let before = fs::read(&path).unwrap();
        let files = fs::read_dir(dir).unwrap().count();
        let batch = [recorded(2), recorded(3), recorded(4)];
        // The newline that ends the torn line, then the three events.
        let written: usize = 1 + batch
            .iter()
            .map(|event| serde_json::to_string(event).unwrap().len() + 1)
            .sum::<usize>();

        // The write stops after every possible number of bytes, also in
        // the middle of an event and between two.
        for torn in 0..written {
            tear_batches_after(Some(torn));
            let failed = append_all_locked(dir, &batch);
            tear_batches_after(None);
            assert!(failed.is_err(), "{torn}");
            assert!(fs::read(&path).unwrap() == before, "{torn}: changed");
            let read = read(dir).unwrap();
            assert_eq!(read.events, std::slice::from_ref(&first), "{torn}: none");
            let left = fs::read_dir(dir).unwrap().count();
            assert_eq!(left, files, "{torn}: no copy is left");
        }

        append_all_locked(dir, &batch).unwrap();
        let after = fs::read(&path).unwrap();
        assert!(after.starts_with(&before), "every earlier byte is kept");
        assert_eq!(after.len(), before.len() + written);
        let read = read(dir).unwrap();
        assert_eq!(read.events.len(), 4);
        assert_eq!(read.bad_lines, vec![2], "the torn line, on its own line");
    }

    #[test]
    fn several_events_are_appended_in_one_write() {
        let tmp = crate::testutil::project_tempdir();
        let path = tmp.path().join(LEDGER_FILE);
        // After a torn line, the first event still starts on its own line.
        fs::write(&path, "{\"v\":1,\"kind\":\"reco").unwrap();
        let events = [recorded(1), recorded(2), recorded(3)];
        append_all_locked(tmp.path(), &events).unwrap();
        append_all_locked(tmp.path(), &[]).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 4);
        assert!(text.ends_with('\n'));
        let ledger = read(tmp.path()).unwrap();
        assert_eq!(ledger.events, events);
        assert_eq!(ledger.bad_lines, vec![1]);
    }

    #[test]
    fn read_skips_bad_middle_line() {
        let tmp = crate::testutil::project_tempdir();
        let path = tmp.path().join(LEDGER_FILE);
        let one = serde_json::to_string(&recorded(1)).unwrap();
        let two = serde_json::to_string(&recorded(2)).unwrap();
        fs::write(
            &path,
            format!("{one}\nnot json\n\n{two}\n{{\"v\":1,\"kind\":\"mystery\"}}\n"),
        )
        .unwrap();
        let ledger = read(tmp.path()).unwrap();
        assert_eq!(ledger.events.len(), 2);
        assert_eq!(ledger.bad_lines, vec![2, 5]);
        assert!(!ledger.torn_tail);
    }
}
