//! Take files: recording into partial files, committing numbered takes, and
//! recovering after a crash.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
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
    /// Gaps in the captured audio, once counted (when the take is stopped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropouts: Option<u32>,
    /// How far the film departed from the take's placement after it
    /// started (a stall or rate change), when more than 5 ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub film_shift_ms: Option<f64>,
}

/// A take's `NNN.json`: the recording input plus the name of the partial
/// file the audio was recorded into, so that a commit interrupted between its
/// two renames can be completed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct TakeMeta {
    #[serde(flatten)]
    input: RecordedInput,
    /// `.<uuid>.wav.partial`, in the take's line directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    partial_wav: Option<String>,
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
    /// `NNN.json` files without a `recorded` event whose audio is in neither
    /// `NNN.wav` nor the partial file they name; kept.
    pub metadata_without_audio: Vec<PathBuf>,
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
    let wav_name = format!(".{id}.wav.partial");
    let wav = line_dir.join(&wav_name);
    let take_meta = TakeMeta {
        input: input.clone(),
        partial_wav: Some(wav_name),
    };
    let json = serde_json::to_vec_pretty(&take_meta).map_err(|err| Error::Parse {
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
// Taken by value: a committed partial take no longer exists.
#[allow(clippy::needless_pass_by_value)]
pub fn commit_take(dir: &Path, partial: PartialTake) -> Result<TakeRef> {
    let input = read_meta(&partial.meta)?.input;
    let _guard = lock(dir)?;
    publish_locked(dir, &partial, input, false)
}

/// Replaces a partial take's metadata with `input`, such as the line's
/// position at commit time. Atomic: a crash leaves the old or the new file.
pub fn update_take_meta(partial: &PartialTake, input: &RecordedInput) -> Result<()> {
    let mut meta = read_meta(&partial.meta)?;
    meta.input = input.clone();
    let json = serde_json::to_vec_pretty(&meta).map_err(|err| Error::Parse {
        path: partial.meta.clone(),
        message: err.to_string(),
    })?;
    crate::fsutil::write_atomic(&partial.meta, &json)
}

/// Numbers and publishes a partial take; the caller holds the project lock.
fn publish_locked(
    dir: &Path,
    partial: &PartialTake,
    input: RecordedInput,
    recovered: bool,
) -> Result<TakeRef> {
    let (sample_rate, samples) = wav_header(&partial.wav)?;
    let line_dir = line_dir(dir, &partial.line)?;
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
    injected_fault(1, &meta)?;
    fs::rename(&partial.wav, &wav).map_err(io(&wav))?;
    injected_fault(2, &wav)?;
    let event = recorded_event(&partial.line, take, input, sample_rate, samples, recovered);
    ledger::append_locked(dir, &event)?;
    Ok(TakeRef {
        line: partial.line.clone(),
        take,
    })
}

#[cfg(any(test, feature = "test-faults"))]
thread_local! {
    static FAIL_AFTER_RENAMES: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
}

/// Makes commits on this thread fail after renaming the first `renames` of
/// the take's two files (1: the metadata, 2: both), as a failing disk can,
/// and [`finish_interrupted_commit`] fail before it changes anything; `None`
/// lets them succeed again. For tests.
#[cfg(any(test, feature = "test-faults"))]
pub fn fail_commits_after_renames(renames: Option<u8>) {
    FAIL_AFTER_RENAMES.set(renames);
}

/// The failure [`fail_commits_after_renames`] asked for, after `renames`
/// renames of a commit (`None`: when finishing an interrupted one).
#[allow(clippy::unnecessary_wraps)]
fn injected_fault(renames: impl Into<Option<u8>>, path: &Path) -> Result<()> {
    #[cfg(any(test, feature = "test-faults"))]
    if let Some(set) = FAIL_AFTER_RENAMES.get()
        && renames.into().is_none_or(|renames| renames == set)
    {
        return Err(io(path)(std::io::Error::other("the commit failed (test)")));
    }
    #[cfg(not(any(test, feature = "test-faults")))]
    let _ = (renames.into(), path);
    Ok(())
}

/// Finishes the commit of `partial` when an earlier [`commit_take`] renamed
/// its files but failed before appending the `recorded` event. Its partial
/// metadata is then gone, so its `NNN.json` is found by the partial audio's
/// name stored in it (the take's uuid); the audio is renamed when it is
/// still partial, and the event is appended once. `Ok(None)` when the
/// partial metadata still exists: commit it with [`commit_take`]. The
/// caller keeps the audio locked, so [`repair`] leaves the take alone.
pub fn finish_interrupted_commit(dir: &Path, partial: &PartialTake) -> Result<Option<TakeRef>> {
    let _guard = lock(dir)?;
    if partial.meta.exists() {
        return Ok(None);
    }
    let missing = || io(&partial.meta)(std::io::ErrorKind::NotFound.into());
    let wav_name = partial
        .wav
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(missing)?;
    let files = scan(dir)?;
    let found = files
        .numbered
        .iter()
        .filter(|f| !f.is_wav && f.line == partial.line)
        .find_map(|f| {
            let meta = read_meta(&dir.join(&f.path)).ok()?;
            (meta.partial_wav.as_deref() == Some(wav_name)).then_some((f, meta))
        });
    let Some((numbered, meta)) = found else {
        return Err(missing());
    };
    let take = TakeRef {
        line: partial.line.clone(),
        take: numbered.take,
    };
    if ledger::read(dir)?.recorded(&take.line, take.take).is_some() {
        return Ok(Some(take));
    }
    injected_fault(None, &partial.meta)?;
    let wav = dir.join(numbered.path.with_extension("wav"));
    if !wav.exists() {
        fs::rename(&partial.wav, &wav).map_err(io(&wav))?;
    }
    let (sample_rate, samples) = wav_header(&wav)?;
    let event = recorded_event(
        &take.line,
        take.take,
        meta.input,
        sample_rate,
        samples,
        false,
    );
    ledger::append_locked(dir, &event)?;
    Ok(Some(take))
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
            dropouts: input.dropouts,
            film_shift_ms: input.film_shift_ms,
        }),
    )
}

fn read_meta(path: &Path) -> Result<TakeMeta> {
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
    mut file: fs::File,
    samples: &[i32],
    sample_rate: u32,
) -> std::result::Result<fs::File, hound::Error> {
    use std::io::Write as _;
    let mut wav = std::io::Cursor::new(Vec::with_capacity(64 + samples.len() * 3));
    {
        let mut writer = hound::WavWriter::new(&mut wav, take_spec(sample_rate))?;
        for &sample in samples {
            writer.write_sample(sample.clamp(MIN_SAMPLE, FULL_SCALE))?;
        }
        writer.finalize()?;
    }
    let mut wav = wav.into_inner();
    crate::wav::centre_mono(&mut wav);
    file.write_all(&wav)?;
    file.flush()?;
    Ok(file)
}

fn take_spec(sample_rate: u32) -> hound::WavSpec {
    hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    }
}

/// A take's audio streamed into its partial WAV while it is recorded.
///
/// The file is exclusively locked (advisory `flock`) while this lives, so
/// [`repair`] never adopts a take that is still being recorded; the lock
/// ends with the process, so a crash leaves the take adoptable. The header
/// is rewritten by [`PartialWav::flush`], after which the file is a valid
/// WAV up to that point; dropping the writer without [`PartialWav::finish`]
/// leaves the file as a crash would.
#[derive(Debug)]
pub struct PartialWav {
    file: fs::File,
    path: PathBuf,
    /// Offset of the data chunk's length field; the data follows it.
    data_len_at: u64,
    samples: u64,
    flushed: u64,
}

/// Keeps a finished take's audio locked until it is committed.
#[derive(Debug)]
pub struct WavLock {
    _file: fs::File,
}

impl WavLock {
    /// Locks the take audio at `path`, creating an empty file when there is
    /// none, so [`repair`] leaves it alone while the take is written there
    /// (from memory) and committed. Fails when another holder locked it.
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(io(path))?;
        file.try_lock().map_err(|err| match err {
            fs::TryLockError::Error(err) => io(path)(err),
            fs::TryLockError::WouldBlock => io(path)(std::io::ErrorKind::WouldBlock.into()),
        })?;
        Ok(Self { _file: file })
    }
}

impl PartialWav {
    /// Creates `path` (which must not exist), locks it and writes the header
    /// of an empty mono 24-bit WAV.
    pub fn create(path: &Path, sample_rate: u32) -> Result<Self> {
        use std::io::Write as _;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(io(path))?;
        file.try_lock().map_err(|err| match err {
            fs::TryLockError::Error(err) => io(path)(err),
            fs::TryLockError::WouldBlock => io(path)(std::io::ErrorKind::WouldBlock.into()),
        })?;
        // hound's own header, so streamed takes match finished ones.
        let mut header = std::io::Cursor::new(Vec::new());
        hound::WavWriter::new(&mut header, take_spec(sample_rate))
            .and_then(hound::WavWriter::finalize)
            .map_err(wav_error(path))?;
        let mut header = header.into_inner();
        crate::wav::centre_mono(&mut header);
        let mut this = Self {
            data_len_at: header.len() as u64 - 4,
            file,
            path: path.to_path_buf(),
            samples: 0,
            flushed: 0,
        };
        this.file.write_all(&header).map_err(io(path))?;
        Ok(this)
    }

    /// Appends samples, clamped to 24 bits. Not yet durable: see `flush`.
    pub fn write(&mut self, samples: &[i32]) -> Result<()> {
        use std::io::Write as _;
        let bytes: Vec<u8> = samples
            .iter()
            .flat_map(|s| {
                let [a, b, c, _] = s.clamp(&MIN_SAMPLE, &FULL_SCALE).to_le_bytes();
                [a, b, c]
            })
            .collect();
        self.file.write_all(&bytes).map_err(io(&self.path))?;
        self.samples += samples.len() as u64;
        Ok(())
    }

    /// Samples written so far.
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Samples written since the last flush.
    pub fn unflushed(&self) -> u64 {
        self.samples - self.flushed
    }

    /// Rewrites the header for everything written and syncs the data, so the
    /// file is a valid WAV of it even if the process dies next.
    pub fn flush(&mut self) -> Result<()> {
        write_wav_lengths(&self.file, self.data_len_at, self.samples * 3)
            .map_err(io(&self.path))?;
        self.file.sync_data().map_err(io(&self.path))?;
        self.flushed = self.samples;
        Ok(())
    }

    /// Stops writing (after a failed write) but keeps the file locked, so
    /// nothing adopts it before the take is written again from memory.
    pub fn into_lock(self) -> WavLock {
        WavLock { _file: self.file }
    }

    /// Flushes and fsyncs the file; it stays locked until the returned
    /// guard is dropped (after `commit_take`).
    pub fn finish(mut self) -> Result<WavLock> {
        self.sync()?;
        Ok(self.into_lock())
    }

    /// Flushes and fsyncs everything written. On failure the caller keeps
    /// the lock (`into_lock`) while it writes the take again from memory.
    pub fn sync(&mut self) -> Result<()> {
        self.flush()?;
        self.file.sync_all().map_err(io(&self.path))
    }
}

/// Writes the RIFF and data chunk lengths for `data_bytes` of audio whose
/// length field is at `data_len_at`.
fn write_wav_lengths(file: &fs::File, data_len_at: u64, data_bytes: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt as _;
    let too_long = || std::io::Error::other("the take is longer than a WAV file can hold");
    let data = u32::try_from(data_bytes).map_err(|_| too_long())?;
    let riff = u32::try_from(data_len_at + 4 - 8 + data_bytes).map_err(|_| too_long())?;
    file.write_all_at(&riff.to_le_bytes(), 4)?;
    file.write_all_at(&data.to_le_bytes(), data_len_at)
}

/// Sets a WAV's lengths from its size on disk, as left by a crash while it
/// was streamed: the data chunk (the last chunk) runs to the end of the file,
/// in whole sample frames.
fn fix_wav_lengths(file: &fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt as _;
    let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "not a WAV file");
    let read_u32 = |at: u64| -> std::io::Result<u32> {
        let mut word = [0; 4];
        file.read_exact_at(&mut word, at)?;
        Ok(u32::from_le_bytes(word))
    };
    let len = file.metadata()?.len();
    let mut tag = [0; 4];
    file.read_exact_at(&mut tag, 0)?;
    let mut wave = [0; 4];
    file.read_exact_at(&mut wave, 8)?;
    if &tag != b"RIFF" || &wave != b"WAVE" {
        return Err(invalid());
    }
    let mut at = 12;
    let mut block_align = None;
    while at + 8 <= len {
        file.read_exact_at(&mut tag, at)?;
        let size = u64::from(read_u32(at + 4)?);
        if &tag == b"fmt " {
            let mut align = [0; 2];
            file.read_exact_at(&mut align, at + 8 + 12)?;
            block_align = Some(u64::from(u16::from_le_bytes(align)).max(1));
        } else if &tag == b"data" {
            let align = block_align.ok_or_else(invalid)?;
            let data = (len - (at + 8)) / align * align;
            return write_wav_lengths(file, at + 4, data);
        }
        at += 8 + size + size % 2;
    }
    Err(invalid())
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

/// Records a choice of a recorded take of a current line whose file exists.
pub fn choose_take(dir: &Path, line: &str, take: u32, by: &str) -> Result<()> {
    let _guard = lock(dir)?;
    let (_, file) = check_take(dir, line, take)?;
    let path = dir.join(&file);
    if !path.is_file() {
        return Err(Error::MissingFile {
            line: line.to_owned(),
            take,
            path,
        });
    }
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
    let (snapshot, _) = check_take(dir, line, take)?;
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
/// returns the line's current snapshot and the take's file.
fn check_take(dir: &Path, line: &str, take: u32) -> Result<(Snapshot, String)> {
    let project = Project::open(dir)?;
    let snapshot = project
        .snapshot(line)
        .ok_or_else(|| Error::UnknownLine(line.to_owned()))?;
    let file = ledger::read(dir)?
        .recorded(line, take)
        .map(|rec| rec.file.clone())
        .ok_or_else(|| Error::UnknownTake {
            line: line.to_owned(),
            take,
        })?;
    Ok((snapshot, file))
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

    /// `NNN.json` files with neither an `NNN.wav` nor a `recorded` event.
    pub fn unlogged_metadata<'a>(
        &'a self,
        ledger: &'a Ledger,
    ) -> impl Iterator<Item = &'a NumberedFile> {
        self.numbered.iter().filter(|f| {
            !f.is_wav
                && !self.has_wav(&f.line, f.take)
                && ledger.recorded(&f.line, f.take).is_none()
        })
    }

    fn has_wav(&self, line: &str, take: u32) -> bool {
        self.numbered
            .iter()
            .any(|f| f.is_wav && f.line == line && f.take == take)
    }

    fn has_meta(&self, line: &str, take: u32) -> bool {
        self.numbered
            .iter()
            .any(|f| !f.is_wav && f.line == line && f.take == take)
    }
}

/// The partial audio file an unlogged `NNN.json` names, when it still exists:
/// a commit interrupted between its two renames.
pub(crate) fn interrupted_commit(dir: &Path, meta: &NumberedFile) -> Option<PathBuf> {
    let name = read_meta(&dir.join(&meta.path)).ok()?.partial_wav?;
    let is_plain_name = Path::new(&name).file_name() == Some(std::ffi::OsStr::new(&name));
    if !is_plain_name || !name.ends_with(".wav.partial") {
        return None;
    }
    let wav = dir.join(&meta.path).with_file_name(name);
    wav.is_file().then_some(wav)
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

/// How a take's audio file is held.
enum Hold {
    /// Locked here (released when dropped).
    Free(#[allow(dead_code)] fs::File),
    /// Locked by its recording window, which finishes the take itself.
    Recorder,
    /// Not openable for locking; treated as before locks.
    Unopened,
}

/// Locks the audio at `path` without blocking. The recording window's lock
/// follows the file through a commit's renames.
fn hold(path: &Path) -> Hold {
    let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(path) else {
        return Hold::Unopened;
    };
    match file.try_lock() {
        Ok(()) => Hold::Free(file),
        Err(fs::TryLockError::WouldBlock) => Hold::Recorder,
        Err(fs::TryLockError::Error(_)) => Hold::Unopened,
    }
}

/// Adopts `NNN.wav` files that lack a `recorded` event from their `NNN.json`
/// (marked `recovered`), completes commits interrupted between renaming the
/// metadata and the audio, adopts takes whose recording ended without a
/// commit (a crash: an unlocked `.partial` metadata and audio pair), and
/// reports the rest. Audio a recording window still holds locked is left
/// alone: the window finishes that commit itself. Never deletes anything.
pub fn repair(dir: &Path) -> Result<RepairReport> {
    let _guard = lock(dir)?;
    let ledger = ledger::read(dir)?;
    let mut adopted = BTreeSet::new();
    let mut metadata_without_audio = Vec::new();
    // Audio renamed below stays locked until it is adopted: releasing and
    // retaking the lock could find it still held by a process another
    // thread is spawning (a child briefly shares this process's open
    // files), and skip the take as if a recording held it.
    let mut renamed: BTreeMap<PathBuf, Hold> = BTreeMap::new();
    for meta in scan(dir)?.unlogged_metadata(&ledger) {
        match interrupted_commit(dir, meta) {
            Some(partial_wav) => {
                let held = hold(&partial_wav);
                if matches!(held, Hold::Recorder) {
                    continue;
                }
                let rel = meta.path.with_extension("wav");
                let wav = dir.join(&rel);
                fs::rename(&partial_wav, &wav).map_err(io(&wav))?;
                renamed.insert(rel, held);
            }
            None => metadata_without_audio.push(meta.path.clone()),
        }
    }

    for meta in scan(dir)?.partials {
        if let Some(take) = adopt_partial(dir, &meta)? {
            adopted.insert(take);
        }
    }

    let ledger = ledger::read(dir)?;
    let files = scan(dir)?;
    let mut report = RepairReport {
        partials: files.partials.clone(),
        metadata_without_audio,
        ..RepairReport::default()
    };
    for wav in files.unlogged_wavs(&ledger) {
        let held = renamed
            .remove(&wav.path)
            .unwrap_or_else(|| hold(&dir.join(&wav.path)));
        if matches!(held, Hold::Recorder) {
            continue;
        }
        if !files.has_meta(&wav.line, wav.take) {
            report.unadopted.push(wav.path.clone());
            continue;
        }
        let meta = dir.join(&wav.path).with_extension("json");
        let adopt = read_meta(&meta).and_then(|meta| {
            let (sample_rate, samples) = wav_header(&dir.join(&wav.path))?;
            Ok((meta.input, sample_rate, samples))
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
        drop(held);
    }
    report.adopted = adopted.into_iter().collect();
    Ok(report)
}

/// Adopts the take whose partial metadata is `meta` (relative to `dir`)
/// when its partial audio exists, is not locked by a recording, and holds
/// audio: fixes the WAV's lengths from its size and publishes it as
/// `recovered`. The caller holds the project lock.
fn adopt_partial(dir: &Path, meta: &Path) -> Result<Option<TakeRef>> {
    let Some(name) = meta.file_name().and_then(|n| n.to_str()) else {
        return Ok(None);
    };
    if !name.ends_with(".json.partial") {
        return Ok(None);
    }
    let meta = dir.join(meta);
    let Ok(take_meta) = read_meta(&meta) else {
        return Ok(None);
    };
    let Some(wav_name) = take_meta.partial_wav.as_deref() else {
        return Ok(None);
    };
    if Path::new(wav_name).file_name() != Some(std::ffi::OsStr::new(wav_name))
        || !wav_name.ends_with(".wav.partial")
    {
        return Ok(None);
    }
    let wav = meta.with_file_name(wav_name);
    let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(&wav) else {
        return Ok(None);
    };
    // A live recording holds the lock; never touch its files.
    if file.try_lock().is_err() {
        return Ok(None);
    }
    if fix_wav_lengths(&file)
        .and_then(|()| file.sync_all())
        .is_err()
    {
        return Ok(None);
    }
    if !matches!(wav_header(&wav), Ok((_, samples)) if samples > 0) {
        return Ok(None);
    }
    let Some(line) = meta
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
    else {
        return Ok(None);
    };
    let partial = PartialTake {
        line: line.to_owned(),
        meta: meta.clone(),
        wav,
    };
    publish_locked(dir, &partial, take_meta.input, true).map(Some)
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
            dropouts: None,
            film_shift_ms: None,
        }
    }

    #[test]
    fn repair_leaves_a_take_written_under_its_lock() {
        let tmp = crate::testutil::project_tempdir();
        let dir = tmp.path();
        let partial = begin_take(dir, "hook", &input()).unwrap();
        let lock = WavLock::acquire(&partial.wav).unwrap();
        write_take_wav(&partial.wav, &[1000; 480], 48_000).unwrap();
        assert!(WavLock::acquire(&partial.wav).is_err(), "held");
        assert_eq!(repair(dir).unwrap().adopted, Vec::new());
        drop(lock);
        let adopted = repair(dir).unwrap().adopted;
        assert_eq!(adopted.len(), 1, "a crash leaves it adoptable");
    }

    #[test]
    fn a_partly_committed_take_is_left_to_its_recorder_and_finished_once() {
        for renames in [1, 2] {
            let tmp = crate::testutil::project_tempdir();
            let dir = tmp.path();
            let partial = begin_take(dir, "hook", &input()).unwrap();
            let lock = WavLock::acquire(&partial.wav).unwrap();
            write_take_wav(&partial.wav, &[1000; 480], 48_000).unwrap();
            fail_commits_after_renames(Some(renames));
            assert!(commit_take(dir, partial.clone()).is_err(), "{renames}");
            fail_commits_after_renames(None);

            let report = repair(dir).unwrap();
            assert_eq!(report.adopted, Vec::new(), "{renames}: {report:?}");
            assert_eq!(report.unadopted, Vec::<PathBuf>::new(), "{renames}");
            assert!(read(dir).unwrap().events.is_empty(), "{renames}");

            let take = TakeRef {
                line: "hook".to_owned(),
                take: 1,
            };
            for _ in 0..2 {
                let finished = finish_interrupted_commit(dir, &partial).unwrap();
                assert_eq!(finished, Some(take.clone()), "{renames}");
            }
            let events = read(dir).unwrap().events;
            assert_eq!(events.len(), 1, "{renames}: exactly one recorded event");
            let EventKind::Recorded(rec) = &events[0].kind else {
                panic!("{events:?}");
            };
            assert_eq!((rec.samples, rec.recovered), (480, false));
            assert!(dir.join("takes/hook/001.wav").is_file());
            drop(lock);
            assert_eq!(repair(dir).unwrap().adopted, Vec::new());
        }
    }

    #[test]
    fn an_untouched_partial_take_is_not_an_interrupted_commit() {
        let tmp = crate::testutil::project_tempdir();
        let dir = tmp.path();
        let partial = begin_take(dir, "hook", &input()).unwrap();
        write_take_wav(&partial.wav, &[1000; 480], 48_000).unwrap();
        assert_eq!(finish_interrupted_commit(dir, &partial).unwrap(), None);
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
        let tmp = crate::testutil::project_tempdir();
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
        let tmp = crate::testutil::project_tempdir();
        assert!(begin_take(tmp.path(), "../escape", &input()).is_err());
    }

    #[test]
    fn commit_take_numbers_sequentially() {
        let tmp = crate::testutil::project_tempdir();
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
        let tmp = crate::testutil::project_tempdir();
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
        let tmp = crate::testutil::project_tempdir();
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
        assert_eq!(report.unadopted, Vec::<PathBuf>::new());
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

    /// Does what `commit_take` does up to its first rename, then "crashes".
    fn crash_after_metadata_rename(dir: &Path, take: u32) -> PartialTake {
        let partial = begin_take(dir, "hook", &input()).unwrap();
        write_take_wav(&partial.wav, &[7; 480], 48_000).unwrap();
        fs::rename(
            &partial.meta,
            dir.join(format!("takes/hook/{take:03}.json")),
        )
        .unwrap();
        partial
    }

    #[test]
    fn repair_completes_commit_interrupted_between_renames() {
        let tmp = crate::testutil::project_tempdir();
        let dir = tmp.path();
        record(dir, "hook", 10);
        let partial = crash_after_metadata_rename(dir, 2);

        // Until repaired, the number stays taken.
        assert_eq!(record(dir, "hook", 10).take, 3);

        let report = repair(dir).unwrap();
        assert_eq!(
            report.adopted,
            vec![TakeRef {
                line: "hook".to_owned(),
                take: 2
            }]
        );
        assert!(report.partials.is_empty(), "{report:?}");
        assert!(!partial.wav.exists());
        assert_eq!(
            read_take_wav(&dir.join("takes/hook/002.wav")).unwrap(),
            (vec![7; 480], 48_000)
        );
        let ledger = read(dir).unwrap();
        let rec = ledger
            .recorded("hook", 2)
            .expect("recorded event for take 2");
        assert!(rec.recovered);
        assert_eq!(rec.samples, 480);
        assert_eq!(rec.file, "takes/hook/002.wav");
        assert_eq!(rec.snapshot, input().snapshot);

        let again = repair(dir).unwrap();
        assert!(again.adopted.is_empty(), "repair is idempotent");
        assert_eq!(read(dir).unwrap().events.len(), 3);
    }

    /// A process spawned on another thread briefly shares this process's
    /// open files, and with them their locks. `repair` must not release
    /// and retake the lock on the audio it renamed: a lock that lingers in
    /// such a child would look like a recording window's, and the take
    /// would be skipped.
    #[test]
    fn repair_completes_interrupted_commits_while_processes_spawn() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let spawners: Vec<_> = (0..4)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let _ = std::process::Command::new("/usr/bin/true").status();
                    }
                })
            })
            .collect();
        let skipped = (0..100)
            .filter(|_| {
                let tmp = crate::testutil::project_tempdir();
                let dir = tmp.path();
                crash_after_metadata_rename(dir, 1);
                repair(dir).unwrap().adopted.is_empty()
            })
            .count();
        stop.store(true, Ordering::Relaxed);
        for spawner in spawners {
            spawner.join().unwrap();
        }
        assert_eq!(skipped, 0, "takes skipped as if a recording held them");
    }

    #[test]
    fn repair_reports_metadata_without_audio() {
        let tmp = crate::testutil::project_tempdir();
        let dir = tmp.path();
        let partial = crash_after_metadata_rename(dir, 1);
        fs::rename(&partial.wav, dir.join("takes/hook/elsewhere.wav")).unwrap();

        let report = repair(dir).unwrap();
        assert_eq!(report.adopted, []);
        assert_eq!(
            report.metadata_without_audio,
            vec![PathBuf::from("takes/hook/001.json")]
        );
        assert!(dir.join("takes/hook/001.json").exists(), "kept");
        assert_eq!(read(dir).unwrap().events, []);
    }

    #[test]
    fn repair_reports_wav_without_metadata() {
        let tmp = crate::testutil::project_tempdir();
        let dir = tmp.path();
        let line_dir = dir.join("takes/hook");
        fs::create_dir_all(&line_dir).unwrap();
        write_take_wav(&line_dir.join("003.wav"), &[5; 10], 48_000).unwrap();
        let report = repair(dir).unwrap();
        assert_eq!(report.adopted, []);
        assert_eq!(report.unadopted, vec![PathBuf::from("takes/hook/003.wav")]);
        assert!(line_dir.join("003.wav").exists());
        assert_eq!(read(dir).unwrap().events, []);
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
    fn choose_refuses_a_take_whose_file_is_missing() {
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
        record(dir, "hook", 10);
        fs::remove_file(dir.join("takes/hook/001.wav")).unwrap();
        let err = choose_take(dir, "hook", 1, "narrator").unwrap_err();
        assert!(matches!(err, Error::MissingFile { take: 1, .. }), "{err:?}");
        assert_eq!(err.code(), "missing-file");
        assert_eq!(read(dir).unwrap().events.len(), 1, "nothing appended");
    }

    #[test]
    fn repair_keeps_partials_without_a_pair() {
        let tmp = crate::testutil::project_tempdir();
        let dir = tmp.path();
        // Metadata whose audio never started, and audio without metadata.
        let partial = begin_take(dir, "hook", &input()).unwrap();
        let stray = dir.join("takes/hook/.stray.wav.partial");
        write_take_wav(&stray, &[1; 10], 48_000).unwrap();
        let report = repair(dir).unwrap();
        let mut expected = vec![
            partial.meta.strip_prefix(dir).unwrap().to_path_buf(),
            stray.strip_prefix(dir).unwrap().to_path_buf(),
        ];
        expected.sort();
        assert_eq!(report.partials, expected);
        assert!(partial.meta.exists() && stray.exists());
        assert_eq!(read(dir).unwrap().events, []);
    }

    #[test]
    fn partial_wav_streams_and_finishes() {
        let tmp = crate::testutil::project_tempdir();
        let path = tmp.path().join("a.wav.partial");
        let mut wav = PartialWav::create(&path, 48_000).unwrap();
        let samples: Vec<i32> = (0..10_000).map(|i| (i * 37) % 9_000 - 4_500).collect();
        for chunk in samples.chunks(512) {
            wav.write(chunk).unwrap();
        }
        assert_eq!(wav.samples(), 10_000);
        let lock = wav.finish().unwrap();
        assert_eq!(read_take_wav(&path).unwrap(), (samples, 48_000));
        drop(lock);
    }

    #[test]
    fn new_takes_are_tagged_plain_mono() {
        let tmp = crate::testutil::project_tempdir();
        let streamed = tmp.path().join("a.wav.partial");
        let wav = PartialWav::create(&streamed, 48_000).unwrap();
        drop(wav.finish().unwrap());
        let written = tmp.path().join("b.wav");
        write_take_wav(&written, &[1000; 480], 48_000).unwrap();
        for path in [streamed, written] {
            let header = fs::read(&path).unwrap();
            assert_eq!(
                crate::wav::channel_mask(&header),
                Some(crate::wav::FRONT_CENTER),
                "{}",
                path.display()
            );
        }
    }

    #[test]
    fn partial_wav_is_valid_up_to_the_last_flush() {
        let tmp = crate::testutil::project_tempdir();
        let path = tmp.path().join("a.wav.partial");
        let mut wav = PartialWav::create(&path, 48_000).unwrap();
        wav.write(&vec![3; 4800]).unwrap();
        wav.flush().unwrap();
        wav.write(&vec![4; 2400]).unwrap();
        let (samples, rate) = read_take_wav(&path).unwrap();
        assert_eq!((samples.len(), rate), (4800, 48_000));
        drop(wav);
    }

    /// Records into a partial take as the window does, then "crashes":
    /// drops the writer without finishing, after unflushed writes.
    fn crash_mid_take(dir: &Path) -> PartialTake {
        let partial = begin_take(dir, "hook", &input()).unwrap();
        let mut wav = PartialWav::create(&partial.wav, 48_000).unwrap();
        wav.write(&vec![9; 4800]).unwrap();
        wav.flush().unwrap();
        wav.write(&vec![8; 2400]).unwrap();
        drop(wav);
        partial
    }

    #[test]
    fn repair_adopts_a_crashed_partial_take() {
        let tmp = crate::testutil::project_tempdir();
        let dir = tmp.path();
        record(dir, "hook", 10);
        let partial = crash_mid_take(dir);

        let report = repair(dir).unwrap();
        let take_2 = TakeRef {
            line: "hook".to_owned(),
            take: 2,
        };
        assert_eq!(report.adopted, vec![take_2]);
        assert!(report.partials.is_empty(), "{report:?}");
        assert!(!partial.meta.exists() && !partial.wav.exists());
        let (samples, _) = read_take_wav(&dir.join("takes/hook/002.wav")).unwrap();
        assert_eq!(
            samples.len(),
            7200,
            "the header is fixed from the file size"
        );
        assert_eq!(&samples[4800..], &vec![8; 2400][..]);
        let ledger = read(dir).unwrap();
        let rec = ledger
            .recorded("hook", 2)
            .expect("recorded event for take 2");
        assert!(rec.recovered);
        assert_eq!(rec.samples, 7200);
        assert_eq!(rec.snapshot, input().snapshot);
        assert!(dir.join("takes/hook/002.json").is_file());

        let again = repair(dir).unwrap();
        assert!(again.adopted.is_empty(), "repair is idempotent");
    }

    #[test]
    fn repair_never_adopts_a_live_partial_take() {
        let tmp = crate::testutil::project_tempdir();
        let dir = tmp.path();
        let partial = begin_take(dir, "hook", &input()).unwrap();
        let mut wav = PartialWav::create(&partial.wav, 48_000).unwrap();
        wav.write(&vec![9; 4800]).unwrap();
        wav.flush().unwrap();

        let report = repair(dir).unwrap();
        assert_eq!(report.adopted, []);
        assert_eq!(report.partials.len(), 2, "{report:?}");
        assert!(partial.meta.exists() && partial.wav.exists());
        assert_eq!(read(dir).unwrap().events, []);

        // Once recording ends (here: a crash), the take is adopted.
        drop(wav);
        assert_eq!(repair(dir).unwrap().adopted.len(), 1);
    }

    #[test]
    fn dropouts_reach_the_recorded_event_only_when_known() {
        let tmp = crate::testutil::project_tempdir();
        let dir = tmp.path();
        record(dir, "hook", 10);
        let partial = begin_take(dir, "hook", &input()).unwrap();
        write_take_wav(&partial.wav, &[1; 10], 48_000).unwrap();
        let counted = RecordedInput {
            dropouts: Some(2),
            ..input()
        };
        update_take_meta(&partial, &counted).unwrap();
        commit_take(dir, partial).unwrap();
        let ledger = read(dir).unwrap();
        assert_eq!(ledger.recorded("hook", 1).unwrap().dropouts, None);
        assert_eq!(ledger.recorded("hook", 2).unwrap().dropouts, Some(2));
        let text = fs::read_to_string(dir.join("takes.jsonl")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(!lines[0].contains("dropouts"), "{}", lines[0]);
        assert!(lines[1].contains("\"dropouts\":2"), "{}", lines[1]);
    }
}
