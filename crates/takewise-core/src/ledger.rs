//! The append-only take ledger, `takes.jsonl`.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, io};
use crate::fsutil::lock;

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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checked {
    pub snapshot: Snapshot,
    pub transcript: Option<Vec<Word>>,
    pub word_diffs: Vec<WordDiff>,
    /// Speech start and end in seconds from the take's first sample.
    pub speech: Option<(f64, f64)>,
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
    line.push_str(&serde_json::to_string(event).map_err(|err| Error::Parse {
        path: path.clone(),
        message: err.to_string(),
    })?);
    line.push('\n');
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .map_err(io(&path))?;
    file.write_all(line.as_bytes()).map_err(io(&path))?;
    file.sync_data().map_err(io(&path))
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
        let back: Event = serde_json::from_value(json).unwrap();
        assert_eq!(back, checked);

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
