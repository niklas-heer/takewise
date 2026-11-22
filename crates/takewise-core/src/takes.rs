//! Take files: recording into partial files, committing numbered takes, and
//! recovering after a crash.

use std::collections::BTreeSet;
use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, io};
use crate::fsutil::lock;
use crate::ledger::{self, Chosen, Event, EventKind, Judged, Ledger, Recorded, Snapshot, Verdict};
use crate::project::Project;
use crate::project::TAKES_DIR;
use crate::script::is_valid_id;

/// 24-bit full scale.
pub const FULL_SCALE: i32 = (1 << 23) - 1;
const MIN_SAMPLE: i32 = -(1 << 23);

/// Everything the `recorded` event needs that is known when capture starts;
/// stored as the take's `NNN.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedInput {
    pub sample_rate: u32,
    pub device: String,
    pub channel: u16,
    pub line_offset_s: f64,
    pub snapshot: Snapshot,
}

/// A take being recorded: write the audio to `wav`, then `commit_take`.
#[derive(Debug, Clone, PartialEq)]
pub struct PartialTake {
    pub line: String,
    pub meta: PathBuf,
    pub wav: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct TakeRef {
    pub line: String,
    pub take: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RepairReport {
    /// Takes given a `recorded` event (marked `recovered`) from their `NNN.json`.
    pub adopted: Vec<TakeRef>,
    /// `NNN.wav` files without a `recorded` event or `NNN.json`; kept, not adopted.
    pub unadopted: Vec<PathBuf>,
    /// Leftover `.partial` files, relative to the project; kept.
    pub partials: Vec<PathBuf>,
}

fn line_dir(dir: &Path, line_id: &str) -> Result<PathBuf> {
    if !is_valid_id(line_id) {
        return Err(Error::UnknownLine(line_id.to_owned()));
    }
    Ok(dir.join(TAKES_DIR).join(line_id))
}

/// Starts a take: writes its metadata to `takes/<line>/.<uuid>.json.partial`
/// and returns where to write the audio. Needs no lock.
pub fn begin_take(dir: &Path, line_id: &str, input: &RecordedInput) -> Result<PartialTake> {
    let line_dir = line_dir(dir, line_id)?;
    fs::create_dir_all(&line_dir).map_err(io(&line_dir))?;
    let id = uuid::Uuid::new_v4();
    let meta = line_dir.join(format!(".{id}.json.partial"));
    let wav = line_dir.join(format!(".{id}.wav.partial"));
    let json = serde_json::to_vec_pretty(input).map_err(|err| Error::Parse {
        path: meta.clone(),
        message: err.to_string(),
    })?;
    write_synced(&meta, &json)?;
    Ok(PartialTake {
        line: line_id.to_owned(),
        meta,
        wav,
    })
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut file = fs::File::create(path).map_err(io(path))?;
    file.write_all(bytes).map_err(io(path))?;
    file.sync_all().map_err(io(path))
}

/// Publishes a finished take: under the lock, allocates the next number,
/// renames the metadata to `NNN.json` and the audio to `NNN.wav`, and appends
/// the `recorded` event.
pub fn commit_take(dir: &Path, partial: PartialTake) -> Result<TakeRef> {
    let input = read_input(&partial.meta)?;
    let (sample_rate, samples) = wav_header(&partial.wav)?;
    let line_dir = line_dir(dir, &partial.line)?;

    let _guard = lock(dir)?;
    let ledger = ledger::read(dir)?;
    let files = scan(dir)?;
    let used = files
        .numbered
        .iter()
        .filter(|f| f.line == partial.line)
        .map(|f| f.take)
        .chain(
            ledger
                .events
                .iter()
                .filter(|e| e.line == partial.line)
                .map(|e| e.take),
        )
        .max()
        .unwrap_or(0);
    let take = used + 1;
    let meta = line_dir.join(format!("{take:03}.json"));
    let wav = line_dir.join(format!("{take:03}.wav"));
    fs::rename(&partial.meta, &meta).map_err(io(&meta))?;
    fs::rename(&partial.wav, &wav).map_err(io(&wav))?;
    let event = recorded_event(&partial.line, take, input, sample_rate, samples, false);
    ledger::append_locked(dir, &event)?;
    Ok(TakeRef {
        line: partial.line,
        take,
    })
}

fn recorded_event(
    line: &str,
    take: u32,
    input: RecordedInput,
    sample_rate: u32,
    samples: u64,
    recovered: bool,
) -> Event {
    Event::new(
        line,
        take,
        EventKind::Recorded(Recorded {
            file: format!("{TAKES_DIR}/{line}/{take:03}.wav"),
            sample_rate,
            samples,
            device: input.device,
            channel: input.channel,
            line_offset_s: input.line_offset_s,
            snapshot: input.snapshot,
            recovered,
        }),
    )
}

fn read_input(path: &Path) -> Result<RecordedInput> {
    let bytes = fs::read(path).map_err(io(path))?;
    serde_json::from_slice(&bytes).map_err(|err| Error::Parse {
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

fn wav_error(path: &Path) -> impl FnOnce(hound::Error) -> Error {
    let path = path.to_path_buf();
    move |err| match err {
        hound::Error::IoError(source) => Error::Io { path, source },
        other => Error::Parse {
            path,
            message: other.to_string(),
        },
    }
}

/// The sample rate and length in samples (per channel) of a WAV file.
fn wav_header(path: &Path) -> Result<(u32, u64)> {
    let reader = hound::WavReader::open(path).map_err(wav_error(path))?;
    Ok((reader.spec().sample_rate, u64::from(reader.duration())))
}

/// Writes mono 24-bit PCM, clamping samples to 24 bits, and fsyncs it.
pub fn write_take_wav(path: &Path, samples: &[i32], sample_rate: u32) -> Result<()> {
    let file = fs::File::create(path).map_err(io(path))?;
    let file = encode_wav(file, samples, sample_rate).map_err(wav_error(path))?;
    file.sync_all().map_err(io(path))
}

/// Encodes mono 24-bit PCM into `file` and returns it flushed.
pub(crate) fn encode_wav(
    file: fs::File,
    samples: &[i32],
    sample_rate: u32,
) -> std::result::Result<fs::File, hound::Error> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    let mut buffer = BufWriter::new(file);
    {
        let mut writer = hound::WavWriter::new(&mut buffer, spec)?;
        for &sample in samples {
            writer.write_sample(sample.clamp(MIN_SAMPLE, FULL_SCALE))?;
        }
        writer.finalize()?;
    }
    buffer
        .into_inner()
        .map_err(|err| hound::Error::IoError(err.into_error()))
}

/// Reads a take as 24-bit-scale samples of its first channel.
pub fn read_take_wav(path: &Path) -> Result<(Vec<i32>, u32)> {
    let mut reader = hound::WavReader::open(path).map_err(wav_error(path))?;
    let spec = reader.spec();
    if spec.sample_format != hound::SampleFormat::Int || spec.bits_per_sample > 32 {
        return Err(Error::Parse {
            path: path.to_path_buf(),
            message: "takes must be integer PCM".to_owned(),
        });
    }
    let shift = i32::from(spec.bits_per_sample) - 24;
    let channels = usize::from(spec.channels.max(1));
    let mut samples = Vec::with_capacity(reader.duration() as usize);
    for (idx, sample) in reader.samples::<i32>().enumerate() {
        let sample = sample.map_err(wav_error(path))?;
        if idx % channels == 0 {
            samples.push(match shift {
                s if s < 0 => sample << -s,
                s => sample >> s,
            });
        }
    }
    Ok((samples, spec.sample_rate))
}

/// Records a choice of a recorded take of a current line.
pub fn choose_take(dir: &Path, line: &str, take: u32, by: &str) -> Result<()> {
    let _guard = lock(dir)?;
    check_take(dir, line, take)?;
    let event = Event::new(line, take, EventKind::Chosen(Chosen { by: by.to_owned() }));
    ledger::append_locked(dir, &event)
}

/// An outside judgment of a recorded take, against the line as it is now.
#[derive(Debug, Clone, PartialEq)]
pub struct Judgment {
    pub by: String,
    pub verdict: Verdict,
    pub score: Option<f64>,
    pub reason: String,
}

/// Appends a `judged` event for a recorded take of a current line.
pub fn judge_take(dir: &Path, line: &str, take: u32, judgment: Judgment) -> Result<()> {
    let _guard = lock(dir)?;
    let snapshot = check_take(dir, line, take)?;
    let event = Event::new(
        line,
        take,
        EventKind::Judged(Judged {
            snapshot,
            by: judgment.by,
            verdict: judgment.verdict,
            score: judgment.score,
            reason: judgment.reason,
        }),
    );
    ledger::append_locked(dir, &event)
}

/// Checks that `line` is in the script and `take` was recorded for it, and
/// returns the line's current snapshot.
fn check_take(dir: &Path, line: &str, take: u32) -> Result<Snapshot> {
    let project = Project::open(dir)?;
    let snapshot = project
        .snapshot(line)
        .ok_or_else(|| Error::UnknownLine(line.to_owned()))?;
    if ledger::read(dir)?.recorded(line, take).is_none() {
        return Err(Error::UnknownTake {
            line: line.to_owned(),
            take,
        });
    }
    Ok(snapshot)
}

/// A numbered take file, `NNN.wav` or `NNN.json`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NumberedFile {
    pub line: String,
    pub take: u32,
    pub is_wav: bool,
    /// Relative to the project directory.
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TakeFiles {
    pub numbered: Vec<NumberedFile>,
    /// Relative to the project directory, sorted.
    pub partials: Vec<PathBuf>,
}

impl TakeFiles {
    /// `NNN.wav` files that have no `recorded` event.
    pub fn unlogged_wavs<'a>(
        &'a self,
        ledger: &'a Ledger,
    ) -> impl Iterator<Item = &'a NumberedFile> {
        self.numbered
            .iter()
            .filter(|f| f.is_wav && ledger.recorded(&f.line, f.take).is_none())
    }

    fn has_meta(&self, line: &str, take: u32) -> bool {
        self.numbered
            .iter()
            .any(|f| !f.is_wav && f.line == line && f.take == take)
    }
}

/// Lists the files under `takes/`.
pub(crate) fn scan(dir: &Path) -> Result<TakeFiles> {
    let takes = dir.join(TAKES_DIR);
    let mut files = TakeFiles::default();
    let line_dirs = match fs::read_dir(&takes) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(err) => return Err(io(&takes)(err)),
    };
    for entry in line_dirs {
        let entry = entry.map_err(io(&takes))?;
        if !entry.file_type().map_err(io(entry.path()))?.is_dir() {
            continue;
        }
        let line = entry.file_name().to_string_lossy().into_owned();
        for file in fs::read_dir(entry.path()).map_err(io(entry.path()))? {
            let file = file.map_err(io(entry.path()))?;
            let name = file.file_name().to_string_lossy().into_owned();
            let rel = PathBuf::from(TAKES_DIR).join(&line).join(&name);
            if name.ends_with(".partial") {
                files.partials.push(rel);
                continue;
            }
            let Some((stem, ext)) = name.rsplit_once('.') else {
                continue;
            };
            if !(ext == "wav" || ext == "json") || !stem.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            if let Ok(take) = stem.parse::<u32>() {
                files.numbered.push(NumberedFile {
                    line: line.clone(),
                    take,
                    is_wav: ext == "wav",
                    path: rel,
                });
            }
        }
    }
    files.partials.sort();
    files
        .numbered
        .sort_by(|a, b| (&a.line, a.take, a.is_wav).cmp(&(&b.line, b.take, b.is_wav)));
    Ok(files)
}

/// Adopts `NNN.wav` files that lack a `recorded` event from their `NNN.json`
/// (marked `recovered`), and reports the rest. Never deletes anything.
pub fn repair(dir: &Path) -> Result<RepairReport> {
    let _guard = lock(dir)?;
    let ledger = ledger::read(dir)?;
    let files = scan(dir)?;
    let mut report = RepairReport {
        partials: files.partials.clone(),
        ..RepairReport::default()
    };
    let mut adopted = BTreeSet::new();
    for wav in files.unlogged_wavs(&ledger) {
        if !files.has_meta(&wav.line, wav.take) {
            report.unadopted.push(wav.path.clone());
            continue;
        }
        let meta = dir.join(&wav.path).with_extension("json");
        let adopt = read_input(&meta).and_then(|input| {
            let (sample_rate, samples) = wav_header(&dir.join(&wav.path))?;
            Ok((input, sample_rate, samples))
        });
        match adopt {
            Ok((input, sample_rate, samples)) => {
                let event = recorded_event(&wav.line, wav.take, input, sample_rate, samples, true);
                ledger::append_locked(dir, &event)?;
                adopted.insert(TakeRef {
                    line: wav.line.clone(),
                    take: wav.take,
                });
            }
            Err(_) => report.unadopted.push(wav.path.clone()),
        }
    }
    report.adopted = adopted.into_iter().collect();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::ledger::{EventKind, Snapshot, read};

    fn input() -> RecordedInput {
        RecordedInput {
            sample_rate: 48_000,
            device: "Test Mic".to_owned(),
            channel: 1,
            line_offset_s: -2.0,
            snapshot: Snapshot {
                text: "Hello.".to_owned(),
                start: 10.0,
                end: 15.0,
            },
        }
    }

    fn record(dir: &Path, line: &str, samples: usize) -> TakeRef {
        let partial = begin_take(dir, line, &input()).unwrap();
        write_take_wav(&partial.wav, &vec![1000; samples], 48_000).unwrap();
        commit_take(dir, partial).unwrap()
    }

    fn files_in(dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn wav_roundtrip_is_24_bit_mono() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.wav");
        let samples = vec![0, 1, -1, 8_388_607, -8_388_608, 123_456];
        write_take_wav(&path, &samples, 48_000).unwrap();
        let spec = hound::WavReader::open(&path).unwrap().spec();
        assert_eq!(
            (spec.channels, spec.bits_per_sample, spec.sample_rate),
            (1, 24, 48_000)
        );
        assert_eq!(read_take_wav(&path).unwrap(), (samples, 48_000));
    }

    #[test]
    fn begin_take_refuses_unsafe_line_ids() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(begin_take(tmp.path(), "../escape", &input()).is_err());
    }

    #[test]
    fn commit_take_numbers_sequentially() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let takes: Vec<u32> = (0..3).map(|_| record(dir, "hook", 4800).take).collect();
        assert_eq!(takes, vec![1, 2, 3]);
        let line_dir = dir.join("takes/hook");
        let names: Vec<String> = files_in(&line_dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "001.json", "001.wav", "002.json", "002.wav", "003.json", "003.wav"
            ]
        );

        let ledger = read(dir).unwrap();
        assert_eq!(ledger.events.len(), 3);
        for (n, event) in ledger.events.iter().enumerate() {
            let EventKind::Recorded(rec) = &event.kind else {
                panic!("expected recorded, got {event:?}");
            };
            assert_eq!(event.take, u32::try_from(n + 1).unwrap());
            assert_eq!(rec.file, format!("takes/hook/{:03}.wav", n + 1));
            assert_eq!(rec.samples, 4800);
            assert!(!rec.recovered);
            assert_eq!(rec.snapshot, input().snapshot);
        }
    }

    #[test]
    fn commit_skips_numbers_with_leftover_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::create_dir_all(dir.join("takes/hook")).unwrap();
        fs::write(dir.join("takes/hook/004.json"), "{}").unwrap();
        assert_eq!(record(dir, "hook", 10).take, 5);
        assert_eq!(
            fs::read_to_string(dir.join("takes/hook/004.json")).unwrap(),
            "{}"
        );
    }

    #[test]
    fn repair_adopts_unlogged_wav() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        record(dir, "hook", 10);
        // A crash between the renames and the ledger append leaves 002 unlogged.
        let line_dir = dir.join("takes/hook");
        fs::write(
            line_dir.join("002.json"),
            serde_json::to_string(&input()).unwrap(),
        )
        .unwrap();
        write_take_wav(&line_dir.join("002.wav"), &[5; 960], 48_000).unwrap();

        let report = repair(dir).unwrap();
        assert_eq!(
            report.adopted,
            vec![TakeRef {
                line: "hook".to_owned(),
                take: 2
            }]
        );
        assert!(report.unadopted.is_empty());
        let ledger = read(dir).unwrap();
        assert_eq!(ledger.events.len(), 2);
        let EventKind::Recorded(rec) = &ledger.events[1].kind else {
            panic!("expected recorded");
        };
        assert!(rec.recovered);
        assert_eq!(rec.samples, 960);
        assert_eq!(rec.file, "takes/hook/002.wav");

        let again = repair(dir).unwrap();
        assert!(again.adopted.is_empty(), "repair is idempotent");
        assert_eq!(read(dir).unwrap().events.len(), 2);
    }

    #[test]
    fn repair_reports_wav_without_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let line_dir = dir.join("takes/hook");
        fs::create_dir_all(&line_dir).unwrap();
        write_take_wav(&line_dir.join("003.wav"), &[5; 10], 48_000).unwrap();
        let report = repair(dir).unwrap();
        assert!(report.adopted.is_empty());
        assert_eq!(report.unadopted, vec![PathBuf::from("takes/hook/003.wav")]);
        assert!(line_dir.join("003.wav").exists());
        assert!(read(dir).unwrap().events.is_empty());
    }

    #[test]
    fn choose_and_judge_require_a_recorded_take() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let info = crate::project::FilmInfo {
            duration_s: 51.5,
            size: 1,
            modified: "2026-11-22T20:05:11Z".to_owned(),
        };
        Project::init(dir, Path::new("film.mp4"), info).unwrap();
        fs::write(
            dir.join("script.toml"),
            "[[line]]\nid = \"hook\"\nstart = 10.0\ntext = \"Hello.\"\n",
        )
        .unwrap();
        let err = choose_take(dir, "hook", 1, "narrator").unwrap_err();
        assert!(matches!(err, Error::UnknownTake { take: 1, .. }), "{err:?}");
        let err = choose_take(dir, "nope", 1, "narrator").unwrap_err();
        assert!(matches!(err, Error::UnknownLine(_)), "{err:?}");

        record(dir, "hook", 10);
        choose_take(dir, "hook", 1, "narrator").unwrap();
        let judgment = Judgment {
            by: "claude".to_owned(),
            verdict: Verdict::Keep,
            score: Some(0.5),
            reason: "clear".to_owned(),
        };
        judge_take(dir, "hook", 1, judgment).unwrap();
        let ledger = read(dir).unwrap();
        assert_eq!(ledger.events.len(), 3);
        let EventKind::Judged(judged) = &ledger.events[2].kind else {
            panic!("expected judged");
        };
        assert_eq!(
            judged.snapshot,
            Project::open(dir).unwrap().snapshot("hook").unwrap()
        );
    }

    #[test]
    fn repair_keeps_partials() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let partial = begin_take(dir, "hook", &input()).unwrap();
        write_take_wav(&partial.wav, &[1; 10], 48_000).unwrap();
        let report = repair(dir).unwrap();
        let mut expected = vec![
            partial.meta.strip_prefix(dir).unwrap().to_path_buf(),
            partial.wav.strip_prefix(dir).unwrap().to_path_buf(),
        ];
        expected.sort();
        assert_eq!(report.partials, expected);
        assert!(partial.meta.exists() && partial.wav.exists());
        assert!(read(dir).unwrap().events.is_empty());
    }
}
