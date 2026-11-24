//! The recording session: takes of a line and of a passage, play in
//! context, the input meter and the timing that places captured samples on
//! the film. Free of GPUI; the window calls it from key actions and once per
//! rendered frame.
//!
//! A passage take (several lines in one go) runs like a line take: one
//! capture, one film playback, the same writer and lock. It streams into the
//! passage's partial files, and when it stops the passage is published in
//! the frame (the commit point) and cut into one take per line on a worker
//! thread, which holds the audio's lock until the takes are there.

use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use takewise_core::passage::{self, Latency};
use takewise_core::rescue::Rescue;
use takewise_core::takes::FULL_SCALE;
use takewise_core::timing::film_shift_s;
use takewise_core::{
    Dropout, PartialPassage, PartialTake, PassageInput, PassageLine, PassagePiece, Project,
    ProjectConfig, RECORD_RATE, RecordPlan, RecordedInput, Route, Snapshot, TakeRef, TakeState,
    Timebase, UnsavedPassage, WavLock, begin_passage, begin_take, commit_take,
    finish_interrupted_commit, line_offset_s, passage_file, plan_take, read_passage,
    rewrite_take_wav, sample_film_time, save_passage, update_passage_meta, update_take_meta,
};
use takewise_media::CaptureBuffer;

use super::clock::{FilmClock, START_LEAD_S};
use super::media::Media;
use super::writer::{Progress, TakeWriter, Written};

/// How long a take waits for its first buffer before it gives up.
const FIRST_BUFFER_TIMEOUT_S: f64 = 2.0;
/// How long the film plays muted after opening, so a cold output device does
/// not shift the first take.
pub const WARMUP_S: f64 = 0.3;

/// The count-in's first digit.
const COUNT_IN: u32 = 3;

/// How long a take may take to load before play in context gives up.
const TAKE_LOAD_TIMEOUT_S: f64 = 3.0;
/// The meter forgets a level this long after the last buffer.
const LEVEL_HOLD_S: f64 = 0.5;
/// How long to wait before reopening the meter after it failed.
const METER_RETRY_S: f64 = 2.0;
/// How often saving a take that could not be saved is retried.
const SAVE_RETRY_S: f64 = 2.0;
/// A film halted this close to its end has reached it.
const END_TOLERANCE_S: f64 = 0.25;
/// A film not playing this long after its scheduled start stops the take.
const FILM_START_TIMEOUT_S: f64 = 1.0;
/// A film shift larger than this after the take started is recorded.
const FILM_SHIFT_TOLERANCE_MS: f64 = 5.0;
/// How long, in film time, the prompter keeps the line just read in view
/// after a passage take moved on to the next line, so a reader running a
/// little late can finish it.
pub const PASSED_S: f64 = 1.5;
/// A passage's sidecar is rewritten when its first sample's film time moved
/// by more than this (the film started late, so the take follows it).
const SIDECAR_TOLERANCE_S: f64 = 0.0005;

/// The latency offset in ms that places takes on `route`: the calibrated
/// `latency_offset_ms` when `[calibration]` names exactly this input, this
/// output (the one in use, after any fallback) and this buffer size; else 0,
/// since an offset measured on other devices would misplace the takes.
/// The latency offset to apply on `route` in ms: the calibrated one only
/// while `[calibration]` fits these devices and buffer (see
/// [`takewise_core::effective_latency_offset`]).
pub fn effective_latency_offset(config: &ProjectConfig, route: &Route) -> i32 {
    takewise_core::effective_latency_offset(config, route).0
}

/// Whether a calibration was measured on exactly `route`. A project without
/// `[calibration]` never matches, so the setup check offers to calibrate.
pub fn calibration_matches(config: &ProjectConfig, route: &Route) -> bool {
    config.calibration.is_some() && !takewise_core::effective_latency_offset(config, route).1
}

/// Whether a playback or a take's film may start, as the window says from
/// its output before each tick.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum StartHold {
    /// It may.
    #[default]
    Go,
    /// Not yet: the output could not be checked just now.
    Wait,
    /// No: the output can no longer be used, for this reason.
    Stop(String),
}

/// What a tick or a stop produced, for the window to show.
#[derive(Debug, Default)]
pub struct Update {
    pub committed: Option<TakeRef>,
    pub message: Option<String>,
    /// The committed take's highest peak in dBFS, for the "clipped" mark
    /// until its check says.
    pub peak_dbfs: Option<f64>,
    /// The takes a passage was cut into, once they are all there.
    pub passage: Option<PassageTakes>,
    /// What a take that just stopped says of its output (it went away, or
    /// the system output changed, while its film played), also in
    /// `message`: the window keeps it in sight until the next take's
    /// result.
    pub route_note: Option<String>,
}

/// The takes of a passage recorded in one go.
#[derive(Debug)]
pub struct PassageTakes {
    /// The take of each line that got one, in script order, with its
    /// highest peak in dBFS.
    pub takes: Vec<(TakeRef, Option<f64>)>,
    /// The passage's lines without a take: the recording stopped before
    /// they were read.
    pub unread: Vec<String>,
}

pub struct Recorder {
    phase: Phase,
    warmup: Warmup,
    /// Where the film rests while nothing plays.
    park_s: f64,
    /// Peak of the last captured buffer in dBFS, and when it arrived.
    level: Option<(f64, f64)>,
    /// The highest peak of the meter's buffers in the last tick.
    tick_peak: Option<f64>,
    /// The idle capture that drives the level meter (never played back),
    /// and the device and channel it captures.
    meter: Option<Receiver<CaptureBuffer>>,
    meter_input: Option<(String, u16)>,
    meter_retry_at: f64,
    /// A short clip recorded from the meter's capture, without a take (the
    /// setup check's voice and room steps), while one is being recorded.
    clip: Option<Vec<i32>>,
    /// The meter's buffers with their host times, kept for the latency
    /// calibration while it runs, and whether the meter's capture stopped
    /// or moved since the tap began (the buffers then do not line up).
    tap: Option<Vec<CaptureBuffer>>,
    tap_broken: bool,
    /// Takes whose audio could not be saved yet; kept in memory and retried.
    unsaved: Vec<Unsaved>,
    unsaved_retry_at: f64,
    /// Why saving a take or passage failed last, while any waits.
    save_error: Option<String>,
    /// Which line the last passage take rests on (from 0, and its id),
    /// until the next take starts: the line it was reading when the
    /// narrator stopped it, or its first line when it ran to its end, so
    /// Shift+Space picks it up there or reads it again whole.
    read_last: Option<(usize, String)>,
    /// Passages that could not be published yet; kept in memory and retried.
    unsaved_passages: Vec<Stopped>,
    /// Published passages whose takes are being cut, or wait to be cut
    /// again.
    uncut: Vec<Uncut>,
    /// Whether a playback or a take's film may start at this tick, as the
    /// window says before each tick from its output.
    start_hold: StartHold,
}

/// A stopped passage that is not published yet.
struct Stopped {
    unsaved: UnsavedPassage,
    /// What to say with its takes: why it stopped, dropouts, a film that
    /// stalled.
    notes: Option<String>,
    /// Its rescue copy, once one is written (see [`Recorder::rescue`]).
    rescued: Option<PathBuf>,
}

impl Stopped {
    /// The passage once it is published as `id`: its audio stays locked,
    /// and in memory for the peaks of its takes, until they are cut.
    fn published(self, id: String) -> Uncut {
        Uncut {
            id,
            samples: Arc::new(self.unsaved.samples),
            _lock: self.unsaved.lock,
            notes: self.notes,
            worker: None,
            failed: false,
        }
    }
}

/// A published passage whose takes are not all cut yet.
struct Uncut {
    id: String,
    /// The passage's audio, for the peaks of its takes.
    samples: Arc<Vec<i32>>,
    /// The audio's lock, held from the recording until the takes are cut.
    _lock: Option<WavLock>,
    /// What to say with its takes.
    notes: Option<String>,
    /// The worker cutting it, while one runs.
    worker: Option<Receiver<Result<Cut, String>>>,
    /// Whether a failed cut was already shown.
    failed: bool,
}

/// What a worker cut a passage into.
struct Cut {
    takes: Vec<(TakeRef, Option<f64>)>,
    unread: Vec<String>,
    /// Another process is writing some of the takes: look again.
    pending: bool,
}

/// A stopped take that is not on disk yet.
struct Unsaved {
    line: String,
    input: RecordedInput,
    samples: Vec<i32>,
    partial: Option<PartialTake>,
    /// The partial WAV's lock, held from the first write (or the rewrite
    /// from memory) until the take is committed or the window exits.
    lock: Option<WavLock>,
    /// Whether the partial WAV already holds every sample.
    complete: bool,
    /// Its rescue copy, once one is written (see [`Recorder::rescue`]).
    rescued: Option<PathBuf>,
    /// What to say of the take with its result (its dropouts, an output
    /// lost during it), also when it is saved only on a later try.
    notes: Option<String>,
}

/// Whether a stopped take or passage is whole on disk, where a repair
/// finds it once the window is gone: its partial audio holds every sample
/// and is there, with its metadata.
fn on_disk(complete: bool, files: Option<(&Path, &Path)>) -> bool {
    complete && files.is_some_and(|(wav, meta)| wav.is_file() && meta.is_file())
}

impl Unsaved {
    /// Whether closing now would lose the take: it is not whole on disk.
    fn only_in_memory(&self) -> bool {
        let files = self.partial.as_ref().map(|p| (&*p.wav, &*p.meta));
        !on_disk(self.complete, files)
    }
}

impl Stopped {
    /// Whether closing now would lose the passage: it is not whole on disk.
    fn only_in_memory(&self) -> bool {
        let files = self.unsaved.partial.as_ref().map(|p| (&*p.wav, &*p.meta));
        !on_disk(self.unsaved.complete, files)
    }
}

/// The rescue copies [`Recorder::rescue`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rescued {
    /// The folder they are in.
    pub folder: PathBuf,
    pub takes: usize,
    pub passages: usize,
}

/// Why [`Recorder::rescue`] did not give everything a rescue copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RescueFailed {
    /// What failed first.
    pub error: String,
    /// The copies that were written all the same.
    pub rescued: Option<Rescued>,
    /// The audio that was written without its JSON file.
    pub audio_only: Option<AudioOnly>,
}

/// Audio that [`Recorder::rescue`] wrote without its JSON file: the WAVs
/// are in the folder, but nothing with them says where they belong, so it
/// still counts as having no rescue copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioOnly {
    /// The folder the WAVs are in.
    pub folder: PathBuf,
    pub takes: usize,
    pub passages: usize,
    /// Why the first of the JSON files could not be written.
    pub error: String,
}

impl RescueFailed {
    /// Nothing was written, because of `error`.
    pub fn new(error: &impl ToString) -> Self {
        Self {
            error: error.to_string(),
            rescued: None,
            audio_only: None,
        }
    }
}

enum Warmup {
    Pending,
    Running { until_host_s: f64 },
    Done,
}

enum Phase {
    Idle,
    Recording(Box<Take>),
    Playing(Playback),
}

struct Take {
    line: String,
    line_start: f64,
    snapshot: Snapshot,
    /// The line's note and the next line's text when the take started: the
    /// prompter keeps them, like the text, until the take stops.
    note: Option<String>,
    next: Option<String>,
    device: String,
    channel: u16,
    latency_s: f64,
    offset_ms: f64,
    plan: RecordPlan,
    /// Host time the capture was started.
    capture_started_host_s: f64,
    /// Schedules the film once the first buffer arrived, then follows it.
    clock: FilmClock,
    /// Receives the capture and keeps and writes the audio.
    writer: TakeWriter,
    /// The take on disk: begun once the first buffer places the take.
    partial: Option<PartialTake>,
    /// Why the take could not be begun on disk (it is kept in memory).
    begin_error: Option<String>,
    /// Whether the writer's error was shown.
    write_error_shown: bool,
    /// Host time at which the film reaches the line's start, once scheduled.
    line_start_host_s: Option<f64>,
    /// The film's duration and the project's `tail_s`: a take whose stop
    /// lies past the film's end stops `tail_s` after the film ends.
    film_end_s: f64,
    tail_s: f64,
    /// Host time at which the film reached its end, once it did.
    film_ended_host_s: Option<f64>,
    /// The passage, when the take records one: several lines in one go.
    passage: Option<PassageTake>,
    /// What happened to the output the film played through during the
    /// take (it went away, or the system output changed): said with the
    /// take's result.
    route_lost: Option<String>,
}

/// What a passage take has beyond a line take: its lines, the one being
/// read and its files. The take's own `line`, `snapshot`, `note` and `next`
/// are those of its first line.
struct PassageTake {
    /// What the passage's sidecar says: the device, the latency and the
    /// lines from the one the take started on to the passage's end, as they
    /// were when it started. Where it lies on the film is filled in once
    /// its first sample is placed.
    input: PassageInput,
    /// Each line's note.
    notes: Vec<Option<String>>,
    /// The line being read: the last one whose start the film reached.
    reading: usize,
    /// The text of the line after the passage, shown as next under its
    /// last line.
    after: Option<String>,
    /// The passage on disk: begun once the first buffer places it.
    partial: Option<PartialPassage>,
    /// The first sample's film time as the sidecar on disk has it.
    sidecar_first_film_s: Option<f64>,
}

impl PassageTake {
    /// The line being read.
    fn line(&self) -> Option<&PassageLine> {
        self.input.lines.get(self.reading)
    }

    /// Moves on to each line whose start the film reached.
    fn follow(&mut self, film_s: f64) {
        while self
            .input
            .lines
            .get(self.reading + 1)
            .is_some_and(|next| film_s >= next.snapshot.start)
        {
            self.reading += 1;
        }
    }
}

impl Take {
    /// The line being read, during a passage take.
    fn cue(&self) -> Option<&PassageLine> {
        self.passage.as_ref()?.line()
    }
}

struct Playback {
    film_from_s: f64,
    hold_s: f64,
    stop_film_s: f64,
    /// Film time of the take's first sample.
    take_first_film_s: f64,
    film_gain_db: f64,
    /// The level the take plays at (`[record] voice_gain_db`).
    voice_gain_db: f64,
    opened_host_s: f64,
    /// The film's scheduled timebase once both players were scheduled,
    /// and the host time the first of them starts at.
    started: Option<Timebase>,
    starts_host_s: f64,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            phase: Phase::Idle,
            warmup: Warmup::Pending,
            park_s: 0.0,
            level: None,
            tick_peak: None,
            meter: None,
            meter_input: None,
            meter_retry_at: 0.0,
            clip: None,
            tap: None,
            tap_broken: false,
            unsaved: Vec::new(),
            unsaved_retry_at: 0.0,
            save_error: None,
            read_last: None,
            unsaved_passages: Vec::new(),
            uncut: Vec::new(),
            start_hold: StartHold::Go,
        }
    }

    /// What the take writer has seen of the take being recorded.
    #[cfg(test)]
    pub fn progress(&self) -> Option<Progress> {
        match &self.phase {
            Phase::Recording(take) => Some(take.writer.progress()),
            _ => None,
        }
    }

    /// Makes finishing the take's partial WAV fail when it stops.
    #[cfg(test)]
    pub fn fail_next_finish(&self) {
        if let Phase::Recording(take) = &self.phase {
            take.writer.fail_finish();
        }
    }

    /// Whether stopped takes still wait to be saved, or passages to be
    /// saved or cut into their takes.
    pub fn has_unsaved(&self) -> bool {
        !self.unsaved.is_empty() || !self.unsaved_passages.is_empty() || !self.uncut.is_empty()
    }

    /// How many stopped takes and passages have audio only in memory: not
    /// saved, and not complete on disk either, where a repair would find
    /// it after the window closed. Closing now would lose them.
    pub fn in_memory_only(&self) -> (usize, usize) {
        let takes = self.unsaved.iter().filter(|u| u.only_in_memory()).count();
        let passages = self.unsaved_passages.iter();
        (takes, passages.filter(|s| s.only_in_memory()).count())
    }

    /// How many takes and passages exist only in memory and have no
    /// rescue copy either: closing now would lose them for good.
    pub fn without_rescue_copy(&self) -> (usize, usize) {
        let takes = self.unsaved.iter();
        let passages = self.unsaved_passages.iter();
        (
            takes
                .filter(|u| u.only_in_memory() && u.rescued.is_none())
                .count(),
            passages
                .filter(|s| s.only_in_memory() && s.rescued.is_none())
                .count(),
        )
    }

    /// Whether audio exists only in memory and has no rescue copy either.
    pub fn needs_rescue(&self) -> bool {
        self.without_rescue_copy() != (0, 0)
    }

    /// Writes a rescue copy of every stopped take and passage whose audio
    /// is only in memory and has none yet, into a new folder in `root`
    /// (see [`takewise_core::rescue`]): the audio and everything the
    /// project of `dir` would store with it. The audio stays in memory,
    /// and saving it into the project is still retried.
    ///
    /// `Ok(None)` when nothing needed one. On an error some copies may
    /// have been written, and some audio without its JSON file, which the
    /// error says; what has no whole copy yet still needs one, and this
    /// can be called again.
    pub fn rescue(
        &mut self,
        root: &Path,
        dir: &Path,
        at: jiff::Timestamp,
    ) -> Result<Option<Rescued>, Box<RescueFailed>> {
        if !self.needs_rescue() {
            return Ok(None);
        }
        let mut rescue =
            Rescue::create(root, dir, at).map_err(|err| Box::new(RescueFailed::new(&err)))?;
        // What was copied, to be marked once the folder is durable.
        let (mut takes, mut passages) = (Vec::new(), Vec::new());
        // The audio that got its WAV and no JSON file: takes, passages,
        // and why the first JSON file could not be written.
        let mut audio_only = (0, 0, None);
        let mut failed = None;
        for (index, unsaved) in self.unsaved.iter().enumerate() {
            if !unsaved.only_in_memory() || unsaved.rescued.is_some() {
                continue;
            }
            match rescue.add_take(&unsaved.line, &unsaved.input, &unsaved.samples) {
                Ok(wav) => takes.push((index, wav)),
                Err(err) => {
                    if let takewise_core::Error::RescueJson { source, .. } = &err {
                        audio_only.0 += 1;
                        audio_only.2 = audio_only.2.or(Some(source.to_string()));
                    }
                    failed = failed.or(Some(err.to_string()));
                }
            }
        }
        for (index, stopped) in self.unsaved_passages.iter().enumerate() {
            if !stopped.only_in_memory() || stopped.rescued.is_some() {
                continue;
            }
            match rescue.add_passage(&stopped.unsaved.input, &stopped.unsaved.samples) {
                Ok(wav) => passages.push((index, wav)),
                Err(err) => {
                    if let takewise_core::Error::RescueJson { source, .. } = &err {
                        audio_only.1 += 1;
                        audio_only.2 = audio_only.2.or(Some(source.to_string()));
                    }
                    failed = failed.or(Some(err.to_string()));
                }
            }
        }
        if takes.is_empty() && passages.is_empty() && audio_only.2.is_none() {
            // Nothing could be written: no empty folder is left behind.
            let _ = std::fs::remove_dir(rescue.folder());
            let failed = failed.unwrap_or_else(|| "nothing was copied".to_owned());
            return Err(Box::new(RescueFailed::new(&failed)));
        }
        // A WAV without its JSON file is made durable like a whole copy:
        // it is all there is of that audio outside this process.
        let folder = rescue
            .finish()
            .map_err(|err| Box::new(RescueFailed::new(&err)))?;
        let rescued = (!takes.is_empty() || !passages.is_empty()).then(|| Rescued {
            folder: folder.clone(),
            takes: takes.len(),
            passages: passages.len(),
        });
        for (index, wav) in takes {
            if let Some(unsaved) = self.unsaved.get_mut(index) {
                unsaved.rescued = Some(wav);
            }
        }
        for (index, wav) in passages {
            if let Some(stopped) = self.unsaved_passages.get_mut(index) {
                stopped.rescued = Some(wav);
            }
        }
        match failed {
            Some(error) => Err(Box::new(RescueFailed {
                error,
                rescued,
                audio_only: audio_only.2.map(|error| AudioOnly {
                    folder,
                    takes: audio_only.0,
                    passages: audio_only.1,
                    error,
                }),
            })),
            None => Ok(rescued),
        }
    }

    /// Why saving failed last, while a take or passage waits to be saved.
    pub fn save_error(&self) -> Option<&str> {
        self.save_error.as_deref()
    }

    pub fn is_recording(&self) -> bool {
        matches!(self.phase, Phase::Recording(_))
    }

    pub fn is_playing(&self) -> bool {
        matches!(self.phase, Phase::Playing(_))
    }

    /// The line being recorded: during a passage take, the one being read.
    pub fn recording_line(&self) -> Option<&str> {
        match &self.phase {
            Phase::Recording(take) => Some(take.cue().map_or(&take.line, |cue| &cue.id)),
            _ => None,
        }
    }

    /// The line as the take being recorded started with it: during a
    /// passage take, the one being read.
    pub fn recording_snapshot(&self) -> Option<&Snapshot> {
        match &self.phase {
            Phase::Recording(take) => Some(take.cue().map_or(&take.snapshot, |cue| &cue.snapshot)),
            _ => None,
        }
    }

    /// The line's note and the next line's text as the take being recorded
    /// started with them: during a passage take, those of the line being
    /// read.
    pub fn recording_cues(&self) -> Option<(Option<&str>, Option<&str>)> {
        let Phase::Recording(take) = &self.phase else {
            return None;
        };
        let Some(passage) = &take.passage else {
            return Some((take.note.as_deref(), take.next.as_deref()));
        };
        let note = passage
            .notes
            .get(passage.reading)
            .and_then(Option::as_deref);
        let next = passage
            .input
            .lines
            .get(passage.reading + 1)
            .map(|line| line.snapshot.text.as_str())
            .or(passage.after.as_deref());
        Some((note, next))
    }

    /// Whether a passage take is being recorded.
    pub fn is_recording_passage(&self) -> bool {
        matches!(&self.phase, Phase::Recording(take) if take.passage.is_some())
    }

    /// During a passage take, the text of the line read before the one
    /// being read, for [`PASSED_S`] of film after the film reached the
    /// line being read.
    pub fn passed_line(&self, media: &dyn Media) -> Option<&str> {
        let Phase::Recording(take) = &self.phase else {
            return None;
        };
        let passage = take.passage.as_ref()?;
        let before = passage.input.lines.get(passage.reading.checked_sub(1)?)?;
        let since = take.clock.film_at(media.host_now_s())? - passage.line()?.snapshot.start;
        (since < PASSED_S).then_some(before.snapshot.text.as_str())
    }

    /// Which line of a passage take is being read (from 0), and that
    /// line's id; after the take, the line it stopped on, until the next
    /// take starts.
    pub fn passage_reading(&self) -> Option<(usize, &str)> {
        let Phase::Recording(take) = &self.phase else {
            return self
                .read_last
                .as_ref()
                .map(|(index, id)| (*index, id.as_str()));
        };
        let passage = take.passage.as_ref()?;
        Some((passage.reading, &passage.line()?.id))
    }

    /// The input level in dBFS, while buffers arrive.
    pub fn level_dbfs(&self, media: &dyn Media) -> Option<f64> {
        let (db, at) = match &self.phase {
            Phase::Recording(take) => take.writer.progress().level?,
            _ => self.level?,
        };
        (media.host_now_s() - at < LEVEL_HOLD_S).then_some(db)
    }

    /// The highest peak to judge clipping by: of the take so far while one
    /// records, else of the buffers the last tick received.
    pub fn clip_peak_dbfs(&self) -> Option<f64> {
        match &self.phase {
            Phase::Recording(take) => take.writer.progress().peak_dbfs,
            _ => self.tick_peak,
        }
    }

    /// Starts keeping what the meter's capture hears, from the next tick on,
    /// as a clip that is not a take; `false` (and nothing kept) while the
    /// meter does not run.
    pub fn start_clip(&mut self) -> bool {
        self.clip = self.meter.is_some().then(Vec::new);
        self.clip.is_some()
    }

    /// Ends the clip and hands over what it kept.
    pub fn take_clip(&mut self) -> Option<Vec<i32>> {
        self.clip.take()
    }

    /// Starts handing over the meter's capture buffers, as captured, from
    /// the next tick on; `false` (and nothing kept) while the meter does not
    /// run.
    pub fn start_tap(&mut self) -> bool {
        self.tap = self.meter.is_some().then(Vec::new);
        self.tap_broken = false;
        self.tap.is_some()
    }

    /// The buffers the tap kept since the last call.
    pub fn drain_tap(&mut self) -> Vec<CaptureBuffer> {
        self.tap.as_mut().map(mem::take).unwrap_or_default()
    }

    /// Whether the meter's capture stopped or moved to another input since
    /// the tap began.
    pub fn tap_broken(&self) -> bool {
        self.tap_broken
    }

    pub fn stop_tap(&mut self) {
        self.tap = None;
        self.tap_broken = false;
    }

    /// Plays the film muted briefly again before the next take: a newly
    /// opened film starts cold.
    pub fn rewarm(&mut self) {
        self.warmup = Warmup::Pending;
    }

    /// Rests the film at `film_s` while nothing plays.
    pub fn park(&mut self, media: &mut dyn Media, film_s: f64) {
        self.park_s = film_s;
        if matches!(self.phase, Phase::Idle) && !matches!(self.warmup, Warmup::Running { .. }) {
            media.film_seek(film_s);
        }
    }

    /// The film time on screen: the scheduled mapping while recording or
    /// playing, else the parked position.
    pub fn film_now_s(&self, media: &dyn Media) -> f64 {
        self.playhead_s(media).unwrap_or(self.park_s)
    }

    /// The playhead while the film plays (recording or play in context).
    pub fn playhead_s(&self, media: &dyn Media) -> Option<f64> {
        let now = media.host_now_s();
        match &self.phase {
            Phase::Recording(take) => take.clock.film_at(now),
            Phase::Playing(Playback {
                started: Some(tb), ..
            }) => Some(tb.film_at(now)),
            _ => None,
        }
    }

    /// Whole seconds left before the line starts, during the count-in: its
    /// last three seconds, so a longer pre-roll still counts 3-2-1.
    pub fn count_in(&self, media: &dyn Media) -> Option<u32> {
        let Phase::Recording(take) = &self.phase else {
            return None;
        };
        let left = take.line_start_host_s? - media.host_now_s();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        (left > 0.0 && left <= f64::from(COUNT_IN)).then(|| left.ceil() as u32)
    }

    /// Keeps the meter's capture running on `input` while idle; stops it
    /// when `input` is `None` (recording is blocked).
    pub fn keep_meter(&mut self, media: &mut dyn Media, input: Option<(&str, u16)>) {
        if !matches!(self.phase, Phase::Idle) {
            return;
        }
        match input {
            None => {
                if self.meter.take().is_some() {
                    media.stop_capture();
                }
            }
            Some((device, channel)) => {
                let now = media.host_now_s();
                let moved = self
                    .meter_input
                    .as_ref()
                    .is_some_and(|(d, c)| d != device || *c != channel);
                if self.meter.is_some() && moved {
                    // Another device or channel was chosen: meter that.
                    self.meter = None;
                    media.stop_capture();
                    self.level = None;
                }
                if self.meter.is_some() && media.capture_error().is_some() {
                    self.meter = None;
                    media.stop_capture();
                    self.meter_retry_at = now + METER_RETRY_S;
                }
                if self.meter.is_none() && now >= self.meter_retry_at {
                    let (tx, rx) = mpsc::channel();
                    match media.start_capture(device, channel, tx) {
                        Ok(()) => {
                            self.meter = Some(rx);
                            self.meter_input = Some((device.to_owned(), channel));
                        }
                        Err(_) => self.meter_retry_at = now + METER_RETRY_S,
                    }
                }
            }
        }
        let metering = self.meter_input.as_ref().map(|(d, c)| (d.as_str(), *c));
        if self.tap.is_some() && (self.meter.is_none() || metering != input) {
            self.tap_broken = true;
        }
    }

    /// Starts a take of line `idx`: capture starts now; once its first
    /// buffer arrives, the film is scheduled to play from the pre-roll
    /// (holding its first frame when the line starts within the pre-roll),
    /// with the count-in. The latency offset applies only when `route`, the
    /// route in use, is the one `[calibration]` measured.
    ///
    /// With `whole_passage` the take runs from line `idx` through the end
    /// of its passage in one go: one capture and one film playback, the
    /// pre-roll and count-in only before the first line, the lines as they
    /// are now. With one line left in the passage it is a take of that line.
    pub fn start_take(
        &mut self,
        media: &mut dyn Media,
        project: &Project,
        idx: usize,
        route: &Route,
        whole_passage: bool,
    ) -> Result<(), String> {
        if !matches!(self.phase, Phase::Idle) {
            return Err("A take or playback is already running.".to_owned());
        }
        let line = project
            .script
            .lines
            .get(idx)
            .ok_or("The script has no lines.")?;
        let input = &project.config.input;
        let device = input
            .device
            .clone()
            .ok_or("No input device chosen; set [input] device in project.toml.")?;
        if !media.film_ready() {
            return Err("The film is still loading; try again in a moment.".to_owned());
        }
        let snapshot = project
            .snapshot(&line.id)
            .ok_or("The line is no longer in the script.")?;
        let latency_s = media.input_latency_s(&device)?;
        let offset_ms = f64::from(effective_latency_offset(&project.config, route));
        let rec = &project.config.record;
        let lines = &project.script.lines;
        // With one line left in the passage, it is a take of that line.
        let passage = project
            .passage_of(idx)
            .filter(|passage| whole_passage && passage.end > idx + 1)
            .map(|passage| PassageTake {
                input: PassageInput {
                    sample_rate: RECORD_RATE,
                    device: device.clone(),
                    channel: input.channel,
                    latency: Latency {
                        input_s: latency_s,
                        offset_ms,
                    },
                    preroll_s: rec.preroll_s,
                    lines: (idx..passage.end)
                        .filter_map(|at| {
                            let id = &lines.get(at)?.id;
                            Some(PassageLine {
                                id: id.clone(),
                                snapshot: project.snapshot(id)?,
                            })
                        })
                        .collect(),
                    first_sample_film_s: None,
                    dropouts: None,
                    film_shift_ms: None,
                },
                notes: (idx..passage.end)
                    .map(|at| lines.get(at).and_then(|line| line.note.clone()))
                    .collect(),
                reading: 0,
                after: lines.get(passage.end).map(|line| line.text.clone()),
                partial: None,
                sidecar_first_film_s: None,
            });
        let slot_end = passage
            .as_ref()
            .and_then(|passage| passage.input.lines.last())
            .map_or(snapshot.end, |last| last.snapshot.end);
        self.end_warmup(media);
        self.stop_meter(media);

        let plan = plan_take(line.start, slot_end, rec.preroll_s, rec.tail_s);
        let (tx, rx) = mpsc::channel();
        if let Err(err) = media.start_capture(&device, input.channel, tx) {
            media.film_seek(self.park_s);
            return Err(format!("Cannot record from {device:?}: {err}"));
        }
        media.film_set_muted(false);
        media.film_set_gain_db(rec.film_gain_db);
        self.level = None;
        self.read_last = None;
        self.phase = Phase::Recording(Box::new(Take {
            line: line.id.clone(),
            line_start: line.start,
            snapshot,
            note: line.note.clone(),
            next: lines.get(idx + 1).map(|next| next.text.clone()),
            device,
            channel: input.channel,
            latency_s,
            offset_ms,
            plan,
            capture_started_host_s: media.host_now_s(),
            clock: FilmClock::new(),
            writer: TakeWriter::spawn(rx),
            partial: None,
            begin_error: None,
            write_error_shown: false,
            line_start_host_s: None,
            film_end_s: project.film_duration(),
            tail_s: rec.tail_s,
            film_ended_host_s: None,
            passage,
            route_lost: None,
        }));
        Ok(())
    }

    /// Stops the take and commits it. A take without any audio is dropped
    /// (nothing was begun on disk); one whose film could not be scheduled,
    /// or was stopped before it was, is placed where the film would have
    /// played, and flagged. When the take cannot be saved, it is kept in
    /// memory and saving is retried; the error says so.
    ///
    /// A passage take is published here (the commit point) and then cut
    /// into its takes on a worker thread: they are reported by a later
    /// [`Recorder::tick`].
    pub fn finish_take(&mut self, media: &mut dyn Media, dir: &Path) -> Update {
        self.stop_take(media, dir, None, false)
    }

    /// [`Recorder::finish_take`], with `why` the take stopped when it was
    /// not the narrator who stopped it, and whether it ran to its end.
    fn stop_take(
        &mut self,
        media: &mut dyn Media,
        dir: &Path,
        why: Option<String>,
        ran_to_end: bool,
    ) -> Update {
        let Phase::Recording(take) = mem::replace(&mut self.phase, Phase::Idle) else {
            return Update::default();
        };
        media.stop_capture();
        let mut take = *take;
        self.read_last = take.passage.as_ref().and_then(|passage| {
            let reading = if ran_to_end { 0 } else { passage.reading };
            let line = passage.input.lines.get(reading)?;
            Some((reading, line.id.clone()))
        });
        let written = take.writer.finish();
        // Stopped after audio arrived but before a frame scheduled the film:
        // the audio is placed by the mapping the film would have had, had it
        // been scheduled when the first buffer arrived.
        let estimated = !written.samples.is_empty() && !take.clock.placed();
        if estimated {
            let first = written.first_host_s.unwrap_or(take.capture_started_host_s);
            let plan = take.plan;
            take.clock
                .assume(first, plan.film_from_s, plan.hold_first_frame_s);
        }
        let unplayed_s = take.clock.unplayed_lateness_s(media, media.host_now_s());
        media.film_pause();
        media.film_seek(self.park_s);
        self.level = None;
        if written.samples.is_empty() {
            return Update {
                message: why,
                ..Update::default()
            };
        }
        let mut placement = place(&take, &written, estimated, unplayed_s);
        // Only a film that ran can have played elsewhere.
        let lost = take.route_lost.take().filter(|_| take.clock.ran());
        placement.notes = join(placement.notes, lost.clone());
        if let Some(passage) = take.passage.take() {
            placement.notes = join(why, placement.notes);
            let mut update = self.publish_passage(dir, passage, written, placement);
            update.route_note = lost;
            return update;
        }
        let mut update = self.commit_line(dir, take, written, placement);
        update.message = join(why, update.message);
        update.route_note = lost;
        update
    }

    /// Commits a stopped take of one line, or keeps it to retry.
    fn commit_line(
        &mut self,
        dir: &Path,
        take: Take,
        written: Written,
        placement: Placement,
    ) -> Update {
        let take_peak = peak_dbfs(&written.samples);
        // The take keeps its film position: placed against the line's slot
        // as it is now, which M or an agent may have moved during the take.
        // The text stays the one read during the take, so an edit made
        // meanwhile shows the take as recorded against old text.
        let snapshot = match Project::open(dir).ok().and_then(|p| p.snapshot(&take.line)) {
            Some(now) => Snapshot {
                text: take.snapshot.text,
                ..now
            },
            None => take.snapshot,
        };
        #[allow(clippy::cast_possible_truncation)]
        let input = RecordedInput {
            sample_rate: RECORD_RATE,
            device: take.device,
            channel: take.channel,
            line_offset_s: line_offset_s(placement.first_film_s, snapshot.start),
            snapshot,
            dropouts: Some(written.dropouts.len().min(u32::MAX as usize) as u32),
            film_shift_ms: placement.film_shift_ms,
            passage: None,
        };
        let unsaved = Unsaved {
            line: take.line,
            input,
            samples: written.samples,
            partial: take.partial,
            lock: written.lock,
            complete: written.complete,
            rescued: None,
            notes: placement.notes.clone(),
        };
        let mut update = self.save(dir, unsaved);
        update.message = join(update.message, placement.notes);
        update.peak_dbfs = update.committed.as_ref().map(|_| take_peak);
        update
    }

    /// Writes and commits a stopped take, or keeps it to retry.
    fn save(&mut self, dir: &Path, mut unsaved: Unsaved) -> Update {
        match save(dir, &mut unsaved) {
            Ok(saved) => Update {
                committed: Some(saved.take),
                message: saved.note,
                ..Update::default()
            },
            Err(err) => {
                let message = format!(
                    "The take of {} could not be saved ({err}); it is kept in memory and saving is retried.",
                    unsaved.line
                );
                self.save_error = Some(err);
                self.unsaved.push(unsaved);
                Update {
                    message: Some(message),
                    ..Update::default()
                }
            }
        }
    }

    /// Publishes a stopped passage take, the commit point, and has its
    /// takes cut on a worker thread; one that cannot be published is kept
    /// in memory and retried.
    fn publish_passage(
        &mut self,
        dir: &Path,
        passage: PassageTake,
        written: Written,
        placement: Placement,
    ) -> Update {
        let stopped = Stopped {
            unsaved: UnsavedPassage {
                input: PassageInput {
                    first_sample_film_s: Some(placement.first_film_s),
                    dropouts: Some(written.dropouts),
                    film_shift_ms: placement.film_shift_ms,
                    ..passage.input
                },
                samples: written.samples,
                partial: passage.partial,
                lock: written.lock,
                complete: written.complete,
            },
            notes: placement.notes,
            rescued: None,
        };
        self.publish(dir, stopped)
    }

    /// Publishes a stopped passage and starts cutting it, or keeps it to
    /// retry.
    fn publish(&mut self, dir: &Path, mut stopped: Stopped) -> Update {
        let message = match publish(dir, &mut stopped.unsaved) {
            Ok(id) => {
                let mut uncut = stopped.published(id);
                start_cut(dir, &mut uncut);
                self.uncut.push(uncut);
                "Saved the passage; cutting it into takes…".to_owned()
            }
            Err(err) => {
                let message = format!(
                    "The passage could not be saved ({err}); it is kept in memory and saving is retried."
                );
                let message = join(stopped.notes.clone(), Some(message)).unwrap_or_default();
                self.save_error = Some(err);
                self.unsaved_passages.push(stopped);
                message
            }
        };
        Update {
            message: Some(message),
            ..Update::default()
        }
    }

    /// Takes in the cuts that finished: reports the first passage whose
    /// takes are all there and releases its audio; one that could not be
    /// cut waits to be cut again.
    fn collect_cuts(&mut self) -> Update {
        let mut update = Update::default();
        let mut left = Vec::new();
        for mut uncut in mem::take(&mut self.uncut) {
            let result = match &uncut.worker {
                Some(worker) if update.passage.is_none() => match worker.try_recv() {
                    Ok(result) => result,
                    Err(TryRecvError::Empty) => {
                        left.push(uncut);
                        continue;
                    }
                    Err(TryRecvError::Disconnected) => Err("the worker stopped".to_owned()),
                },
                _ => {
                    left.push(uncut);
                    continue;
                }
            };
            uncut.worker = None;
            match cut_outcome(uncut, result) {
                Ok(done) => {
                    update.message = join(update.message.take(), done.message);
                    update.passage = done.passage;
                }
                Err((uncut, message)) => {
                    update.message = join(update.message.take(), message);
                    left.push(uncut);
                }
            }
        }
        self.uncut = left;
        update
    }

    /// Waits for the passages being cut and cuts those that wait for it
    /// (when the window closes); what cannot be cut now is cut by the next
    /// repair, from the published passage.
    pub fn finish_cuts_now(&mut self, dir: &Path) -> Vec<Update> {
        let mut updates = Vec::new();
        let mut left = Vec::new();
        for mut uncut in mem::take(&mut self.uncut) {
            let result = match uncut.worker.take() {
                Some(worker) => worker
                    .recv()
                    .unwrap_or_else(|_| Err("the worker stopped".to_owned())),
                None => cut_passage(dir, &uncut.id, &uncut.samples),
            };
            match cut_outcome(uncut, result) {
                Ok(done) => updates.push(done),
                Err((uncut, message)) => {
                    updates.push(Update {
                        message,
                        ..Update::default()
                    });
                    left.push(uncut);
                }
            }
        }
        self.uncut = left;
        updates
    }

    /// Tries once more to save the takes and passages that could not be
    /// saved (when the window closes).
    pub fn save_unsaved_now(&mut self, dir: &Path) -> Update {
        self.unsaved_retry_at = f64::NEG_INFINITY;
        self.retry_unsaved(dir, f64::NEG_INFINITY)
    }

    /// Retries saving the takes that could not be saved (the first one
    /// saved is reported), publishing the passages that could not be, and
    /// cutting the passages whose cut failed.
    fn retry_unsaved(&mut self, dir: &Path, now: f64) -> Update {
        let waiting = !self.unsaved.is_empty()
            || !self.unsaved_passages.is_empty()
            || self.uncut.iter().any(|uncut| uncut.worker.is_none());
        if !waiting || now < self.unsaved_retry_at {
            return Update::default();
        }
        self.unsaved_retry_at = now + SAVE_RETRY_S;
        for mut stopped in mem::take(&mut self.unsaved_passages) {
            // Silent while it still fails, as for a take: the first
            // failure was shown.
            match publish(dir, &mut stopped.unsaved) {
                Ok(id) => {
                    let note = "The passage that could not be saved is saved now.".to_owned();
                    let note = join(Some(note), spare_copy_note(stopped.rescued.as_deref()));
                    stopped.notes = join(note, stopped.notes.take());
                    self.uncut.push(stopped.published(id));
                }
                Err(err) => {
                    self.save_error = Some(err);
                    self.unsaved_passages.push(stopped);
                }
            }
        }
        for uncut in self.uncut.iter_mut().filter(|uncut| uncut.worker.is_none()) {
            start_cut(dir, uncut);
        }
        let mut update = Update::default();
        let mut left = Vec::new();
        for mut unsaved in mem::take(&mut self.unsaved) {
            let spare = spare_copy_note(unsaved.rescued.as_deref());
            match save(dir, &mut unsaved) {
                Ok(saved) if update.committed.is_none() => {
                    let committed = saved.take;
                    let note = format!(
                        "Saved the earlier take of {} as take {}.",
                        committed.line, committed.take
                    );
                    let note = join(Some(note), unsaved.notes.take());
                    update.message = join(saved.note, join(note, spare));
                    update.committed = Some(committed);
                }
                Ok(saved) => {
                    let note = join(saved.note, join(unsaved.notes.take(), spare));
                    update.message = join(update.message.take(), note);
                }
                Err(err) => {
                    self.save_error = Some(err);
                    left.push(unsaved);
                }
            }
        }
        self.unsaved = left;
        if self.unsaved.is_empty() && self.unsaved_passages.is_empty() {
            self.save_error = None;
        }
        update
    }

    /// Plays line `idx` in context: pre-roll, the take locked to the film,
    /// and the tail. Starts once the take has loaded. A piece of a passage
    /// plays through its own end and the tail, as it ends at the cut before
    /// the next line, which can lie past the slot's tail; it can even begin
    /// there, when the line's slot was shortened after the recording.
    pub fn start_playback(
        &mut self,
        media: &mut dyn Media,
        project: &Project,
        idx: usize,
        take: &TakeState,
    ) -> Result<(), String> {
        if !matches!(self.phase, Phase::Idle) {
            return Err("A take or playback is already running.".to_owned());
        }
        let line = project
            .script
            .lines
            .get(idx)
            .ok_or("The script has no lines.")?;
        if take.missing {
            return Err(format!(
                "Take {}'s file {} is missing.",
                take.take, take.file
            ));
        }
        let snapshot = project
            .snapshot(&line.id)
            .ok_or("The line is no longer in the script.")?;
        self.end_warmup(media);
        media.open_take(&project.dir.join(&take.file))?;
        let rec = &project.config.record;
        let plan = plan_take(line.start, snapshot.end, rec.preroll_s, rec.tail_s);
        let take_first_film_s = line.start + take.line_offset_s;
        let stop_film_s = take
            .passage
            .as_ref()
            .map_or(plan.capture_stop_film_s, |piece| {
                let samples = piece.to_sample.saturating_sub(piece.from_sample);
                let end_s = take_first_film_s + samples as f64 / f64::from(RECORD_RATE);
                plan.capture_stop_film_s.max(end_s + rec.tail_s)
            });
        self.phase = Phase::Playing(Playback {
            film_from_s: plan.film_from_s,
            hold_s: plan.hold_first_frame_s,
            stop_film_s,
            take_first_film_s,
            film_gain_db: rec.film_gain_db,
            voice_gain_db: rec.voice_gain_db,
            opened_host_s: media.host_now_s(),
            started: None,
            starts_host_s: f64::INFINITY,
        });
        Ok(())
    }

    /// Plays the passage a take was cut from in context, from that take's
    /// line to the passage's end: the passage file locked to the film. From
    /// the passage's first piece, the film starts at the passage's first
    /// sample (holding the film's first frame when that lies before the
    /// film); from a later piece, it starts one pre-roll before that
    /// piece's line as recorded (or before the piece, when it begins
    /// earlier), and the passage plays from the sample the film starts at.
    /// Starts once the file has loaded.
    pub fn start_passage_playback(
        &mut self,
        media: &mut dyn Media,
        project: &Project,
        piece: &PassagePiece,
    ) -> Result<(), String> {
        if !matches!(self.phase, Phase::Idle) {
            return Err("A take or playback is already running.".to_owned());
        }
        let passage = read_passage(&project.dir, &piece.id)
            .map_err(|err| format!("Cannot play the passage: {err}"))?;
        let file = passage.file();
        let (Some(first_film_s), Some(samples)) =
            (passage.input.first_sample_film_s, passage.samples)
        else {
            return Err(format!("The passage {file} is not placed on the film."));
        };
        let path = project.dir.join(&file);
        if !path.is_file() {
            return Err(format!("The passage's file {file} is missing."));
        }
        let rate = f64::from(passage.input.sample_rate.max(1));
        let line = passage
            .input
            .lines
            .get(piece.index as usize)
            .filter(|_| !piece.is_first());
        let (film_from_s, hold_s) = match line {
            None => (first_film_s.max(0.0), (-first_film_s).max(0.0)),
            Some(line) => {
                let piece_film_s = first_film_s + piece.from_sample as f64 / rate;
                let from = line.snapshot.start.min(piece_film_s);
                let plan = plan_take(from, from, project.config.record.preroll_s, 0.0);
                (plan.film_from_s, plan.hold_first_frame_s)
            }
        };
        self.end_warmup(media);
        media.open_take(&path)?;
        let length_s = samples as f64 / rate;
        self.phase = Phase::Playing(Playback {
            film_from_s,
            hold_s,
            stop_film_s: first_film_s + length_s,
            take_first_film_s: first_film_s,
            film_gain_db: project.config.record.film_gain_db,
            voice_gain_db: project.config.record.voice_gain_db,
            opened_host_s: media.host_now_s(),
            started: None,
            starts_host_s: f64::INFINITY,
        });
        Ok(())
    }

    /// The film and voice levels a playback starts with, when they change
    /// before it started (the window applies them to running players).
    pub fn set_playback_levels(&mut self, film_db: f64, voice_db: f64) {
        if let Phase::Playing(playback) = &mut self.phase {
            playback.film_gain_db = film_db;
            playback.voice_gain_db = voice_db;
        }
    }

    /// Before each tick: whether a playback or a take's film that is about
    /// to start, or whose start is to come, may go on.
    pub fn hold_start(&mut self, hold: StartHold) {
        self.start_hold = hold;
    }

    /// A playback opened and not playing yet at host time `now`: its take
    /// may still load, or both are scheduled and their start is to come.
    pub fn playback_waiting(&self, now: f64) -> bool {
        matches!(&self.phase, Phase::Playing(playback)
            if playback.started.is_none() || now < playback.starts_host_s)
    }

    /// A take recording whose film was not seen running yet: not
    /// scheduled, its start to come, or a player that has not moved.
    pub fn film_waiting(&self) -> bool {
        matches!(&self.phase, Phase::Recording(take) if !take.clock.ran())
    }

    /// The output the film plays through went away or changed during the
    /// take: the take goes on, and its result says `note`, once. Only once
    /// its film was seen running, since before that nothing was heard;
    /// returns whether the note was taken.
    pub fn note_route(&mut self, note: String) -> bool {
        match &mut self.phase {
            Phase::Recording(take) if take.clock.ran() => {
                take.route_lost.get_or_insert(note);
                true
            }
            _ => false,
        }
    }

    pub fn stop_playback(&mut self, media: &mut dyn Media) {
        if matches!(self.phase, Phase::Playing(_)) {
            self.phase = Phase::Idle;
            media.stop_take();
            media.film_pause();
            media.film_seek(self.park_s);
        }
    }

    /// Runs once per frame: the warm-up, the meter, collecting captured
    /// audio, stopping a take at its end or on a capture error, and starting
    /// and ending play in context.
    pub fn tick(&mut self, media: &mut dyn Media, dir: &Path) -> Update {
        let now = media.host_now_s();
        self.tick_warmup(media, now);
        self.tick_peak = None;
        if let Some(meter) = &self.meter {
            while let Ok(buffer) = meter.try_recv() {
                if let Some(clip) = &mut self.clip {
                    clip.extend_from_slice(&buffer.samples);
                }
                if !buffer.samples.is_empty() {
                    let peak = peak_dbfs(&buffer.samples);
                    self.level = Some((peak, now));
                    self.tick_peak = Some(self.tick_peak.map_or(peak, |max| max.max(peak)));
                }
                if let Some(tap) = &mut self.tap {
                    tap.push(buffer);
                }
            }
        }
        let cut = self.collect_cuts();
        let update = self.tick_phase(media, dir, now);
        Update {
            committed: update.committed,
            peak_dbfs: update.peak_dbfs,
            passage: cut.passage,
            message: join(update.message, cut.message),
            route_note: update.route_note,
        }
    }

    /// The part of [`Recorder::tick`] that depends on what runs.
    fn tick_phase(&mut self, media: &mut dyn Media, dir: &Path, now: f64) -> Update {
        let hold = mem::take(&mut self.start_hold);
        match &mut self.phase {
            Phase::Idle => self.retry_unsaved(dir, now),
            Phase::Recording(take) => {
                let progress = take.writer.progress();
                // The film is scheduled once the first buffer is in; not
                // when its output can no longer be used: a start not yet
                // seen running is called off then, whatever the time, the
                // film paused at once. One not confirmed waits.
                let failure = match hold {
                    StartHold::Stop(why)
                        if !take.clock.ran() && progress.first_host_s.is_some() =>
                    {
                        media.film_pause();
                        Some(why)
                    }
                    StartHold::Wait if !take.clock.started() => None,
                    _ => start_film(take, media, &progress, now),
                };
                let mut message = begin_on_disk(take, dir, &progress);
                if let (Some(passage), Some(film_s)) = (&mut take.passage, take.clock.film_at(now))
                {
                    passage.follow(film_s);
                }
                if let Some(err) = progress.error.as_ref().filter(|_| !take.write_error_shown) {
                    take.write_error_shown = true;
                    message = join(
                        message,
                        Some(format!(
                            "Writing the take failed ({err}); recording goes on, the audio is kept in memory and written when you stop."
                        )),
                    );
                }
                let failure = failure.or_else(|| media.capture_error());
                let at_end = reached_stop(take, now);
                if failure.is_none() && !at_end {
                    return Update {
                        message,
                        ..Update::default()
                    };
                }
                let ran_to_end = failure.is_none();
                let why = failure.map(|failure| format!("Recording stopped: {failure}"));
                self.stop_take(media, dir, why, ran_to_end)
            }
            Phase::Playing(playback) => match playback.started {
                Some(tb) if tb.film_at(now) >= playback.stop_film_s => {
                    self.stop_playback(media);
                    Update {
                        ..Update::default()
                    }
                }
                _ if matches!(hold, StartHold::Stop(_)) => {
                    self.stop_playback(media);
                    let StartHold::Stop(why) = hold else {
                        return Update::default();
                    };
                    Update {
                        message: Some(why),
                        ..Update::default()
                    }
                }
                None if hold == StartHold::Wait => Update::default(),
                None if media.take_ready() && media.film_ready() => {
                    match start_both(media, playback, now) {
                        Ok(tb) => {
                            playback.started = Some(tb);
                            playback.starts_host_s = now + START_LEAD_S;
                            Update::default()
                        }
                        Err(err) => {
                            self.stop_playback(media);
                            Update {
                                message: Some(format!("Cannot play the take: {err}")),
                                ..Update::default()
                            }
                        }
                    }
                }
                None if now - playback.opened_host_s > TAKE_LOAD_TIMEOUT_S => {
                    self.stop_playback(media);
                    Update {
                        message: Some("The take did not load.".to_owned()),
                        ..Update::default()
                    }
                }
                _ => Update::default(),
            },
        }
    }

    fn tick_warmup(&mut self, media: &mut dyn Media, now: f64) {
        match self.warmup {
            Warmup::Pending if media.film_ready() && matches!(self.phase, Phase::Idle) => {
                media.film_set_muted(true);
                self.warmup = match media.film_play_at(self.park_s, now) {
                    Ok(_) => Warmup::Running {
                        until_host_s: now + WARMUP_S,
                    },
                    Err(_) => Warmup::Done,
                };
            }
            Warmup::Running { until_host_s } if now >= until_host_s => self.end_warmup(media),
            _ => {}
        }
    }

    fn end_warmup(&mut self, media: &mut dyn Media) {
        if matches!(self.warmup, Warmup::Running { .. }) {
            media.film_pause();
            media.film_set_muted(false);
            media.film_seek(self.park_s);
        }
        self.warmup = Warmup::Done;
    }

    fn stop_meter(&mut self, media: &mut dyn Media) {
        if self.meter.take().is_some() {
            media.stop_capture();
        }
    }
}

/// Schedules the film once the take's first buffer arrived and follows it;
/// returns why the take must stop: no audio, or a film that cannot play or
/// did not start.
fn start_film(
    take: &mut Take,
    media: &mut dyn Media,
    progress: &Progress,
    now: f64,
) -> Option<String> {
    let mut failure = None;
    if !take.clock.started() {
        if progress.first_host_s.is_some() {
            let plan = take.plan;
            match take
                .clock
                .start(media, plan.film_from_s, plan.hold_first_frame_s)
            {
                Ok(tb) => {
                    take.line_start_host_s =
                        Some(tb.host_anchor_s + (take.line_start - tb.film_anchor_s));
                }
                Err(err) => {
                    // The audio is kept, placed where the film should have
                    // played.
                    let plan = take.plan;
                    take.clock
                        .assume(now, plan.film_from_s, plan.hold_first_frame_s);
                    failure = Some(format!("the film cannot play: {err}"));
                }
            }
        } else if now - take.capture_started_host_s > FIRST_BUFFER_TIMEOUT_S {
            failure = Some(format!(
                "No audio arrived from {:?}; check the input device.",
                take.device
            ));
        }
    }
    take.clock.track(media);
    let overdue = take.clock.overdue_s(now);
    failure.or_else(|| {
        overdue
            .is_some_and(|late| late >= FILM_START_TIMEOUT_S)
            .then(|| "the film did not start; check the film file.".to_owned())
    })
}

/// Whether the take reached its stop: `tail_s` after the slot's end, or,
/// when the film ends first, `tail_s` of host time after the film's end.
/// Notes when the film reached its end (played to it, or halted near it):
/// by the mapping that played it there, not when the halt was seen.
fn reached_stop(take: &mut Take, now: f64) -> bool {
    let Some(film) = take.clock.film_at(now) else {
        return false;
    };
    if take.film_ended_host_s.is_none() {
        let history = take.clock.history();
        let tb = history.timebase_at(now);
        let halted = tb.is_some_and(|tb| tb.rate == 0.0);
        let end = take.film_end_s;
        if film >= end || (halted && film >= end - END_TOLERANCE_S) {
            let reaches = |tb: Timebase, film_s: f64| {
                (tb.rate > 0.0).then(|| tb.host_anchor_s + (film_s - tb.film_anchor_s) / tb.rate)
            };
            let ended = match tb {
                Some(tb) if tb.rate > 0.0 => reaches(tb, end),
                // Halted: when the mapping before the halt reached the
                // frame the film halted at.
                _ => history
                    .entry_at(now)
                    .and_then(|(halted_at, _)| history.timebase_before(halted_at))
                    .and_then(|before| reaches(before, film.min(end))),
            };
            take.film_ended_host_s = Some(ended.unwrap_or(now).min(now));
        }
    }
    film >= take.plan.capture_stop_film_s
        || take
            .film_ended_host_s
            .is_some_and(|ended| now >= ended + take.tail_s)
}

/// Starts the film and the take at the same host time; returns the film's
/// scheduled timebase.
fn start_both(media: &mut dyn Media, playback: &Playback, now: f64) -> Result<Timebase, String> {
    let begin = now + START_LEAD_S;
    media.film_set_muted(false);
    media.film_set_gain_db(playback.film_gain_db);
    let tb = media.film_play_at(playback.film_from_s, begin + playback.hold_s)?;
    media.take_set_gain_db(playback.voice_gain_db);
    let take_host = tb.host_anchor_s + (playback.take_first_film_s - tb.film_anchor_s);
    if take_host > begin {
        // The take's first sample lies after the point the film starts
        // from (a piece of a passage after its first): the take starts at
        // its own host time, from its beginning.
        media.take_play_at(0.0, take_host)?;
    } else {
        media.take_play_at(begin - take_host, begin)?;
    }
    Ok(tb)
}

/// Where a stopped take lies on the film.
struct Placement {
    /// Film time of the take's first sample.
    first_film_s: f64,
    /// The take's `film_shift_ms`.
    film_shift_ms: Option<f64>,
    /// What to say of the take: its dropouts, a film that stalled.
    notes: Option<String>,
}

/// Places a stopped take with the film's timebase as read back (a late
/// start moves the take), by the same mapping the calibration measures.
/// `estimated`: the film was never scheduled; `unplayed_s`: how far a film
/// that never played was behind its schedule.
fn place(take: &Take, written: &Written, estimated: bool, unplayed_s: Option<f64>) -> Placement {
    let history = take.clock.history();
    let first_film_s = written.first_host_s.and_then(|host| {
        sample_film_time(
            history,
            host,
            0,
            RECORD_RATE,
            take.latency_s,
            take.offset_ms,
        )
    });
    // Measured strictly before the film's end, so the player halting
    // there is no shift.
    let film_shift_ms = written.first_host_s.and_then(|host| {
        let from = host - take.latency_s;
        let to = (from + written.samples.len() as f64 / f64::from(RECORD_RATE))
            .min(take.film_ended_host_s.unwrap_or(f64::INFINITY));
        // A film that never played leaves the take on its schedule,
        // off by how late the film was when the take stopped.
        let ms = unplayed_s.unwrap_or_else(|| film_shift_s(history, from, to)) * 1000.0;
        (ms > FILM_SHIFT_TOLERANCE_MS).then_some(ms)
    });
    let shift_note = if estimated {
        Some("Stopped before the film started; placement estimated.".to_owned())
    } else {
        film_shift_ms.map(|ms| {
            if unplayed_s.is_some() {
                format!(
                    "The film did not play during the take; its placement may be off by {ms:.0} ms."
                )
            } else {
                format!("The film stalled during the take; placement may be off by {ms:.0} ms.")
            }
        })
    };
    Placement {
        first_film_s: first_film_s.unwrap_or(take.line_start),
        film_shift_ms,
        notes: join(dropout_note(&written.dropouts, first_film_s), shift_note),
    }
}

/// Film time of the take's first sample, whose buffer has callback host time
/// `first_host_s`.
fn first_sample_film_s(take: &Take, first_host_s: f64) -> Option<f64> {
    sample_film_time(
        take.clock.history(),
        first_host_s,
        0,
        RECORD_RATE,
        take.latency_s,
        take.offset_ms,
    )
}

/// Begins the take on disk once its first sample is placed: writes the
/// metadata and has the writer stream into the partial WAV. On failure the
/// take goes on in memory; returns the message to show.
fn begin_on_disk(take: &mut Take, dir: &Path, progress: &Progress) -> Option<String> {
    if take.passage.is_some() {
        return begin_passage_on_disk(take, dir, progress);
    }
    if take.partial.is_some() || take.begin_error.is_some() || !take.clock.started() {
        return None;
    }
    // Placed by the schedule for now; rewritten with the film's timebase as
    // read back when the take is committed.
    let first = first_sample_film_s(take, progress.first_host_s?).unwrap_or(take.line_start);
    let input = RecordedInput {
        sample_rate: RECORD_RATE,
        device: take.device.clone(),
        channel: take.channel,
        line_offset_s: line_offset_s(first, take.line_start),
        snapshot: take.snapshot.clone(),
        dropouts: None,
        film_shift_ms: None,
        passage: None,
    };
    match begin_take(dir, &take.line, &input) {
        Ok(partial) => {
            take.writer.open(partial.wav.clone());
            take.partial = Some(partial);
            None
        }
        Err(err) => {
            let message = format!(
                "The take cannot be started on disk ({err}); recording goes on, the audio is kept in memory and written when you stop."
            );
            take.begin_error = Some(err.to_string());
            Some(message)
        }
    }
}

/// Begins a passage take on disk once its first sample is placed: writes
/// the sidecar and has the writer stream into the partial audio. The
/// sidecar is written again when the film's timebase, as read back, moves
/// the first sample, so a passage a crash interrupts is cut where it was
/// recorded. On failure the take goes on in memory; returns the message to
/// show.
fn begin_passage_on_disk(take: &mut Take, dir: &Path, progress: &Progress) -> Option<String> {
    if take.begin_error.is_some() || !take.clock.started() {
        return None;
    }
    let first = first_sample_film_s(take, progress.first_host_s?).unwrap_or(take.line_start);
    let passage = take.passage.as_mut()?;
    if passage
        .sidecar_first_film_s
        .is_some_and(|written| (written - first).abs() <= SIDECAR_TOLERANCE_S)
    {
        return None;
    }
    passage.input.first_sample_film_s = Some(first);
    passage.sidecar_first_film_s = Some(first);
    if let Some(partial) = &passage.partial {
        // Left as it was when this fails: the sidecar is written once more
        // when the take stops.
        let _ = update_passage_meta(partial, &passage.input);
        return None;
    }
    match begin_passage(dir, &passage.input) {
        Ok(partial) => {
            take.writer.open(partial.wav.clone());
            passage.partial = Some(partial);
            None
        }
        Err(err) => {
            let message = format!(
                "The passage cannot be started on disk ({err}); recording goes on, the audio is kept in memory and written when you stop."
            );
            take.begin_error = Some(err.to_string());
            Some(message)
        }
    }
}

/// Publishes a stopped passage (see [`save_passage`]); returns its id. On
/// failure the passage keeps everything, so this can be called again.
fn publish(dir: &Path, unsaved: &mut UnsavedPassage) -> Result<String, String> {
    #[cfg(test)]
    if COMMITS_FAIL.get() {
        return Err("the commit failed (test)".to_owned());
    }
    save_passage(dir, unsaved)
        .map(|passage| passage.id)
        .map_err(|err| err.to_string())
}

/// Starts a worker that cuts the published passage into its takes.
/// Without a thread the passage waits to be cut at the next retry.
fn start_cut(dir: &Path, uncut: &mut Uncut) {
    let (done, worker) = mpsc::channel();
    let (dir, id, samples) = (
        dir.to_path_buf(),
        uncut.id.clone(),
        Arc::clone(&uncut.samples),
    );
    let spawned = std::thread::Builder::new()
        .name("takewise-passage-cut".to_owned())
        .spawn(move || {
            let _ = done.send(cut_passage(&dir, &id, &samples));
        });
    uncut.worker = spawned.is_ok().then_some(worker);
}

/// Cuts the published passage `id` into one take per line (see
/// [`passage::split`], which skips the takes that are already there) and
/// measures each take's peak in `samples`, the passage's audio.
fn cut_passage(dir: &Path, id: &str, samples: &[i32]) -> Result<Cut, String> {
    let split = passage::split(dir, id).map_err(|err| err.to_string())?;
    let pieces = read_passage(dir, id)
        .ok()
        .and_then(|passage| passage.pieces())
        .unwrap_or_default();
    let peak = |index: usize| {
        let piece = pieces.get(index)?;
        let range = usize::try_from(piece.start).ok()?..usize::try_from(piece.end).ok()?;
        samples.get(range).map(peak_dbfs)
    };
    Ok(Cut {
        pending: !split.pending.is_empty(),
        takes: split
            .takes
            .into_iter()
            .enumerate()
            .map(|(index, take)| (take, peak(index)))
            .collect(),
        unread: split.lines_without_piece,
    })
}

/// What a finished cut gives: the passage's takes (its audio's lock is
/// released), or the passage to cut again and what to say.
fn cut_outcome(
    mut uncut: Uncut,
    result: Result<Cut, String>,
) -> Result<Update, (Uncut, Option<String>)> {
    match result {
        Ok(cut) if !cut.pending => Ok(Update {
            passage: Some(PassageTakes {
                takes: cut.takes,
                unread: cut.unread,
            }),
            message: uncut.notes,
            ..Update::default()
        }),
        // Another process is writing some of its takes: look again.
        Ok(_) => Err((uncut, None)),
        Err(err) => {
            let message = (!uncut.failed).then(|| {
                format!(
                    "The passage is saved as {}, but could not be cut into takes ({err}); cutting is retried.",
                    passage_file(&uncut.id)
                )
            });
            uncut.failed = true;
            Err((uncut, message))
        }
    }
}

/// A take saved: the take, and a note when saving it took a detour.
struct Saved {
    take: TakeRef,
    note: Option<String>,
}

/// Writes a stopped take's metadata and, when the partial WAV is not
/// complete, its audio from memory, then commits it.
fn save(dir: &Path, unsaved: &mut Unsaved) -> Result<Saved, String> {
    let mut note = None;
    // An earlier commit renamed the take's files but failed before logging
    // it: finish that commit (the take's lock is still held). When the
    // renamed files are gone, the take starts afresh from memory.
    if let Some(partial) = unsaved.partial.as_ref().filter(|p| !p.meta.exists()) {
        match finish_interrupted_commit(dir, partial) {
            Ok(Some(take)) => return Ok(Saved { take, note }),
            Ok(None) => {}
            Err(takewise_core::Error::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                unsaved.partial = None;
                unsaved.lock = None;
                unsaved.complete = false;
                note = Some(format!(
                    "The files of the take of {} vanished before it was committed; it was saved again from memory.",
                    unsaved.line
                ));
            }
            Err(err) => return Err(err.to_string()),
        }
    }
    let partial = if let Some(partial) = &unsaved.partial {
        partial.clone()
    } else {
        let partial =
            begin_take(dir, &unsaved.line, &unsaved.input).map_err(|err| err.to_string())?;
        unsaved.partial = Some(partial.clone());
        partial
    };
    // The partial WAV vanished although its metadata is there: the lock
    // held is on the removed file, so the audio is written again from
    // memory under a new one.
    if !partial.wav.exists() {
        unsaved.lock = None;
        unsaved.complete = false;
    }
    if !unsaved.complete {
        // Written into a locked file beside the partial and renamed over it
        // only when whole, so a failure never shortens the audio already on
        // disk, and a repair never adopts it meanwhile.
        rewrite_take_wav(
            &partial.wav,
            &mut unsaved.lock,
            &unsaved.samples,
            RECORD_RATE,
        )
        .map_err(|err| err.to_string())?;
        unsaved.complete = true;
    }
    update_take_meta(&partial, &unsaved.input).map_err(|err| err.to_string())?;
    #[cfg(test)]
    if COMMITS_FAIL.get() {
        return Err("the commit failed (test)".to_owned());
    }
    let take = commit_take(dir, partial).map_err(|err| err.to_string())?;
    Ok(Saved { take, note })
}

#[cfg(test)]
thread_local! {
    /// Whether commits on this thread fail, as a failing disk makes them.
    static COMMITS_FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Makes commits of takes on this thread fail (or succeed again).
#[cfg(test)]
pub fn commits_fail(fail: bool) {
    COMMITS_FAIL.set(fail);
}

/// What to say when audio with a rescue copy at `wav` was saved into its
/// project after all: the copy is spare now. Nothing removes it.
fn spare_copy_note(wav: Option<&Path>) -> Option<String> {
    let wav = wav?;
    Some(format!(
        "Its rescue copy {} is no longer needed.",
        wav.display()
    ))
}

/// "Audio dropout at 0:05." for the dropouts of a take whose first sample
/// is at `first_film_s`.
fn dropout_note(dropouts: &[Dropout], first_film_s: Option<f64>) -> Option<String> {
    if dropouts.is_empty() {
        return None;
    }
    let first = first_film_s.unwrap_or(0.0);
    let times: Vec<String> = dropouts
        .iter()
        .map(|d| clock(first + d.at_sample as f64 / f64::from(RECORD_RATE)))
        .collect();
    let noun = if dropouts.len() == 1 {
        "Audio dropout"
    } else {
        "Audio dropouts"
    };
    Some(format!(
        "{noun} at {} (filled with silence; listen before choosing).",
        times.join(", ")
    ))
}

/// Film time as m:ss.
fn clock(film_s: f64) -> String {
    #[allow(clippy::cast_possible_truncation)]
    let whole = film_s.abs().floor() as i64;
    let sign = if film_s < 0.0 { "-" } else { "" };
    format!("{sign}{}:{:02}", whole / 60, whole % 60)
}

/// Both messages, or whichever there is.
pub fn join(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(a), Some(b)) => Some(format!("{a} {b}")),
        (a, b) => a.or(b),
    }
}

/// Peak of `samples` in dBFS (24-bit full scale).
pub fn peak_dbfs(samples: &[i32]) -> f64 {
    let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    if peak == 0 {
        return f64::NEG_INFINITY;
    }
    20.0 * (f64::from(peak) / f64::from(FULL_SCALE)).log10()
}
