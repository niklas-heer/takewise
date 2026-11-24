//! Passages: several consecutive lines recorded in one go, kept whole, and
//! cut into one ordinary take per line.
//!
//! The capture of a passage streams into
//! `takes/.passages/.<uuid>.wav.partial`, beside a sidecar
//! `.<uuid>.json.partial` that says what was recorded and where its first
//! sample lies on the film. On stop the cuts between the lines are computed
//! ([`cut`]) and written into the sidecar, and both files are renamed to
//! `<uuid>.wav` and `<uuid>.json` ([`publish_passage`]): from then on the
//! passage is a published file that nothing changes. [`split`] then writes
//! each piece as a take of its line (`takes/<line>/NNN.wav`, `NNN.json` and
//! a `recorded` event), carrying a [`PassagePiece`] that says where in the
//! passage it came from. `split` can be called again at any time: it skips
//! the pieces that already have a take.
//!
//! Nothing here changes or removes a passage or a take. The only files
//! `split` ever removes are its own scratch files before a piece is
//! committed: the empty file it claims a piece with, and a piece's partial
//! metadata whose audio is not whole.
//!
//! The sidecar has its own schema, marked `"kind": "passage"`, so it is
//! never read as a take's metadata, and a take's never as a passage's.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::check::{Boundary, PieceContext};
use crate::error::{Error, Result, io};
use crate::fsutil::{LockedFile, lock, write_atomic};
use crate::ledger::{self, Chosen, Event, EventKind, Ledger, PassagePiece, Snapshot};
use crate::project::{Project, TAKES_DIR};
use crate::script::{Problem, is_valid_id};
use crate::speech::{analyze, analyze_with_floor, frame_len};
use crate::takes::{
    Hold, PartialTake, PassageFailure, RecordedInput, RepairReport, TakeRef, WavLock,
    existing_piece_locked, fix_wav_lengths, hold, injected_fault, line_dir, publish_locked,
    read_take_wav, rewrite_take_wav, wav_header, write_partial_meta, write_synced, write_take_wav,
};
use crate::timing::Dropout;

/// The directory under `takes/` that holds the passages.
pub const PASSAGES_DIR: &str = ".passages";

/// A pause at a boundary must last this long to be the cut without doubt.
const PAUSE_FRAMES: usize = 15;
/// A pause this long is still a pause between words, not a stop closure
/// inside one (the silence before a p, t or k, up to about 60 ms).
const SHORT_PAUSE_FRAMES: usize = 8;
/// How far from its cue a long pause is looked for (in 10 ms frames).
const NEAR_CUE_FRAMES: usize = 100;

/// Marks a sidecar as a passage's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Passage,
}

/// One line of a passage, as it was when the take started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassageLine {
    pub id: String,
    pub snapshot: Snapshot,
}

/// The latency the passage's samples were placed with.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Latency {
    /// The device's input latency, safety offset and stream latency, in
    /// seconds.
    pub input_s: f64,
    /// The calibrated `latency_offset_ms` in effect.
    pub offset_ms: f64,
}

/// What the recording knows of a passage: everything in its sidecar but
/// the cuts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassageInput {
    pub sample_rate: u32,
    pub device: String,
    pub channel: u16,
    pub latency: Latency,
    /// The configured pre-roll: the passage's first `preroll_s` seconds
    /// give the noise floor.
    pub preroll_s: f64,
    /// The lines to be read, in script order, snapshotted when the take
    /// started.
    pub lines: Vec<PassageLine>,
    /// Film time of the passage's first sample, once the film is scheduled
    /// and the sample placed. Without it the passage cannot be cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_sample_film_s: Option<f64>,
    /// The gaps in the captured audio (filled with silence), once known
    /// (when the take is stopped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropouts: Option<Vec<Dropout>>,
    /// As a take's `film_shift_ms`, for the whole passage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub film_shift_ms: Option<f64>,
}

/// A cut between two lines of a passage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cut {
    /// The first sample of the piece after the cut.
    pub sample: u64,
    /// The film time the next line was cued at: its start.
    pub cue_film_s: f64,
    /// Whether the cut lies in a pause. Without one the cut is at the cue,
    /// and the words around it may be split between the two lines.
    pub pause: bool,
}

/// A passage's sidecar, `<uuid>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Passage {
    pub kind: Kind,
    pub id: String,
    #[serde(flatten)]
    pub input: PassageInput,
    /// The passage's length in samples, once cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples: Option<u64>,
    /// The noise floor of the passage's pre-roll in dBFS, once cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preroll_floor_dbfs: Option<f64>,
    /// The cuts between the pieces, once computed: one fewer than pieces.
    /// Piece `i` is line `i`'s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cuts: Option<Vec<Cut>>,
    /// The recording ended without a commit (a crash) and was finished by
    /// a repair; its takes are marked `recovered` too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recovered: bool,
    /// `.<uuid>.wav.partial`, the file the audio was recorded into, so a
    /// publish interrupted between its two renames can be completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    partial_wav: Option<String>,
}

impl Passage {
    /// The passage's audio, relative to the project directory.
    pub fn file(&self) -> String {
        passage_file(&self.id)
    }

    /// The sample range of each piece in the passage file, in order, piece
    /// `i` being line `i`'s; `None` until the passage is cut, and for cuts
    /// that do not divide the passage into one piece for each of its first
    /// lines (a sidecar changed by hand).
    pub fn pieces(&self) -> Option<Vec<Range<u64>>> {
        let (cuts, samples) = (self.cuts.as_ref()?, self.samples?);
        if cuts.len() >= self.input.lines.len() {
            return None;
        }
        let mut pieces = Vec::with_capacity(cuts.len() + 1);
        let mut from = 0;
        for cut in cuts {
            if cut.sample <= from {
                return None;
            }
            pieces.push(from..cut.sample);
            from = cut.sample;
        }
        if samples <= from {
            return None;
        }
        pieces.push(from..samples);
        Some(pieces)
    }

    /// The lines that got no piece: the capture never reached them, or
    /// stopped before anything of them was spoken.
    pub fn lines_without_piece(&self) -> &[PassageLine] {
        let pieces = self.cuts.as_ref().map_or(0, |cuts| cuts.len() + 1);
        self.input.lines.get(pieces..).unwrap_or_default()
    }
}

/// `takes/.passages/<id>.wav`, the audio of passage `id` relative to the
/// project directory.
pub fn passage_file(id: &str) -> String {
    format!("{TAKES_DIR}/{PASSAGES_DIR}/{id}.wav")
}

fn passages_dir(dir: &Path) -> PathBuf {
    dir.join(TAKES_DIR).join(PASSAGES_DIR)
}

/// Whether `id` can name a passage's files: a uuid's characters only.
fn is_valid_passage_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

fn published_paths(dir: &Path, id: &str) -> Result<(PathBuf, PathBuf)> {
    if !is_valid_passage_id(id) {
        return Err(Error::UnknownPassage(id.to_owned()));
    }
    let passages = passages_dir(dir);
    Ok((
        passages.join(format!("{id}.json")),
        passages.join(format!("{id}.wav")),
    ))
}

/// A passage being recorded: write the audio to `wav`, then
/// [`publish_passage`].
#[derive(Debug, Clone, PartialEq)]
pub struct PartialPassage {
    pub id: String,
    pub meta: PathBuf,
    pub wav: PathBuf,
}

/// Starts a passage: writes its sidecar to
/// `takes/.passages/.<uuid>.json.partial` and returns where to stream the
/// audio (with [`crate::PartialWav`], as for a take). Needs no lock.
pub fn begin_passage(dir: &Path, input: &PassageInput) -> Result<PartialPassage> {
    if let Some(bad) = input.lines.iter().find(|line| !is_valid_id(&line.id)) {
        return Err(Error::UnknownLine(bad.id.clone()));
    }
    let passages = passages_dir(dir);
    fs::create_dir_all(&passages).map_err(io(&passages))?;
    let id = uuid::Uuid::new_v4().to_string();
    let partial = PartialPassage {
        meta: passages.join(format!(".{id}.json.partial")),
        wav: passages.join(format!(".{id}.wav.partial")),
        id,
    };
    let json = to_json(&partial.meta, &sidecar(&partial, input))?;
    write_synced(&partial.meta, &json)?;
    Ok(partial)
}

/// Replaces a partial passage's sidecar with `input`, such as the first
/// sample's film time once the film's timebase was read back, and the
/// dropouts once counted. Atomic: a crash leaves the old or the new file.
/// Cuts computed before are dropped; [`publish_passage`] computes them.
pub fn update_passage_meta(partial: &PartialPassage, input: &PassageInput) -> Result<()> {
    let json = to_json(&partial.meta, &sidecar(partial, input))?;
    write_atomic(&partial.meta, &json)
}

fn sidecar(partial: &PartialPassage, input: &PassageInput) -> Passage {
    Passage {
        kind: Kind::Passage,
        id: partial.id.clone(),
        input: input.clone(),
        samples: None,
        preroll_floor_dbfs: None,
        cuts: None,
        recovered: false,
        partial_wav: partial
            .wav
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
    }
}

/// What [`begin_passage`] writes beside the partial audio of the passage
/// `id`, as JSON, for a copy of the passage kept elsewhere (see
/// [`crate::rescue`]); `path` is the file an error names.
pub(crate) fn partial_sidecar_json(
    path: &Path,
    id: &str,
    input: &PassageInput,
) -> Result<serde_json::Value> {
    let partial = PartialPassage {
        id: id.to_owned(),
        meta: PathBuf::from(format!(".{id}.json.partial")),
        wav: PathBuf::from(format!(".{id}.wav.partial")),
    };
    serde_json::to_value(sidecar(&partial, input)).map_err(|err| Error::Parse {
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

fn to_json(path: &Path, passage: &Passage) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(passage).map_err(|err| Error::Parse {
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

fn read_sidecar(path: &Path) -> Result<Passage> {
    let bytes = fs::read(path).map_err(io(path))?;
    serde_json::from_slice(&bytes).map_err(|err| Error::Parse {
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

/// Reads the sidecar of the published passage `id`.
pub fn read_passage(dir: &Path, id: &str) -> Result<Passage> {
    let (meta, _) = published_paths(dir, id)?;
    let passage = read_sidecar(&meta).map_err(|err| match err {
        Error::Io { source, .. } if source.kind() == std::io::ErrorKind::NotFound => {
            Error::UnknownPassage(id.to_owned())
        }
        other => other,
    })?;
    if passage.id != id {
        return Err(Error::Parse {
            path: meta,
            message: format!("the sidecar is of passage {:?}", passage.id),
        });
    }
    Ok(passage)
}

/// Publishes a stopped passage whose partial audio is finished and synced:
/// computes the cuts, writes them into the sidecar, and renames the sidecar
/// to `<uuid>.json` and the audio to `<uuid>.wav`. The renames are the
/// commit point: from here on the passage is a published file, and
/// [`split`] derives its takes. The caller keeps the audio locked until
/// this returns, so a repair leaves the passage alone.
///
/// Safe to call again after a failure: a publish that renamed the sidecar
/// and then failed is completed. Fails with `passage-unplaced` when the
/// sidecar has no film time for the first sample (nothing says where to
/// cut), and when the audio is empty.
pub fn publish_passage(dir: &Path, partial: &PartialPassage) -> Result<Passage> {
    let (meta, wav) = published_paths(dir, &partial.id)?;
    if partial.meta.exists() {
        let mut passage = read_sidecar(&partial.meta)?;
        if passage.cuts.is_none() {
            let audio = if wav.exists() { &wav } else { &partial.wav };
            cut_into(&mut passage, audio)?;
            write_atomic(&partial.meta, &to_json(&partial.meta, &passage)?)?;
        }
        fs::rename(&partial.meta, &meta).map_err(io(&meta))?;
        injected_fault(1, &meta)?;
    }
    if !wav.exists() {
        fs::rename(&partial.wav, &wav).map_err(io(&wav))?;
    }
    injected_fault(2, &wav)?;
    read_sidecar(&meta)
}

/// Computes the cuts of `passage` from its audio at `wav`.
fn cut_into(passage: &mut Passage, wav: &Path) -> Result<()> {
    let first = passage
        .input
        .first_sample_film_s
        .ok_or_else(|| Error::PassageUnplaced(wav.to_path_buf()))?;
    let (samples, rate) = read_take_wav(wav)?;
    if samples.is_empty() {
        return Err(Error::Parse {
            path: wav.to_path_buf(),
            message: "the passage has no audio".to_owned(),
        });
    }
    let lines: Vec<Snapshot> = passage
        .input
        .lines
        .iter()
        .map(|line| line.snapshot.clone())
        .collect();
    let cuts = cut(&samples, rate, first, &lines, passage.input.preroll_s);
    passage.samples = Some(samples.len() as u64);
    passage.preroll_floor_dbfs = Some(cuts.preroll_floor_dbfs);
    passage.cuts = Some(cuts.cuts);
    Ok(())
}

/// A stopped passage that is not published yet: what the recording window
/// keeps until [`save_passage`] succeeds.
#[derive(Debug)]
pub struct UnsavedPassage {
    /// The sidecar's content, with the first sample's film time.
    pub input: PassageInput,
    /// Every captured sample, kept until the passage is published.
    pub samples: Vec<i32>,
    /// The partial files, when the passage was begun on disk.
    pub partial: Option<PartialPassage>,
    /// The partial audio's lock, held from the recording or taken here.
    pub lock: Option<WavLock>,
    /// Whether the partial audio holds every sample, finished and synced.
    pub complete: bool,
}

impl UnsavedPassage {
    /// Whether part of the passage's audio exists only in memory: the
    /// partial audio on disk does not hold every sample (it could not be
    /// begun, or a write failed), so quitting now would lose what a repair
    /// cannot find there. `false` once the audio is whole on disk, also
    /// when the passage is not published yet: a repair then finishes it.
    pub fn audio_only_in_memory(&self) -> bool {
        !self.complete
    }
}

/// Writes a stopped passage's sidecar and, when its partial audio is not
/// complete (it could not be begun or written on disk), its audio from
/// memory, then publishes it. On failure `unsaved` keeps everything, so
/// calling this again retries. The audio is written from memory into a new
/// file that replaces the partial audio only once it is whole (see
/// [`rewrite_take_wav`]), so what the recording streamed is never
/// shortened; it stays locked throughout, and until the passage is
/// published.
pub fn save_passage(dir: &Path, unsaved: &mut UnsavedPassage) -> Result<Passage> {
    let partial = if let Some(partial) = &unsaved.partial {
        partial.clone()
    } else {
        let partial = begin_passage(dir, &unsaved.input)?;
        unsaved.partial = Some(partial.clone());
        partial
    };
    let (meta, wav) = published_paths(dir, &partial.id)?;
    if !wav.exists() {
        // The partial audio vanished: the lock held is on the removed
        // file, so the audio is written again under a new one.
        if !partial.wav.exists() {
            unsaved.lock = None;
            unsaved.complete = false;
        }
        if !unsaved.complete {
            // Never over the audio that is there: a failure here leaves
            // what the recording streamed, for a repair after a crash.
            rewrite_take_wav(
                &partial.wav,
                &mut unsaved.lock,
                &unsaved.samples,
                unsaved.input.sample_rate,
            )?;
            unsaved.complete = true;
        }
    }
    // An earlier publish that renamed the sidecar keeps it as it is.
    if partial.meta.exists() || !meta.exists() {
        update_passage_meta(&partial, &unsaved.input)?;
    }
    publish_passage(dir, &partial)
}

/// The cuts of a passage and the noise floor they were found with.
#[derive(Debug, Clone, PartialEq)]
pub struct Cuts {
    /// One cut per boundary between two pieces, strictly increasing.
    pub cuts: Vec<Cut>,
    /// The 20th percentile of the frame levels in the passage's pre-roll.
    pub preroll_floor_dbfs: f64,
}

#[allow(clippy::cast_possible_truncation)]
fn to_sample(seconds: f64, rate: f64) -> i64 {
    (seconds * rate).round() as i64
}

fn within(position: i64, lo: usize, hi: usize) -> usize {
    usize::try_from(position.max(0)).map_or(hi, |p| p.clamp(lo, hi))
}

/// Cuts a passage into one piece per line.
///
/// `samples` is the whole passage, whose first sample lies at
/// `first_sample_film_s`; `lines` are the snapshots of its lines, in order.
/// Each line whose start the capture reached gets one piece; the pieces are
/// contiguous and cover the passage exactly, so the cuts returned are one
/// fewer than the pieces, piece `i` being line `i`'s.
///
/// For the boundary between line *k* and line *k+1*:
///
/// 1. The search window runs from the midpoint of line *k*'s slot to the
///    midpoint of line *k+1*'s, clamped to the captured range. A slot that
///    runs past the next line's start (overlapping slots) counts only up
///    to that start, so the windows follow each other in the lines' order,
///    each around its own cue, and two boundaries never search the same
///    audio. A window also begins after the cut before it; a boundary left
///    without any audio that way (two lines starting on the same sample)
///    ends the cutting, and the lines from there on get no piece.
/// 2. Pauses are runs of 10 ms frames, inside the window, at or below the
///    energy detector's speech threshold, which comes from the noise floor
///    of the passage's pre-roll (its first `preroll_s` seconds).
/// 3. The cut is the middle of: the pause that contains the cue gap (from
///    line *k*'s slot end to line *k+1*'s start), or else the cue (line
///    *k+1*'s start), if it lasts at least 150 ms; else the longest pause
///    of at least 150 ms within 1 s of the cue; else the nearest pause of
///    at least 80 ms in the window; else the cue itself (or the end of the
///    window nearest to it), marked `pause: false`. Every cut lies in its
///    own window, and `pause` says whether that very sample is in a pause.
/// 4. A trailing piece without speech (the take was stopped right after a
///    line began) joins the piece before it, and its line gets no piece.
pub fn cut(
    samples: &[i32],
    sample_rate: u32,
    first_sample_film_s: f64,
    lines: &[Snapshot],
    preroll_s: f64,
) -> Cuts {
    let analysis = analyze(samples, sample_rate, preroll_s);
    let floor = analysis.noise_floor_dbfs;
    if samples.is_empty() {
        // Nothing to cut, and no sample a boundary could lie at.
        return Cuts {
            cuts: Vec::new(),
            preroll_floor_dbfs: floor,
        };
    }
    let frames = &analysis.frames;
    let silent: Vec<bool> = frames
        .levels_dbfs
        .iter()
        .map(|&level| level <= frames.threshold_dbfs)
        .collect();
    let frame = frame_len(sample_rate);
    let len = samples.len();
    let rate = f64::from(sample_rate);
    let position = |film_s: f64| to_sample(film_s - first_sample_film_s, rate);

    // The middle of line `k`'s slot, the slot ending at the next line's
    // start at the latest.
    let middle = |k: usize| {
        let (line, next) = (lines.get(k)?, lines.get(k + 1));
        let end = next.map_or(line.end, |next| line.end.min(next.start));
        Some(position(f64::midpoint(line.start, end.max(line.start))))
    };
    let mut cuts: Vec<Cut> = Vec::new();
    let mut previous = 0;
    for (k, pair) in lines.windows(2).enumerate() {
        let [before, after] = pair else { break };
        let cue = position(after.start);
        if usize::try_from(cue).is_ok_and(|cue| cue >= len) {
            // The capture never reached this line.
            break;
        }
        let (Some(from), Some(to)) = (middle(k), middle(k + 1)) else {
            break;
        };
        // `previous < len`, so the window can begin after the cut before.
        let lo = within(from, previous + 1, len);
        let hi = within(to, 0, len);
        if hi < lo {
            // No audio is left for this boundary.
            break;
        }
        let gap_from = position(before.end.min(after.start));
        let (sample, pause) = choose_cut(&silent, frame, lo..hi, gap_from, cue);
        let in_window = (lo..=hi).contains(&sample);
        let in_pause = pause
            .as_ref()
            .is_none_or(|pause| lo <= pause.start && pause.end <= hi && pause.contains(&sample));
        debug_assert!(
            in_window && in_pause,
            "cut {sample} of {lo}..{hi}, {pause:?}"
        );
        if !(in_window && in_pause) || sample >= len {
            break;
        }
        cuts.push(Cut {
            sample: sample as u64,
            cue_film_s: after.start,
            pause: pause.is_some(),
        });
        previous = sample;
    }
    while let Some(last) = cuts.last() {
        let from = usize::try_from(last.sample).unwrap_or(len).min(len);
        let trailing = samples.get(from..).unwrap_or_default();
        if analyze_with_floor(trailing, sample_rate, floor)
            .speech
            .is_some()
        {
            break;
        }
        cuts.pop();
    }
    Cuts {
        cuts,
        preroll_floor_dbfs: floor,
    }
}

/// The pauses inside `window` (in samples): maximal runs of silent frames
/// lying wholly in it, as sample ranges.
fn pauses_in(silent: &[bool], frame: usize, window: &Range<usize>) -> Vec<Range<usize>> {
    let first = window.start.div_ceil(frame);
    let end = (window.end / frame).min(silent.len());
    let mut pauses = Vec::new();
    let mut run: Option<usize> = None;
    for idx in first..end {
        let quiet = silent.get(idx).copied().unwrap_or(false);
        match (quiet, run) {
            (true, None) => run = Some(idx),
            (false, Some(from)) => {
                pauses.push(from * frame..idx * frame);
                run = None;
            }
            _ => {}
        }
    }
    if let Some(from) = run {
        pauses.push(from * frame..end * frame);
    }
    pauses
}

/// Where to cut inside `window`, and the pause that sample is the middle
/// of, when it is in one; `gap_from` and `cue` are the positions of the cue
/// gap's start (the slot end of the line before) and of the cue (the start
/// of the line after).
fn choose_cut(
    silent: &[bool],
    frame: usize,
    window: Range<usize>,
    gap_from: i64,
    cue: i64,
) -> (usize, Option<Range<usize>>) {
    let pauses = pauses_in(silent, frame, &window);
    let signed = |sample: usize| i64::try_from(sample).unwrap_or(i64::MAX);
    let contains =
        |pause: &Range<usize>, at: i64| signed(pause.start) <= at && at <= signed(pause.end);
    let distance = |pause: &Range<usize>| {
        (signed(pause.start) - cue)
            .max(cue - signed(pause.end))
            .max(0)
    };
    let middle =
        |pause: &Range<usize>| (usize::midpoint(pause.start, pause.end), Some(pause.clone()));
    let long = |pause: &&Range<usize>| pause.len() >= PAUSE_FRAMES * frame;

    let at_cue = pauses
        .iter()
        .find(|pause| gap_from < cue && contains(pause, gap_from) && contains(pause, cue))
        .or_else(|| pauses.iter().find(|pause| contains(pause, cue)));
    if let Some(pause) = at_cue.filter(long) {
        return middle(pause);
    }
    let near = signed(NEAR_CUE_FRAMES * frame);
    let longest_near = pauses
        .iter()
        .filter(long)
        .filter(|pause| distance(pause) <= near)
        .max_by(|a, b| a.len().cmp(&b.len()).then(distance(b).cmp(&distance(a))));
    if let Some(pause) = longest_near {
        return middle(pause);
    }
    let nearest = pauses
        .iter()
        .filter(|pause| pause.len() >= SHORT_PAUSE_FRAMES * frame)
        .min_by(|a, b| distance(a).cmp(&distance(b)).then(b.len().cmp(&a.len())));
    if let Some(pause) = nearest {
        return middle(pause);
    }
    (within(cue, window.start, window.end), None)
}

/// What [`split`] did.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Split {
    /// The take of every piece that has one, in piece order.
    pub takes: Vec<TakeRef>,
    /// The takes among them that this call wrote.
    pub created: Vec<TakeRef>,
    /// Pieces (by index) another process is writing right now; calling
    /// `split` again finds their takes.
    pub pending: Vec<u32>,
    /// The passage's lines that got no piece: never reached, or not spoken.
    pub lines_without_piece: Vec<String>,
}

/// Derives one ordinary take per piece of the published passage `id`:
/// `takes/<line>/NNN.wav`, `NNN.json` and a `recorded` event, committed
/// like any take under the project lock. Each take's `line_offset_s` comes
/// from its piece's first sample, and its `passage` says which piece it is.
///
/// Idempotent: a piece whose (passage id, index) already has a `recorded`
/// event is skipped, also one adopted by a repair after a crash; a piece
/// whose commit renamed its files and stopped before the event is finished
/// instead of written again. The passage file is never changed.
pub fn split(dir: &Path, id: &str) -> Result<Split> {
    let (meta_path, wav_path) = published_paths(dir, id)?;
    let passage = read_passage(dir, id)?;
    let not_cut = || Error::Parse {
        path: meta_path.clone(),
        message: "the passage's sidecar does not say how to cut it".to_owned(),
    };
    let ranges = passage.pieces().ok_or_else(not_cut)?;
    let first_film_s = passage.input.first_sample_film_s.ok_or_else(not_cut)?;
    let floor = passage.preroll_floor_dbfs.ok_or_else(not_cut)?;
    let count = u32::try_from(ranges.len()).unwrap_or(u32::MAX);
    let mut report = Split {
        lines_without_piece: passage
            .lines_without_piece()
            .iter()
            .map(|line| line.id.clone())
            .collect(),
        ..Split::default()
    };
    // Read only when a piece is missing.
    let mut audio: Option<Vec<i32>> = None;
    for (index, (range, line)) in (0..count).zip(ranges.iter().zip(&passage.input.lines)) {
        let piece = PassagePiece {
            id: passage.id.clone(),
            index,
            count,
            from_sample: range.start,
            to_sample: range.end,
            preroll_floor_dbfs: floor,
        };
        let line_dir = line_dir(dir, &line.id)?;
        fs::create_dir_all(&line_dir).map_err(io(&line_dir))?;
        let partial = PartialTake {
            line: line.id.clone(),
            meta: line_dir.join(format!(".{id}-{index:03}.json.partial")),
            wav: line_dir.join(format!(".{id}-{index:03}.wav.partial")),
        };
        // The piece's partial audio is its claim: whoever holds its lock
        // writes the piece, and a repair leaves the files alone meanwhile.
        let claim = match WavLock::acquire(&partial.wav) {
            Ok(claim) => claim,
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::WouldBlock => {
                report.pending.push(index);
                continue;
            }
            Err(err) => return Err(err),
        };
        let existing = {
            let _guard = lock(dir)?;
            existing_piece_locked(dir, &line.id, &piece, &partial.wav)
        };
        let written = match existing {
            Ok(Some(take)) => Ok((take, false)),
            Ok(None) => {
                let start_film_s =
                    first_film_s + range.start as f64 / f64::from(passage.input.sample_rate);
                let input = RecordedInput {
                    sample_rate: passage.input.sample_rate,
                    device: passage.input.device.clone(),
                    channel: passage.input.channel,
                    line_offset_s: start_film_s - line.snapshot.start,
                    snapshot: line.snapshot.clone(),
                    dropouts: passage
                        .input
                        .dropouts
                        .as_ref()
                        .map(|dropouts| dropouts_in(dropouts, range)),
                    film_shift_ms: passage.input.film_shift_ms,
                    passage: Some(piece),
                };
                passage_audio(&mut audio, &wav_path, &passage)
                    .and_then(|samples| {
                        write_piece(dir, &partial, samples, input, passage.recovered)
                    })
                    .map(|take| (take, true))
            }
            Err(err) => Err(err),
        };
        // The claim made an empty file when there was none; a piece that
        // was not written here leaves it behind.
        remove_if_empty(&partial.wav);
        drop(claim);
        let (take, created) = written?;
        report.takes.push(take.clone());
        if created {
            report.created.push(take);
        }
    }
    Ok(report)
}

/// The passage's audio, read once and checked against its sidecar.
fn passage_audio<'a>(
    audio: &'a mut Option<Vec<i32>>,
    wav: &Path,
    passage: &Passage,
) -> Result<&'a [i32]> {
    if audio.is_none() {
        let (samples, rate) = read_take_wav(wav)?;
        if rate != passage.input.sample_rate || Some(samples.len() as u64) != passage.samples {
            return Err(Error::Parse {
                path: wav.to_path_buf(),
                message: "the passage's audio does not match its sidecar".to_owned(),
            });
        }
        *audio = Some(samples);
    }
    Ok(audio.as_deref().unwrap_or_default())
}

/// Writes one piece as a partial take and commits it; `samples` is the
/// whole passage. The caller holds the lock on `partial.wav`.
fn write_piece(
    dir: &Path,
    partial: &PartialTake,
    samples: &[i32],
    input: RecordedInput,
    recovered: bool,
) -> Result<TakeRef> {
    let range = input
        .passage
        .as_ref()
        .map_or(0..0, |piece| piece.from_sample..piece.to_sample);
    // The audio is written whole before its metadata exists, and the
    // metadata is what a repair adopts a partial take by: a crash in
    // between leaves nothing that could be adopted half written.
    let whole = partial.meta.exists()
        && matches!(wav_header(&partial.wav), Ok((_, samples)) if samples == range.end - range.start);
    if !whole {
        if partial.meta.exists() {
            fs::remove_file(&partial.meta).map_err(io(&partial.meta))?;
        }
        let piece = samples
            .get(to_index(range.start)..to_index(range.end))
            .unwrap_or_default();
        write_take_wav(&partial.wav, piece, input.sample_rate)?;
    }
    write_partial_meta(partial, &input)?;
    let _guard = lock(dir)?;
    publish_locked(dir, partial, input, recovered)
}

fn to_index(sample: u64) -> usize {
    usize::try_from(sample).unwrap_or(usize::MAX)
}

/// How many of the passage's `dropouts` fall into the piece `range`.
fn dropouts_in(dropouts: &[Dropout], range: &Range<u64>) -> u32 {
    let inside = dropouts
        .iter()
        .filter(|d| d.at_sample < range.end && d.at_sample + d.missing.max(1) > range.start)
        .count();
    u32::try_from(inside).unwrap_or(u32::MAX)
}

/// Removes the empty file a claim left when its piece was already written.
fn remove_if_empty(path: &Path) {
    if fs::metadata(path).is_ok_and(|meta| meta.len() == 0) {
        let _ = fs::remove_file(path);
    }
}

/// Where a take cut from a passage lies in it, for the timing rules of its
/// check: whether it is the first or last piece, and whether the cuts
/// around it were made without a pause (from the passage's sidecar; a
/// sidecar that cannot be read counts as cut in pauses).
pub fn piece_context(dir: &Path, piece: &PassagePiece) -> PieceContext {
    let cuts = read_passage(dir, &piece.id)
        .ok()
        .and_then(|passage| passage.cuts)
        .unwrap_or_default();
    let unpaused = |cut: Option<usize>| {
        cut.and_then(|idx| cuts.get(idx))
            .is_some_and(|cut| !cut.pause)
    };
    let index = usize::try_from(piece.index).unwrap_or(usize::MAX);
    PieceContext {
        first: piece.is_first(),
        last: piece.is_last(),
        unpaused: Boundary::of(unpaused(index.checked_sub(1)), unpaused(Some(index))),
    }
}

/// A line [`choose_passage`] left as it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkippedLine {
    pub line: String,
    pub reason: String,
}

/// What [`choose_passage`] chose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PassageChoice {
    pub passage: String,
    pub by: String,
    /// The takes chosen, one per line, in piece order.
    pub chosen: Vec<TakeRef>,
    /// Lines of the passage whose choice was left alone, and why.
    pub skipped: Vec<SkippedLine>,
}

/// Chooses the piece of passage `id` for every line that has one, so the
/// passage is used whole. Under the project lock it checks that every
/// piece has a take and that each take's file exists, then adds all the
/// `chosen` events at once, in a copy of the ledger renamed over it:
/// either every line is chosen or none is, also when the write fails
/// midway.
///
/// Lines of the passage without a piece, and pieces whose line is no
/// longer in the script, are skipped and keep their choice. Fails with
/// `unknown-passage` when no take belongs to `id`, `passage-not-split` when
/// pieces still lack their takes, and `missing-file` for a missing take.
pub fn choose_passage(dir: &Path, id: &str, by: &str) -> Result<PassageChoice> {
    published_paths(dir, id)?;
    let _guard = lock(dir)?;
    let project = Project::open(dir)?;
    let ledger = ledger::read(dir)?;
    let mut pieces: BTreeMap<u32, (&str, u32)> = BTreeMap::new();
    let mut count = 0;
    for (line, take, piece) in ledger.pieces(id) {
        pieces.entry(piece.index).or_insert((line, take));
        count = count.max(piece.count);
    }
    if pieces.is_empty() {
        return Err(Error::UnknownPassage(id.to_owned()));
    }
    let have = u32::try_from(pieces.len()).unwrap_or(u32::MAX);
    if have < count {
        return Err(Error::PassageNotSplit {
            id: id.to_owned(),
            have,
            count,
        });
    }
    let mut choice = PassageChoice {
        passage: id.to_owned(),
        by: by.to_owned(),
        chosen: Vec::new(),
        skipped: Vec::new(),
    };
    let mut events = Vec::new();
    for &(line, take) in pieces.values() {
        if project.script.position(line).is_none() {
            choice.skipped.push(SkippedLine {
                line: line.to_owned(),
                reason: "the line is no longer in the script".to_owned(),
            });
            continue;
        }
        let file = ledger.recorded(line, take).map(|rec| dir.join(&rec.file));
        if let Some(path) = file.filter(|path| !path.is_file()) {
            return Err(Error::MissingFile {
                line: line.to_owned(),
                take,
                path,
            });
        }
        events.push(Event::new(
            line,
            take,
            EventKind::Chosen(Chosen { by: by.to_owned() }),
        ));
        choice.chosen.push(TakeRef {
            line: line.to_owned(),
            take,
        });
    }
    if let Ok(passage) = read_passage(dir, id) {
        for line in passage.lines_without_piece() {
            choice.skipped.push(SkippedLine {
                line: line.id.clone(),
                reason: "the passage has no take of this line".to_owned(),
            });
        }
    }
    ledger::append_all_locked(dir, &events)?;
    Ok(choice)
}

/// The files under `takes/.passages/`.
#[derive(Debug, Clone, Default, PartialEq)]
struct PassageFiles {
    /// Ids with a published sidecar, `<id>.json`.
    metas: BTreeSet<String>,
    /// Ids with published audio, `<id>.wav`.
    wavs: BTreeSet<String>,
    /// `.partial` files, relative to the project, sorted.
    partials: Vec<PathBuf>,
}

fn scan(dir: &Path) -> Result<PassageFiles> {
    let passages = passages_dir(dir);
    let mut files = PassageFiles::default();
    let entries = match fs::read_dir(&passages) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(err) => return Err(io(&passages)(err)),
    };
    for entry in entries {
        let entry = entry.map_err(io(&passages))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".partial") {
            files
                .partials
                .push(Path::new(TAKES_DIR).join(PASSAGES_DIR).join(&name));
        } else if let Some((id, ext)) = name.rsplit_once('.')
            && is_valid_passage_id(id)
        {
            match ext {
                "json" => files.metas.insert(id.to_owned()),
                "wav" => files.wavs.insert(id.to_owned()),
                _ => false,
            };
        }
    }
    files.partials.sort();
    Ok(files)
}

/// The partial audio a sidecar names, beside it, when the name is a plain
/// `.wav.partial` file name.
fn named_partial_wav(sidecar_path: &Path, passage: &Passage) -> Option<PathBuf> {
    let name = passage.partial_wav.as_deref()?;
    let plain = Path::new(name).file_name() == Some(std::ffi::OsStr::new(name));
    (plain && name.ends_with(".wav.partial")).then(|| sidecar_path.with_file_name(name))
}

/// How many pieces of `passage` have a `recorded` event, and how many it
/// has; `None` for a passage that is not cut.
fn pieces_recorded(passage: &Passage, ledger: &Ledger) -> Option<(usize, usize)> {
    let count = passage.cuts.as_ref()?.len() + 1;
    let have: BTreeSet<u32> = ledger
        .pieces(&passage.id)
        .map(|(_, _, piece)| piece.index)
        .collect();
    Some((have.len().min(count), count))
}

/// The passage part of [`crate::repair`]: finishes and publishes passages
/// whose recording ended without a commit, completes publishes interrupted
/// between their two renames, and cuts every published passage that lacks
/// takes. Audio a recording window holds locked is left alone. A passage
/// that cannot be finished or cut is reported and does not keep the others
/// from being repaired.
pub(crate) fn repair(dir: &Path, report: &mut RepairReport) -> Result<()> {
    for rel in scan(dir)?.partials {
        let sidecar_path = dir.join(&rel);
        let is_sidecar = rel
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".json.partial"));
        if !is_sidecar {
            continue;
        }
        let Ok(mut passage) = read_sidecar(&sidecar_path) else {
            continue;
        };
        let named = format!(".{}.json.partial", passage.id);
        if !is_valid_passage_id(&passage.id) || rel.file_name() != Some(named.as_ref()) {
            continue;
        }
        let Some(wav) = named_partial_wav(&sidecar_path, &passage) else {
            continue;
        };
        let Ok(file) = fs::OpenOptions::new().read(true).write(true).open(&wav) else {
            continue;
        };
        // A live recording holds the lock; never touch its files.
        let Ok(file) = LockedFile::try_lock(file) else {
            continue;
        };
        if fix_wav_lengths(&file)
            .and_then(|()| file.sync_all())
            .is_err()
            || !matches!(wav_header(&wav), Ok((_, samples)) if samples > 0)
        {
            continue;
        }
        if passage.input.first_sample_film_s.is_none() {
            let wav = wav.strip_prefix(dir).unwrap_or(&wav).to_path_buf();
            report.passages_unplaced.push(wav);
            continue;
        }
        if !passage.recovered {
            passage.recovered = true;
            // The cuts are computed from the audio as it is now.
            passage.cuts = None;
            write_atomic(&sidecar_path, &to_json(&sidecar_path, &passage)?)?;
        }
        let partial = PartialPassage {
            id: passage.id.clone(),
            meta: sidecar_path,
            wav,
        };
        match publish_passage(dir, &partial) {
            Ok(_) => report.passages_published.push(passage.id),
            Err(err) => report.passages_failed.push(PassageFailure {
                passage: passage.id,
                error: err.to_string(),
            }),
        }
        drop(file);
    }

    let files = scan(dir)?;
    for id in files.metas.difference(&files.wavs) {
        let (meta, wav) = published_paths(dir, id)?;
        let partial = read_sidecar(&meta)
            .ok()
            .and_then(|passage| named_partial_wav(&meta, &passage))
            .filter(|partial| partial.is_file());
        if let Some(partial) = partial {
            // Kept locked across the rename, as for a take's audio.
            let held = hold(&partial);
            if !matches!(held, Hold::Recorder) {
                fs::rename(&partial, &wav).map_err(io(&wav))?;
            }
            drop(held);
        }
    }

    let files = scan(dir)?;
    let ledger = ledger::read(dir)?;
    for id in files.metas.intersection(&files.wavs) {
        let Ok(passage) = read_passage(dir, id) else {
            continue;
        };
        if pieces_recorded(&passage, &ledger).is_some_and(|(have, count)| have < count) {
            match split(dir, id) {
                Ok(split) => report.pieces.extend(split.created),
                Err(err) => report.passages_failed.push(PassageFailure {
                    passage: id.clone(),
                    error: err.to_string(),
                }),
            }
        }
    }
    report.partials.extend(files.partials);
    report.partials.sort();
    Ok(())
}

/// What `status` reports of the passages: leftover partial files (returned,
/// relative to the project), publishes a crash interrupted, and published
/// passages that are not cut into takes yet. An interrupted publish is one
/// notice, `interrupted-passage`, not also a `partial-passage` for its
/// audio.
pub(crate) fn notices(
    project: &Project,
    ledger: &Ledger,
    notices: &mut Vec<Problem>,
) -> Vec<PathBuf> {
    let dir = &project.dir;
    let files = match scan(dir) {
        Ok(files) => files,
        Err(err) => {
            notices.push(Problem::new(None, "scan-failed", err.to_string()));
            return Vec::new();
        }
    };
    let mut paired = BTreeSet::new();
    for rel in &files.partials {
        let path = dir.join(rel);
        let Ok(passage) = read_sidecar(&path) else {
            continue;
        };
        let wav = named_partial_wav(&path, &passage);
        let shown = wav.as_ref().map_or_else(
            || rel.clone(),
            |wav| wav.strip_prefix(dir).unwrap_or(wav).to_path_buf(),
        );
        paired.insert(shown.clone());
        paired.insert(rel.clone());
        let (code, message) = if passage.input.first_sample_film_s.is_none() {
            (
                "passage-unplaced",
                format!(
                    "{} is a passage whose recording was interrupted before it was placed on the film; it is kept, and cannot be cut into takes",
                    shown.display()
                ),
            )
        } else {
            (
                "partial-passage",
                format!(
                    "{} is left over from an interrupted passage recording; run `status --repair` to finish it",
                    shown.display()
                ),
            )
        };
        notices.push(Problem::new(None, code, message));
    }
    // The partial audio of a passage whose sidecar is published belongs to
    // an interrupted publish, which is reported once, below.
    let publishing = |rel: &PathBuf| {
        rel.file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix('.')?.strip_suffix(".wav.partial"))
            .is_some_and(|id| files.metas.contains(id))
    };
    for rel in files
        .partials
        .iter()
        .filter(|rel| !paired.contains(*rel) && !publishing(rel))
    {
        notices.push(Problem::new(
            None,
            "partial-passage",
            format!(
                "{} is left over from an interrupted passage recording; it is kept",
                rel.display()
            ),
        ));
    }
    for id in &files.metas {
        let Ok(passage) = read_passage(dir, id) else {
            continue;
        };
        let file = passage.file();
        if !files.wavs.contains(id) {
            notices.push(Problem::new(
                None,
                "interrupted-passage",
                format!(
                    "passage {id} was interrupted while being saved ({file} is missing); run `status --repair` to finish it"
                ),
            ));
        } else if let Some((have, count)) = pieces_recorded(&passage, ledger)
            && have < count
        {
            notices.push(Problem::new(
                None,
                "passage-not-split",
                format!(
                    "passage {id} ({file}) has takes for {have} of its {count} pieces; run `status --repair` to cut the rest"
                ),
            ));
        }
    }
    files.partials
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::ledger::{Recorded, read};
    use crate::project::FilmInfo;
    use crate::state::fold;
    use crate::takes::{PartialWav, fail_commits_after_renames, repair};
    use crate::testutil::{RATE, burst, noise, samples};

    /// Film time of the passage's first sample: 3 s of pre-roll before the
    /// first line.
    const FIRST: f64 = 7.0;

    /// Three sentences 0.5 and 0.4 s apart, then a line after a real pause.
    const SCRIPT: &str = r#"
[[line]]
id = "a"
start = 10.0
end = 13.0
text = "One."

[[line]]
id = "b"
start = 13.5
end = 16.0
text = "Two."

[[line]]
id = "c"
start = 16.4
end = 19.0
text = "Three."

[[line]]
id = "later"
start = 25.0
end = 27.0
text = "Later."
"#;

    fn snap(start: f64, end: f64) -> Snapshot {
        Snapshot {
            text: "Words.".to_owned(),
            start,
            end,
        }
    }

    /// `seconds` of room noise from film time [`FIRST`] on, with speech in
    /// the given ranges of film time.
    fn audio(seconds: f64, speech: &[(f64, f64)]) -> Vec<i32> {
        let mut audio = noise(seconds);
        for &(from, to) in speech {
            if from - FIRST < seconds {
                burst(&mut audio, from - FIRST, to - FIRST);
            }
        }
        audio
    }

    /// The sample of the passage at film time `film_s`.
    fn at(film_s: f64) -> u64 {
        samples(film_s - FIRST) as u64
    }

    fn cut_at_film(audio: &[i32], lines: &[Snapshot]) -> Vec<Cut> {
        cut(audio, RATE, FIRST, lines, 3.0).cuts
    }

    fn two_lines() -> Vec<Snapshot> {
        vec![snap(10.0, 13.0), snap(13.5, 16.0)]
    }

    #[test]
    fn a_passage_without_audio_has_no_cuts() {
        // Nothing was captured, and by the film time the capture would
        // have begun at, the second line had already started.
        for lines in [
            two_lines(),
            vec![snap(1.0, 2.0), snap(2.5, 4.0), snap(4.5, 6.0)],
        ] {
            let cuts = cut(&[], RATE, FIRST, &lines, 3.0);
            assert_eq!(cuts.cuts, [], "{lines:?}");
        }
    }

    #[test]
    fn a_pause_around_the_cue_is_cut_in_its_middle() {
        let audio = audio(10.0, &[(10.0, 12.8), (13.6, 15.8)]);
        let cuts = cut(&audio, RATE, FIRST, &two_lines(), 3.0);
        assert_eq!(
            cuts.cuts,
            vec![Cut {
                sample: at(13.2),
                cue_film_s: 13.5,
                pause: true
            }]
        );
        assert!((cuts.preroll_floor_dbfs + 65.0).abs() < 2.0, "{cuts:?}");
    }

    #[test]
    fn a_stop_closure_at_the_cue_loses_to_a_real_pause_beside_it() {
        // The narrator is early: a real pause of 300 ms ends 0.6 s before
        // the cue, and exactly at the cue a stop closure of 60 ms.
        let audio = audio(10.0, &[(10.0, 12.6), (12.9, 13.47), (13.53, 15.8)]);
        let cuts = cut_at_film(&audio, &two_lines());
        assert_eq!(cuts.len(), 1);
        assert_eq!((cuts[0].sample, cuts[0].pause), (at(12.75), true));
    }

    #[test]
    fn without_a_pause_the_cut_is_the_cue_and_says_so() {
        let audio = audio(10.0, &[(10.0, 15.8)]);
        let cuts = cut_at_film(&audio, &two_lines());
        assert_eq!(
            cuts,
            vec![Cut {
                sample: at(13.5),
                cue_film_s: 13.5,
                pause: false
            }]
        );
        // A stop closure alone is no pause either.
        let closure = self::audio(10.0, &[(10.0, 13.47), (13.53, 15.8)]);
        assert_eq!(cut_at_film(&closure, &two_lines()), cuts);
    }

    #[test]
    fn a_short_pause_is_taken_when_there_is_no_long_one() {
        // 100 ms between the sentences, 0.3 s after the cue.
        let audio = audio(10.0, &[(10.0, 13.8), (13.9, 15.8)]);
        let cuts = cut_at_film(&audio, &two_lines());
        assert_eq!((cuts[0].sample, cuts[0].pause), (at(13.85), true));
    }

    #[test]
    fn two_short_lines_are_cut_in_their_own_pauses() {
        let lines = vec![snap(10.0, 10.6), snap(10.7, 11.3), snap(11.4, 12.0)];
        let audio = audio(7.0, &[(10.0, 10.5), (10.72, 11.2), (11.45, 11.9)]);
        let cuts = cut_at_film(&audio, &lines);
        let at_samples: Vec<u64> = cuts.iter().map(|c| c.sample).collect();
        assert_eq!(at_samples, vec![at(10.61), at(11.325)]);
        assert!(cuts.iter().all(|c| c.pause));
    }

    #[test]
    fn an_early_and_a_late_narrator_are_cut_where_they_paused() {
        // Early: the second sentence starts 0.9 s before its cue.
        let early = audio(10.0, &[(10.0, 12.0), (12.6, 14.8)]);
        let cuts = cut_at_film(&early, &two_lines());
        assert_eq!((cuts[0].sample, cuts[0].pause), (at(12.3), true));
        // Late: the first sentence runs 0.4 s past the cue.
        let late = audio(10.0, &[(10.0, 13.9), (14.3, 15.9)]);
        let cuts = cut_at_film(&late, &two_lines());
        assert_eq!((cuts[0].sample, cuts[0].pause), (at(14.1), true));
    }

    #[test]
    fn a_stop_right_after_a_boundary_leaves_that_line_without_a_piece() {
        let lines = vec![snap(10.0, 13.0), snap(13.5, 16.0), snap(16.4, 19.0)];
        // Stopped 0.2 s into the second line, before a word of it.
        let stopped = audio(6.7, &[(10.0, 12.8)]);
        assert_eq!(cut_at_film(&stopped, &lines), vec![]);
        // Stopped mid-word: the line was begun, so it gets its piece (and
        // its words check will say what is missing).
        let cut_off = audio(7.2, &[(10.0, 12.8), (13.6, 14.2)]);
        let cuts = cut_at_film(&cut_off, &lines);
        assert_eq!(cuts.len(), 1, "the third line was never reached");
        assert_eq!(cuts[0].sample, at(13.2));
        // Read to the end, but the last line never spoken.
        let unspoken = self::audio(13.5, &[(10.0, 12.8), (13.6, 15.8)]);
        assert_eq!(cut_at_film(&unspoken, &lines).len(), 1);
    }

    #[test]
    fn the_pieces_cover_the_passage_exactly() {
        let lines = vec![
            snap(10.0, 13.0),
            snap(13.5, 16.0),
            // Slots that overlap, and one that is all but empty.
            snap(15.0, 19.0),
            snap(15.001, 15.002),
            snap(17.0, 18.0),
        ];
        let layouts: [&[(f64, f64)]; 4] = [
            &[(10.0, 20.0)],
            &[(10.0, 12.8), (13.6, 15.8), (16.5, 18.6)],
            &[(9.0, 9.5), (14.9, 15.1), (17.2, 17.9)],
            &[],
        ];
        for speech in layouts {
            for seconds in [0.01, 3.0, 6.5, 8.004, 9.0, 13.5] {
                let audio = audio(seconds, speech);
                let cuts = cut_at_film(&audio, &lines);
                let mut from = 0;
                for cut in &cuts {
                    assert!(cut.sample > from, "{speech:?} {seconds}: {cuts:?}");
                    from = cut.sample;
                }
                assert!(from < audio.len() as u64, "{speech:?} {seconds}: {cuts:?}");
                assert!(cuts.len() < lines.len());
                let passage = Passage {
                    samples: Some(audio.len() as u64),
                    cuts: Some(cuts.clone()),
                    ..sidecar(
                        &PartialPassage {
                            id: "p".to_owned(),
                            meta: PathBuf::new(),
                            wav: PathBuf::new(),
                        },
                        &input(&lines_of(&lines)),
                    )
                };
                let pieces = passage.pieces().unwrap();
                assert_eq!(pieces.len(), cuts.len() + 1);
                assert_eq!(pieces[0].start, 0);
                assert_eq!(pieces[pieces.len() - 1].end, audio.len() as u64);
                assert!(pieces.windows(2).all(|w| w[0].end == w[1].start));
                assert!(pieces.iter().all(|piece| piece.start < piece.end));
                assert_eq!(
                    passage.lines_without_piece().len(),
                    lines.len() - pieces.len()
                );
            }
        }
    }

    #[test]
    fn overlapping_slots_are_cut_at_their_own_cues() {
        // The first line's slot runs over the two after it.
        let lines = vec![snap(10.0, 19.0), snap(12.0, 13.0), snap(13.2, 15.0)];
        let audio = audio(9.0, &[(10.0, 11.8), (12.1, 12.9), (13.3, 14.9)]);
        let cuts = cut_at_film(&audio, &lines);
        assert_eq!(
            cuts,
            vec![
                Cut {
                    sample: at(11.95),
                    cue_film_s: 12.0,
                    pause: true
                },
                Cut {
                    sample: at(13.1),
                    cue_film_s: 13.2,
                    pause: true
                }
            ]
        );
        // Read without a pause: each cut is its own cue, and says so.
        let through = self::audio(9.0, &[(10.0, 14.9)]);
        let cuts: Vec<_> = cut_at_film(&through, &lines)
            .into_iter()
            .map(|cut| (cut.sample, cut.pause))
            .collect();
        assert_eq!(cuts, vec![(at(12.0), false), (at(13.2), false)]);
    }

    /// A deterministic stream of numbers.
    struct Random(u64);

    impl Random {
        /// The next number below `n`.
        fn below(&mut self, n: u32) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            u32::try_from((self.0 >> 33) % u64::from(n)).unwrap()
        }

        fn between(&mut self, lo: f64, hi: f64) -> f64 {
            lo + f64::from(self.below(1 << 24)) / f64::from(1 << 24) * (hi - lo)
        }
    }

    /// The search window of the cut before line `k + 1`, as the rule says:
    /// from the middle of line `k`'s slot to the middle of line `k + 1`'s,
    /// a slot counting up to the next line's start at the latest, within
    /// the `len` captured samples.
    fn window_of(lines: &[Snapshot], k: usize, len: usize) -> (usize, usize) {
        let middle = |k: usize| {
            let end = lines
                .get(k + 1)
                .map_or(lines[k].end, |next| lines[k].end.min(next.start));
            let film_s = f64::midpoint(lines[k].start, end.max(lines[k].start));
            samples((film_s - FIRST).max(0.0)).min(len)
        };
        (middle(k), middle(k + 1))
    }

    #[test]
    fn every_cut_lies_in_its_own_window_whatever_the_slots() {
        let frame = frame_len(RATE);
        for seed in 0..200 {
            let mut random = Random(seed);
            // Two to six lines, each starting after the one before, some
            // on the same instant; slots that end before the next line,
            // run far over the following ones, or are empty.
            let count = 2 + random.below(5);
            let mut lines = Vec::new();
            let mut start = FIRST + 3.0;
            for _ in 0..count {
                let end = match random.below(4) {
                    0 => start + random.between(0.0, 0.3),
                    1 => start + random.between(3.0, 12.0),
                    2 => start - random.between(0.0, 1.0),
                    _ => start + random.between(0.3, 3.0),
                };
                lines.push(snap(start, end));
                start += match random.below(6) {
                    0 => 0.0,
                    1 => random.between(0.0, 0.000_05),
                    _ => random.between(0.05, 3.0),
                };
            }
            // Speech in bursts with pauses of every length between them.
            let seconds = random.between(0.5, start - FIRST + 3.0);
            let mut speech = Vec::new();
            let mut from = FIRST + random.between(2.5, 3.5);
            while from < FIRST + seconds {
                let to = from + random.between(0.05, 2.5);
                speech.push((from, to));
                from = to + random.between(0.02, 0.6);
            }
            let audio = audio(seconds, &speech);
            let cuts = cut(&audio, RATE, FIRST, &lines, 3.0).cuts;

            let analysis = analyze(&audio, RATE, 3.0);
            let silent = |index: usize| {
                analysis
                    .frames
                    .levels_dbfs
                    .get(index)
                    .is_some_and(|&level| level <= analysis.frames.threshold_dbfs)
            };
            let context = format!("seed {seed}: {lines:?}, {seconds} s, {cuts:?}");
            assert!(cuts.len() < lines.len(), "{context}");
            let mut previous = 0;
            for (k, cut) in cuts.iter().enumerate() {
                let sample = usize::try_from(cut.sample).unwrap();
                assert!(previous < sample && sample < audio.len(), "{context}");
                assert!((cut.cue_film_s - lines[k + 1].start).abs() < 1e-12);
                let (lo, hi) = window_of(&lines, k, audio.len());
                assert!(lo <= sample && sample <= hi, "cut {k}, {context}");
                // The silent frames around the cut, inside its window.
                let lo = lo.max(previous + 1);
                let mut pause = sample / frame..sample / frame;
                while pause.start > 0 && (pause.start - 1) * frame >= lo && silent(pause.start - 1)
                {
                    pause.start -= 1;
                }
                while (pause.end + 1) * frame <= hi && silent(pause.end) {
                    pause.end += 1;
                }
                if cut.pause {
                    assert!(pause.len() >= SHORT_PAUSE_FRAMES, "cut {k}, {context}");
                } else {
                    let cue = samples((lines[k + 1].start - FIRST).max(0.0));
                    let nearest = cue.clamp(lo, hi);
                    assert_eq!(sample, nearest, "cut {k}, {context}");
                }
                previous = sample;
            }
        }
    }

    fn lines_of(snapshots: &[Snapshot]) -> Vec<PassageLine> {
        snapshots
            .iter()
            .enumerate()
            .map(|(i, snapshot)| PassageLine {
                id: format!("line-{i}"),
                snapshot: snapshot.clone(),
            })
            .collect()
    }

    fn input(lines: &[PassageLine]) -> PassageInput {
        PassageInput {
            sample_rate: RATE,
            device: "Test Mic".to_owned(),
            channel: 1,
            latency: Latency {
                input_s: 0.012,
                offset_ms: -29.0,
            },
            preroll_s: 3.0,
            lines: lines.to_vec(),
            first_sample_film_s: Some(FIRST),
            dropouts: None,
            film_shift_ms: None,
        }
    }

    fn project() -> (tempfile::TempDir, PathBuf) {
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

    /// The first passage of [`SCRIPT`]: lines a, b and c.
    fn abc(dir: &Path) -> PassageInput {
        let project = Project::open(dir).unwrap();
        let lines: Vec<PassageLine> = ["a", "b", "c"]
            .iter()
            .map(|id| PassageLine {
                id: (*id).to_owned(),
                snapshot: project.snapshot(id).unwrap(),
            })
            .collect();
        input(&lines)
    }

    /// All three sentences, each in its slot.
    fn spoken() -> Vec<i32> {
        audio(13.5, &[(10.0, 12.8), (13.6, 15.8), (16.5, 18.6)])
    }

    /// Records `audio` as the window does: begins the passage, streams the
    /// audio, and stops; returns the partial and the audio's lock.
    fn stream(dir: &Path, input: &PassageInput, audio: &[i32]) -> (PartialPassage, WavLock) {
        let partial = begin_passage(dir, input).unwrap();
        let mut wav = PartialWav::create(&partial.wav, RATE).unwrap();
        for chunk in audio.chunks(4800) {
            wav.write(chunk).unwrap();
        }
        let lock = wav.finish().unwrap();
        (partial, lock)
    }

    /// Records and publishes a passage; its takes are not cut yet.
    fn publish(dir: &Path, input: &PassageInput, audio: &[i32]) -> Passage {
        let (partial, lock) = stream(dir, input, audio);
        let passage = publish_passage(dir, &partial).unwrap();
        drop(lock);
        passage
    }

    fn recorded_events(dir: &Path) -> Vec<(String, u32, Recorded)> {
        read(dir)
            .unwrap()
            .events
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::Recorded(rec) => Some((event.line, event.take, rec)),
                _ => None,
            })
            .collect()
    }

    fn take(line: &str, take: u32) -> TakeRef {
        TakeRef {
            line: line.to_owned(),
            take,
        }
    }

    #[test]
    fn the_sidecar_is_never_a_takes_metadata_nor_the_other_way_round() {
        let (_tmp, dir) = project();
        let partial = begin_passage(&dir, &abc(&dir)).unwrap();
        assert_eq!(
            partial.meta.parent().unwrap(),
            dir.join("takes/.passages").as_path()
        );
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(&partial.meta).unwrap()).unwrap();
        assert_eq!(json["kind"], "passage");
        assert_eq!(json["id"], partial.id.as_str());
        assert_eq!(json["lines"][1]["id"], "b");
        assert_eq!(json["lines"][1]["snapshot"]["start"], 13.5);
        assert_eq!(json["latency"]["offset_ms"], -29.0);
        assert_eq!(json["first_sample_film_s"], FIRST);
        assert!(json.get("cuts").is_none() && json.get("dropouts").is_none());
        assert!(crate::takes::read_meta(&partial.meta).is_err());

        let take = crate::takes::begin_take(
            &dir,
            "a",
            &RecordedInput {
                sample_rate: RATE,
                device: "Test Mic".to_owned(),
                channel: 1,
                line_offset_s: -3.0,
                snapshot: snap(10.0, 13.0),
                dropouts: None,
                film_shift_ms: None,
                passage: None,
            },
        )
        .unwrap();
        assert!(read_sidecar(&take.meta).is_err());
    }

    #[test]
    fn a_passage_is_published_and_cut_into_one_take_per_line() {
        let (_tmp, dir) = project();
        let audio = spoken();
        let (partial, lock) = stream(&dir, &abc(&dir), &audio);
        // Placed by the film's timebase as read back, with a dropout
        // inside the second sentence.
        let placed = PassageInput {
            dropouts: Some(vec![Dropout {
                at_sample: at(14.0),
                missing: 480,
            }]),
            film_shift_ms: Some(12.0),
            ..abc(&dir)
        };
        update_passage_meta(&partial, &placed).unwrap();
        let passage = publish_passage(&dir, &partial).unwrap();
        drop(lock);

        let id = passage.id.clone();
        assert_eq!(passage.file(), format!("takes/.passages/{id}.wav"));
        assert!(!partial.meta.exists() && !partial.wav.exists());
        assert_eq!(
            read_take_wav(&dir.join(passage.file())).unwrap(),
            (audio.clone(), RATE),
            "the passage file is the capture, whole"
        );
        assert_eq!(passage.samples, Some(audio.len() as u64));
        let cuts = passage.cuts.clone().unwrap();
        assert_eq!(
            cuts.iter().map(|c| c.sample).collect::<Vec<_>>(),
            vec![at(13.2), at(16.15)]
        );
        assert_eq!(read_passage(&dir, &id).unwrap(), passage);
        assert!(recorded_events(&dir).is_empty(), "published, not yet cut");

        let report = split(&dir, &id).unwrap();
        let takes = vec![take("a", 1), take("b", 1), take("c", 1)];
        assert_eq!(report.takes, takes);
        assert_eq!(report.created, takes);
        assert!(report.pending.is_empty() && report.lines_without_piece.is_empty());

        let events = recorded_events(&dir);
        assert_eq!(events.len(), 3);
        let floor = passage.preroll_floor_dbfs.unwrap();
        let ranges = [(0, at(13.2)), (at(13.2), at(16.15)), (at(16.15), at(20.5))];
        let offsets = [-3.0, -0.3, -0.25];
        for (index, (line, number, rec)) in events.iter().enumerate() {
            let (from, to) = ranges[index];
            assert_eq!((line.as_str(), *number), (takes[index].line.as_str(), 1));
            assert_eq!(rec.file, format!("takes/{line}/001.wav"));
            assert_eq!(rec.samples, to - from);
            assert!(!rec.recovered);
            assert_eq!(
                rec.passage,
                Some(PassagePiece {
                    id: id.clone(),
                    index: u32::try_from(index).unwrap(),
                    count: 3,
                    from_sample: from,
                    to_sample: to,
                    preroll_floor_dbfs: floor,
                })
            );
            assert!(
                (rec.line_offset_s - offsets[index]).abs() < 1e-9,
                "{line}: {}",
                rec.line_offset_s
            );
            assert_eq!(rec.snapshot, placed.lines[index].snapshot);
            assert_eq!(rec.dropouts, Some(u32::from(index == 1)));
            assert_eq!(rec.film_shift_ms, Some(12.0));
            let (piece, _) = read_take_wav(&dir.join(&rec.file)).unwrap();
            let (from, to) = (usize::try_from(from).unwrap(), usize::try_from(to).unwrap());
            assert_eq!(piece, audio[from..to], "{line}: the piece's audio");
            assert!(dir.join(format!("takes/{line}/001.json")).is_file());
        }

        // Cutting again finds every piece and writes nothing.
        let again = split(&dir, &id).unwrap();
        assert_eq!(again.takes, takes);
        assert_eq!(again.created, vec![]);
        assert_eq!(recorded_events(&dir).len(), 3);
        let state = fold(&Project::open(&dir).unwrap(), &read(&dir).unwrap());
        assert_eq!(state.partials, Vec::<PathBuf>::new());
        assert_eq!(state.notices, vec![]);
    }

    #[test]
    fn a_passage_stopped_early_leaves_the_later_lines_without_takes() {
        let (_tmp, dir) = project();
        // Stopped mid-word in the second sentence.
        let audio = audio(7.2, &[(10.0, 12.8), (13.6, 14.2)]);
        let passage = publish(&dir, &abc(&dir), &audio);
        let report = split(&dir, &passage.id).unwrap();
        assert_eq!(report.takes, vec![take("a", 1), take("b", 1)]);
        assert_eq!(report.lines_without_piece, vec!["c".to_owned()]);
        let events = recorded_events(&dir);
        assert_eq!(events[1].2.passage.as_ref().unwrap().count, 2);
    }

    #[test]
    fn split_finishes_a_piece_whose_commit_stopped_before_its_event() {
        for renames in [1, 2] {
            let (_tmp, dir) = project();
            let passage = publish(&dir, &abc(&dir), &spoken());
            fail_commits_after_renames(Some(renames));
            assert!(split(&dir, &passage.id).is_err(), "{renames}");
            fail_commits_after_renames(None);
            assert!(recorded_events(&dir).is_empty(), "{renames}");
            assert!(dir.join("takes/a/001.json").is_file(), "{renames}");

            let report = split(&dir, &passage.id).unwrap();
            let takes = vec![take("a", 1), take("b", 1), take("c", 1)];
            assert_eq!(report.takes, takes, "{renames}");
            assert_eq!(report.created, takes[1..], "{renames}: a was finished");
            let events = recorded_events(&dir);
            assert_eq!(events.len(), 3, "{renames}: one take per piece");
            assert_eq!(events[0].2.samples, at(13.2), "{renames}");
            assert!(!events[0].2.recovered, "{renames}");
            let files: Vec<String> = fs::read_dir(dir.join("takes/a"))
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(files.len(), 2, "{renames}: {files:?}");
        }
    }

    #[test]
    fn split_recognizes_a_piece_adopted_by_repair() {
        let (_tmp, dir) = project();
        let passage = publish(&dir, &abc(&dir), &spoken());
        // A crash between renaming the piece's audio and its event.
        fail_commits_after_renames(Some(2));
        assert!(split(&dir, &passage.id).is_err());
        fail_commits_after_renames(None);

        let report = repair(&dir).unwrap();
        assert_eq!(report.adopted, vec![take("a", 1)]);
        assert_eq!(report.pieces, vec![take("b", 1), take("c", 1)]);
        let events = recorded_events(&dir);
        assert_eq!(events.len(), 3);
        assert!(events[0].2.recovered, "adopted from its NNN.json");
        assert_eq!(events[0].2.passage.as_ref().unwrap().index, 0);

        let again = split(&dir, &passage.id).unwrap();
        assert_eq!(again.created, vec![]);
        assert_eq!(recorded_events(&dir).len(), 3);
        assert_eq!(repair(&dir).unwrap().pieces, vec![]);
    }

    #[test]
    fn split_reuses_what_an_interrupted_split_left() {
        let (_tmp, dir) = project();
        let audio = spoken();
        let passage = publish(&dir, &abc(&dir), &audio);
        let id = &passage.id;
        fs::create_dir_all(dir.join("takes/a")).unwrap();
        fs::create_dir_all(dir.join("takes/b")).unwrap();
        // Piece 0: its audio half written, and no metadata yet.
        let stray = dir.join(format!("takes/a/.{id}-000.wav.partial"));
        write_take_wav(&stray, &audio[..1000], RATE).unwrap();
        // Piece 1: written whole with its metadata, not committed. A repair
        // adopts it; here the split finds it first.
        let whole = PartialTake {
            line: "b".to_owned(),
            meta: dir.join(format!("takes/b/.{id}-001.json.partial")),
            wav: dir.join(format!("takes/b/.{id}-001.wav.partial")),
        };
        let (from, to) = (at(13.2), at(16.15));
        let range = usize::try_from(from).unwrap()..usize::try_from(to).unwrap();
        write_take_wav(&whole.wav, &audio[range.clone()], RATE).unwrap();
        let piece = PassagePiece {
            id: id.clone(),
            index: 1,
            count: 3,
            from_sample: from,
            to_sample: to,
            preroll_floor_dbfs: passage.preroll_floor_dbfs.unwrap(),
        };
        let input = RecordedInput {
            sample_rate: RATE,
            device: "Test Mic".to_owned(),
            channel: 1,
            line_offset_s: -0.3,
            snapshot: passage.input.lines[1].snapshot.clone(),
            dropouts: None,
            film_shift_ms: None,
            passage: Some(piece),
        };
        write_partial_meta(&whole, &input).unwrap();

        let report = split(&dir, id).unwrap();
        assert_eq!(report.created.len(), 3);
        assert!(!stray.exists() && !whole.wav.exists() && !whole.meta.exists());
        let (first, _) = read_take_wav(&dir.join("takes/a/001.wav")).unwrap();
        assert_eq!(first.len() as u64, at(13.2), "written anew, whole");
        let (second, _) = read_take_wav(&dir.join("takes/b/001.wav")).unwrap();
        assert_eq!(second, audio[range]);
        assert_eq!(recorded_events(&dir).len(), 3);
        assert_eq!(repair(&dir).unwrap().adopted, vec![]);
    }

    #[test]
    fn a_piece_another_process_is_writing_is_left_to_it() {
        let (_tmp, dir) = project();
        let passage = publish(&dir, &abc(&dir), &spoken());
        fs::create_dir_all(dir.join("takes/b")).unwrap();
        let claim = dir.join(format!("takes/b/.{}-001.wav.partial", passage.id));
        let held = WavLock::acquire(&claim).unwrap();
        let report = split(&dir, &passage.id).unwrap();
        assert_eq!(report.takes, vec![take("a", 1), take("c", 1)]);
        assert_eq!(report.pending, vec![1]);
        // A repair leaves the claimed piece alone, too.
        assert_eq!(repair(&dir).unwrap().pieces, vec![]);
        drop(held);
        let report = split(&dir, &passage.id).unwrap();
        assert_eq!(report.created, vec![take("b", 1)]);
        assert_eq!(recorded_events(&dir).len(), 3);
    }

    #[test]
    fn a_passage_that_could_not_be_written_is_published_from_memory() {
        let (_tmp, dir) = project();
        let audio = spoken();
        // Never begun on disk: everything comes from memory.
        let mut unsaved = UnsavedPassage {
            input: abc(&dir),
            samples: audio.clone(),
            partial: None,
            lock: None,
            complete: false,
        };
        // The disk fails once the sidecar is renamed: the audio stays
        // locked, so a repair leaves the half published passage alone.
        fail_commits_after_renames(Some(1));
        assert!(save_passage(&dir, &mut unsaved).is_err());
        fail_commits_after_renames(None);
        assert!(unsaved.lock.is_some());
        let report = repair(&dir).unwrap();
        assert_eq!(report.passages_published, Vec::<String>::new());
        assert_eq!(report.pieces, vec![]);
        // It fails again after the audio is renamed too: the passage is
        // published all the same, and saving once more only confirms it.
        fail_commits_after_renames(Some(2));
        assert!(save_passage(&dir, &mut unsaved).is_err());
        fail_commits_after_renames(None);
        let passage = save_passage(&dir, &mut unsaved).unwrap();
        assert_eq!(
            read_take_wav(&dir.join(passage.file())).unwrap(),
            (audio, RATE)
        );
        assert_eq!(passage.cuts.as_ref().map(Vec::len), Some(2));
        assert_eq!(split(&dir, &passage.id).unwrap().created.len(), 3);
        assert_eq!(split(&dir, &passage.id).unwrap().created.len(), 0);

        // Begun on disk, but the partial audio is short (a write failed):
        // rewritten from memory under the lock kept from the recording.
        let (partial, lock) = stream(&dir, &abc(&dir), &spoken()[..48_000]);
        let mut unsaved = UnsavedPassage {
            input: abc(&dir),
            samples: spoken(),
            partial: Some(partial),
            lock: Some(lock),
            complete: false,
        };
        let second = save_passage(&dir, &mut unsaved).unwrap();
        assert_eq!(second.samples, Some(spoken().len() as u64));
        assert_ne!(second.id, passage.id);
    }

    #[test]
    fn an_interrupted_save_from_memory_keeps_the_audio_on_disk() {
        let (_tmp, dir) = project();
        let audio = spoken();
        // The recording streamed the first 9 s, then its disk failed; the
        // whole passage is in memory.
        let streamed = samples(9.0);
        let partial = begin_passage(&dir, &abc(&dir)).unwrap();
        let mut wav = PartialWav::create(&partial.wav, RATE).unwrap();
        wav.write(&audio[..streamed]).unwrap();
        wav.flush().unwrap();
        let mut unsaved = UnsavedPassage {
            input: abc(&dir),
            samples: audio.clone(),
            partial: Some(partial.clone()),
            lock: Some(wav.into_lock()),
            complete: false,
        };
        assert!(unsaved.audio_only_in_memory());
        let before = fs::read(&partial.wav).unwrap();
        // Saving from memory fails after it began to write.
        crate::takes::fail_wav_writes_after(Some(100_000));
        assert!(save_passage(&dir, &mut unsaved).is_err());
        crate::takes::fail_wav_writes_after(None);
        assert!(unsaved.audio_only_in_memory());
        assert!(fs::read(&partial.wav).unwrap() == before, "not shortened");
        assert!(WavLock::acquire(&partial.wav).is_err(), "still held");
        assert_eq!(
            repair(&dir).unwrap().passages_published,
            Vec::<String>::new()
        );

        // The window dies: a repair publishes what was streamed.
        drop(unsaved);
        let report = repair(&dir).unwrap();
        assert_eq!(report.passages_published, vec![partial.id.clone()]);
        let recovered = read_passage(&dir, &partial.id).unwrap();
        assert_eq!(recovered.samples, Some(streamed as u64));
        let (on_disk, _) = read_take_wav(&dir.join(recovered.file())).unwrap();
        assert!(on_disk == audio[..streamed], "the 9 s that were streamed");
    }

    #[test]
    fn a_save_from_memory_after_a_failed_one_publishes_the_whole_passage() {
        let (_tmp, dir) = project();
        let audio = spoken();
        let (partial, lock) = stream(&dir, &abc(&dir), &audio[..samples(9.0)]);
        let mut unsaved = UnsavedPassage {
            input: abc(&dir),
            samples: audio.clone(),
            partial: Some(partial.clone()),
            lock: Some(lock),
            complete: false,
        };
        crate::takes::fail_wav_writes_after(Some(0));
        assert!(save_passage(&dir, &mut unsaved).is_err());
        crate::takes::fail_wav_writes_after(None);
        // The disk fails once more after the audio is whole on disk:
        // nothing is only in memory any more, and the audio is still held.
        fail_commits_after_renames(Some(1));
        assert!(save_passage(&dir, &mut unsaved).is_err());
        fail_commits_after_renames(None);
        assert!(!unsaved.audio_only_in_memory());
        assert!(WavLock::acquire(&partial.wav).is_err(), "held");
        assert_eq!(
            repair(&dir).unwrap().passages_published,
            Vec::<String>::new()
        );
        let passage = save_passage(&dir, &mut unsaved).unwrap();
        assert_eq!(passage.id, partial.id);
        assert_eq!(
            read_take_wav(&dir.join(passage.file())).unwrap(),
            (audio, RATE)
        );
    }

    #[test]
    fn a_passage_without_a_film_time_is_not_published() {
        let (_tmp, dir) = project();
        let unplaced = PassageInput {
            first_sample_film_s: None,
            ..abc(&dir)
        };
        let (partial, lock) = stream(&dir, &unplaced, &spoken());
        let err = publish_passage(&dir, &partial).unwrap_err();
        assert_eq!(err.code(), "passage-unplaced");
        assert!(partial.meta.exists() && partial.wav.exists(), "kept");
        drop(lock);
    }

    /// Streams a passage as the window does, then "crashes": drops the
    /// writer without finishing, after unflushed writes.
    fn crash_mid_passage(dir: &Path, input: &PassageInput, audio: &[i32]) -> PartialPassage {
        let partial = begin_passage(dir, input).unwrap();
        let mut wav = PartialWav::create(&partial.wav, RATE).unwrap();
        let (flushed, rest) = audio.split_at(audio.len() / 2);
        wav.write(flushed).unwrap();
        wav.flush().unwrap();
        wav.write(rest).unwrap();
        drop(wav);
        partial
    }

    #[test]
    fn repair_finishes_publishes_and_cuts_a_crashed_passage() {
        let (_tmp, dir) = project();
        let audio = spoken();
        let partial = crash_mid_passage(&dir, &abc(&dir), &audio);

        let before = fold(&Project::open(&dir).unwrap(), &read(&dir).unwrap());
        assert_eq!(before.partials.len(), 2, "{:?}", before.partials);
        let codes: Vec<&str> = before.notices.iter().map(|n| n.code).collect();
        assert_eq!(codes, vec!["partial-passage"]);

        let report = repair(&dir).unwrap();
        assert_eq!(report.passages_published, vec![partial.id.clone()]);
        assert_eq!(
            report.pieces,
            vec![take("a", 1), take("b", 1), take("c", 1)]
        );
        assert_eq!(report.adopted, vec![]);
        assert_eq!(report.partials, Vec::<PathBuf>::new());
        assert!(!partial.meta.exists() && !partial.wav.exists());
        let passage = read_passage(&dir, &partial.id).unwrap();
        assert!(passage.recovered);
        assert_eq!(
            passage.samples,
            Some(audio.len() as u64),
            "the lengths are fixed from the size on disk"
        );
        assert_eq!(read_take_wav(&dir.join(passage.file())).unwrap().0, audio);
        let events = recorded_events(&dir);
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|(_, _, rec)| rec.recovered));
        assert!(events.iter().all(|(_, _, rec)| rec.dropouts.is_none()));

        let again = repair(&dir).unwrap();
        assert_eq!(again.passages_published, Vec::<String>::new());
        assert_eq!(again.pieces, vec![]);
        assert_eq!(recorded_events(&dir).len(), 3);
        let after = fold(&Project::open(&dir).unwrap(), &read(&dir).unwrap());
        assert_eq!(after.notices, vec![]);
    }

    #[test]
    fn repair_keeps_and_reports_a_crashed_passage_without_a_film_time() {
        let (_tmp, dir) = project();
        let unplaced = PassageInput {
            first_sample_film_s: None,
            ..abc(&dir)
        };
        let partial = crash_mid_passage(&dir, &unplaced, &spoken());
        let wav = partial.wav.strip_prefix(&dir).unwrap().to_path_buf();
        let meta = partial.meta.strip_prefix(&dir).unwrap().to_path_buf();

        let report = repair(&dir).unwrap();
        assert_eq!(report.passages_unplaced, vec![wav.clone()]);
        assert_eq!(report.passages_published, Vec::<String>::new());
        assert_eq!(report.partials, vec![meta.clone(), wav.clone()]);
        assert!(partial.meta.exists() && partial.wav.exists(), "kept");
        assert_eq!(recorded_events(&dir), vec![]);
        // Its audio is a valid WAV of everything on disk all the same.
        assert_eq!(read_take_wav(&partial.wav).unwrap().0, spoken());

        let state = fold(&Project::open(&dir).unwrap(), &read(&dir).unwrap());
        assert_eq!(state.partials, vec![meta, wav.clone()]);
        assert_eq!(state.notices.len(), 1, "{:?}", state.notices);
        assert_eq!(state.notices[0].code, "passage-unplaced");
        assert!(
            state.notices[0].message.contains(&*wav.to_string_lossy()),
            "{}",
            state.notices[0].message
        );
    }

    #[test]
    fn repair_never_touches_a_passage_that_is_being_recorded() {
        let (_tmp, dir) = project();
        let partial = begin_passage(&dir, &abc(&dir)).unwrap();
        let mut wav = PartialWav::create(&partial.wav, RATE).unwrap();
        wav.write(&spoken()).unwrap();
        wav.flush().unwrap();

        let report = repair(&dir).unwrap();
        assert_eq!(report.passages_published, Vec::<String>::new());
        assert_eq!(report.partials.len(), 2);
        assert!(partial.meta.exists() && partial.wav.exists());

        // Once the recording ends without a commit, it is finished.
        drop(wav);
        assert_eq!(repair(&dir).unwrap().passages_published, vec![partial.id]);
    }

    #[test]
    fn repair_completes_a_publish_interrupted_between_its_renames() {
        let (_tmp, dir) = project();
        let (partial, lock) = stream(&dir, &abc(&dir), &spoken());
        fail_commits_after_renames(Some(1));
        assert!(publish_passage(&dir, &partial).is_err());
        fail_commits_after_renames(None);
        assert!(!partial.meta.exists() && partial.wav.exists());

        // The window still holds the audio: it finishes the publish itself.
        let state = fold(&Project::open(&dir).unwrap(), &read(&dir).unwrap());
        let codes: Vec<&str> = state.notices.iter().map(|n| n.code).collect();
        assert_eq!(codes, vec!["interrupted-passage"], "said once");
        assert_eq!(state.partials.len(), 1, "the audio is still listed");
        assert_eq!(repair(&dir).unwrap().pieces, vec![]);
        assert!(partial.wav.exists());

        // After a crash nothing holds it, and the repair does.
        drop(lock);
        let report = repair(&dir).unwrap();
        assert_eq!(report.pieces.len(), 3);
        assert_eq!(report.partials, Vec::<PathBuf>::new());
        assert!(dir.join(passage_file(&partial.id)).is_file());
    }

    #[test]
    fn repair_cuts_a_published_passage_that_lacks_takes() {
        let (_tmp, dir) = project();
        let passage = publish(&dir, &abc(&dir), &spoken());
        let state = fold(&Project::open(&dir).unwrap(), &read(&dir).unwrap());
        let codes: Vec<&str> = state.notices.iter().map(|n| n.code).collect();
        assert_eq!(codes, vec!["passage-not-split"]);
        assert!(state.notices[0].message.contains(&passage.id));

        let report = repair(&dir).unwrap();
        assert_eq!(
            report.pieces,
            vec![take("a", 1), take("b", 1), take("c", 1)]
        );
        assert!(
            recorded_events(&dir)
                .iter()
                .all(|(_, _, rec)| !rec.recovered)
        );
    }

    #[test]
    fn cuts_changed_by_hand_are_refused_not_followed() {
        let (_tmp, dir) = project();
        let passage = publish(&dir, &abc(&dir), &spoken());
        let path = dir.join(format!("takes/.passages/{}.json", passage.id));
        let total = passage.samples.unwrap();
        // Out of order, past the end, and more cuts than lines to cut.
        let wrong: [Vec<u64>; 4] = [
            vec![at(16.15), at(13.2)],
            vec![at(13.2), total],
            vec![0, at(13.2)],
            vec![10, 20, 30],
        ];
        for samples in wrong {
            let mut changed = passage.clone();
            changed.cuts = Some(
                samples
                    .iter()
                    .map(|&sample| Cut {
                        sample,
                        cue_film_s: 13.5,
                        pause: true,
                    })
                    .collect(),
            );
            assert_eq!(changed.pieces(), None, "{samples:?}");
            fs::write(&path, to_json(&path, &changed).unwrap()).unwrap();
            let err = split(&dir, &passage.id).unwrap_err();
            assert_eq!(err.code(), "parse", "{samples:?}: {err}");
        }
        assert_eq!(recorded_events(&dir), vec![]);
    }

    #[test]
    fn a_passage_that_cannot_be_cut_does_not_stop_the_repair() {
        let (_tmp, dir) = project();
        let broken = publish(&dir, &abc(&dir), &spoken());
        // Its audio no longer matches its sidecar.
        write_take_wav(&dir.join(broken.file()), &spoken()[..48_000], RATE).unwrap();
        let sound = publish(&dir, &abc(&dir), &spoken());

        let report = repair(&dir).unwrap();
        assert_eq!(report.passages_failed.len(), 1, "{report:?}");
        assert_eq!(report.passages_failed[0].passage, broken.id);
        assert!(
            report.passages_failed[0]
                .error
                .contains("does not match its sidecar"),
            "{report:?}"
        );
        assert_eq!(report.pieces.len(), 3, "the other passage is cut");
        let events = recorded_events(&dir);
        assert!(
            events
                .iter()
                .all(|(_, _, rec)| rec.passage.as_ref().unwrap().id == sound.id)
        );
        // Nothing of the broken one is left half written.
        let state = fold(&Project::open(&dir).unwrap(), &read(&dir).unwrap());
        assert_eq!(state.partials, Vec::<PathBuf>::new());
        let codes: Vec<&str> = state.notices.iter().map(|n| n.code).collect();
        assert_eq!(codes, vec!["passage-not-split"]);
    }

    #[test]
    fn repair_and_the_scan_leave_dot_directories_alone() {
        let (_tmp, dir) = project();
        // What 0.1.3 tripped over: files that look like a take's, in a
        // directory that is no line's.
        for hidden in [".room", ".passages", ".other"] {
            let hidden = dir.join("takes").join(hidden);
            fs::create_dir_all(&hidden).unwrap();
            let stray = PartialTake {
                line: "a".to_owned(),
                meta: hidden.join(".stray.json.partial"),
                wav: hidden.join(".stray.wav.partial"),
            };
            let input = RecordedInput {
                sample_rate: RATE,
                device: "Test Mic".to_owned(),
                channel: 1,
                line_offset_s: -3.0,
                snapshot: snap(10.0, 13.0),
                dropouts: None,
                film_shift_ms: None,
                passage: None,
            };
            write_partial_meta(&stray, &input).unwrap();
            write_take_wav(&stray.wav, &[1000; 480], RATE).unwrap();
            write_take_wav(&hidden.join("001.wav"), &[1000; 480], RATE).unwrap();
        }
        let report = repair(&dir).unwrap();
        assert_eq!(report.adopted, vec![]);
        assert_eq!(report.unadopted, Vec::<PathBuf>::new());
        assert_eq!(recorded_events(&dir), vec![]);
        // Only the passages' own leftovers are listed, and kept.
        let listed = vec![
            PathBuf::from("takes/.passages/.stray.json.partial"),
            PathBuf::from("takes/.passages/.stray.wav.partial"),
        ];
        assert_eq!(report.partials, listed);
        assert!(dir.join("takes/.room/.stray.json.partial").is_file());
        let state = fold(&Project::open(&dir).unwrap(), &read(&dir).unwrap());
        assert_eq!(state.partials, listed);
        assert_eq!(state.unadopted, Vec::<PathBuf>::new());
    }

    fn chosen_events(dir: &Path) -> Vec<(String, u32, String)> {
        read(dir)
            .unwrap()
            .events
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::Chosen(chosen) => Some((event.line, event.take, chosen.by)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn choosing_a_passage_chooses_every_piece_or_none() {
        let (_tmp, dir) = project();
        // An earlier passage of all three lines, and one stopped in b.
        let whole = publish(&dir, &abc(&dir), &spoken());
        split(&dir, &whole.id).unwrap();
        let short = publish(&dir, &abc(&dir), &audio(7.2, &[(10.0, 12.8), (13.6, 14.2)]));
        split(&dir, &short.id).unwrap();

        let choice = choose_passage(&dir, &whole.id, "narrator").unwrap();
        assert_eq!(
            choice.chosen,
            vec![take("a", 1), take("b", 1), take("c", 1)]
        );
        assert_eq!(choice.skipped, vec![]);
        assert_eq!(chosen_events(&dir).len(), 3);

        // The shorter passage has no piece of c: c keeps its choice.
        let choice = choose_passage(&dir, &short.id, "claude").unwrap();
        assert_eq!(choice.chosen, vec![take("a", 2), take("b", 2)]);
        assert_eq!(
            choice.skipped,
            vec![SkippedLine {
                line: "c".to_owned(),
                reason: "the passage has no take of this line".to_owned()
            }]
        );
        let state = fold(&Project::open(&dir).unwrap(), &read(&dir).unwrap());
        let chosen: Vec<_> = state.lines.iter().map(|l| l.chosen.clone()).collect();
        assert_eq!(
            chosen,
            vec![
                Some((2, "claude".to_owned())),
                Some((2, "claude".to_owned())),
                Some((1, "narrator".to_owned())),
                None
            ]
        );

        // One missing take file refuses the whole passage.
        fs::remove_file(dir.join("takes/b/001.wav")).unwrap();
        let before = chosen_events(&dir);
        let err = choose_passage(&dir, &whole.id, "narrator").unwrap_err();
        assert!(
            matches!(&err, Error::MissingFile { line, take: 1, .. } if line == "b"),
            "{err:?}"
        );
        assert_eq!(chosen_events(&dir), before, "nothing appended");
    }

    #[test]
    fn a_torn_choice_of_a_passage_chooses_no_line() {
        let (_tmp, dir) = project();
        let passage = publish(&dir, &abc(&dir), &spoken());
        split(&dir, &passage.id).unwrap();
        let before = fs::read(dir.join("takes.jsonl")).unwrap();
        // The write of the three choices stops in the first event, right
        // after it, and in the last one.
        let one = serde_json::to_string(&Event::new(
            "a",
            1,
            EventKind::Chosen(Chosen {
                by: "narrator".to_owned(),
            }),
        ))
        .unwrap()
        .len()
            + 1;
        for torn in [0, one / 2, one, 2 * one, 3 * one - 1] {
            ledger::tear_batches_after(Some(torn));
            let failed = choose_passage(&dir, &passage.id, "narrator");
            ledger::tear_batches_after(None);
            assert!(failed.is_err(), "{torn}");
            assert!(
                fs::read(dir.join("takes.jsonl")).unwrap() == before,
                "{torn}"
            );
            let state = fold(&Project::open(&dir).unwrap(), &read(&dir).unwrap());
            let chosen: Vec<_> = state
                .lines
                .iter()
                .filter_map(|l| l.chosen.clone())
                .collect();
            assert_eq!(chosen, vec![], "{torn}: a part of the passage was chosen");
        }
        let choice = choose_passage(&dir, &passage.id, "narrator").unwrap();
        assert_eq!(choice.chosen.len(), 3);
        let after = fs::read(dir.join("takes.jsonl")).unwrap();
        assert!(after.starts_with(&before), "every earlier byte is kept");
        assert_eq!(chosen_events(&dir).len(), 3);
    }

    #[test]
    fn choosing_a_passage_needs_its_takes() {
        let (_tmp, dir) = project();
        let err = choose_passage(&dir, "0000", "narrator").unwrap_err();
        assert_eq!(err.code(), "unknown-passage");
        let err = choose_passage(&dir, "../escape", "narrator").unwrap_err();
        assert_eq!(err.code(), "unknown-passage");

        // Cut only partly: a commit failed after the first piece.
        let passage = publish(&dir, &abc(&dir), &spoken());
        let err = choose_passage(&dir, &passage.id, "narrator").unwrap_err();
        assert_eq!(err.code(), "unknown-passage", "no take of it yet");
        split(&dir, &passage.id).unwrap();
        let ledger = fs::read_to_string(dir.join("takes.jsonl")).unwrap();
        let first_line: String = ledger.lines().take(1).collect::<String>() + "\n";
        fs::write(dir.join("takes.jsonl"), first_line).unwrap();
        let err = choose_passage(&dir, &passage.id, "narrator").unwrap_err();
        assert!(
            matches!(
                err,
                Error::PassageNotSplit {
                    have: 1,
                    count: 3,
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(chosen_events(&dir), vec![]);

        // Cutting again finishes the two pieces from their files. A piece
        // whose line left the script is skipped, the rest are chosen.
        assert_eq!(split(&dir, &passage.id).unwrap().takes.len(), 3);
        assert_eq!(recorded_events(&dir).len(), 3);
        let without_b = SCRIPT.replace("id = \"b\"", "id = \"b2\"");
        fs::write(dir.join("script.toml"), without_b).unwrap();
        let choice = choose_passage(&dir, &passage.id, "narrator").unwrap();
        assert_eq!(choice.chosen, vec![take("a", 1), take("c", 1)]);
        assert_eq!(
            choice.skipped,
            vec![SkippedLine {
                line: "b".to_owned(),
                reason: "the line is no longer in the script".to_owned()
            }]
        );
        assert_eq!(chosen_events(&dir).len(), 2);
    }
}
