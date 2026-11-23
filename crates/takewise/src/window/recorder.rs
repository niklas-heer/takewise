//! The recording session: takes, play in context, the input meter and the
//! timing that places captured samples on the film. Free of GPUI; the window
//! calls it from key actions and once per rendered frame.

use std::mem;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};

use takewise_core::takes::FULL_SCALE;
use takewise_core::timing::film_shift_s;
use takewise_core::{
    Dropout, PartialTake, Project, RECORD_RATE, RecordPlan, RecordedInput, Snapshot, TakeRef,
    TakeState, Timebase, WavLock, begin_take, commit_take, finish_interrupted_commit,
    line_offset_s, plan_take, sample_film_time, update_take_meta, write_take_wav,
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
/// A film shift larger than this after the take started is recorded.
const FILM_SHIFT_TOLERANCE_MS: f64 = 5.0;

/// What a tick or a stop produced, for the window to show.
#[derive(Debug, Default)]
pub struct Update {
    pub committed: Option<TakeRef>,
    pub message: Option<String>,
}

pub struct Recorder {
    phase: Phase,
    warmup: Warmup,
    /// Where the film rests while nothing plays.
    park_s: f64,
    /// Peak of the last captured buffer in dBFS, and when it arrived.
    level: Option<(f64, f64)>,
    /// The idle capture that drives the level meter (never played back).
    meter: Option<Receiver<CaptureBuffer>>,
    meter_retry_at: f64,
    /// Takes whose audio could not be saved yet; kept in memory and retried.
    unsaved: Vec<Unsaved>,
    unsaved_retry_at: f64,
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
}

struct Playback {
    film_from_s: f64,
    hold_s: f64,
    stop_film_s: f64,
    /// Film time of the take's first sample.
    take_first_film_s: f64,
    film_gain_db: f64,
    opened_host_s: f64,
    /// The film's scheduled timebase once both players started.
    started: Option<Timebase>,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            phase: Phase::Idle,
            warmup: Warmup::Pending,
            park_s: 0.0,
            level: None,
            meter: None,
            meter_retry_at: 0.0,
            unsaved: Vec::new(),
            unsaved_retry_at: 0.0,
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

    /// Whether stopped takes still wait to be saved.
    pub fn has_unsaved(&self) -> bool {
        !self.unsaved.is_empty()
    }

    pub fn is_recording(&self) -> bool {
        matches!(self.phase, Phase::Recording(_))
    }

    pub fn is_playing(&self) -> bool {
        matches!(self.phase, Phase::Playing(_))
    }

    /// The line being recorded.
    pub fn recording_line(&self) -> Option<&str> {
        match &self.phase {
            Phase::Recording(take) => Some(&take.line),
            _ => None,
        }
    }

    /// The line as the take being recorded started with it.
    pub fn recording_snapshot(&self) -> Option<&Snapshot> {
        match &self.phase {
            Phase::Recording(take) => Some(&take.snapshot),
            _ => None,
        }
    }

    /// The input level in dBFS, while buffers arrive.
    pub fn level_dbfs(&self, media: &dyn Media) -> Option<f64> {
        let (db, at) = match &self.phase {
            Phase::Recording(take) => take.writer.progress().level?,
            _ => self.level?,
        };
        (media.host_now_s() - at < LEVEL_HOLD_S).then_some(db)
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

    /// Whole seconds left before the line starts, during the count-in.
    pub fn count_in(&self, media: &dyn Media) -> Option<u32> {
        let Phase::Recording(take) = &self.phase else {
            return None;
        };
        let left = take.line_start_host_s? - media.host_now_s();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        (left > 0.0).then(|| left.ceil() as u32)
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
                if self.meter.is_some() && media.capture_error().is_some() {
                    self.meter = None;
                    media.stop_capture();
                    self.meter_retry_at = now + METER_RETRY_S;
                }
                if self.meter.is_none() && now >= self.meter_retry_at {
                    let (tx, rx) = mpsc::channel();
                    match media.start_capture(device, channel, tx) {
                        Ok(()) => self.meter = Some(rx),
                        Err(_) => self.meter_retry_at = now + METER_RETRY_S,
                    }
                }
            }
        }
    }

    /// Starts a take of line `idx`: capture starts now; once its first
    /// buffer arrives, the film is scheduled to play from the pre-roll
    /// (holding its first frame when the line starts within the pre-roll),
    /// with the count-in.
    pub fn start_take(
        &mut self,
        media: &mut dyn Media,
        project: &Project,
        idx: usize,
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
        self.end_warmup(media);
        self.stop_meter(media);

        let rec = &project.config.record;
        let plan = plan_take(line.start, snapshot.end, rec.preroll_s, rec.tail_s);
        let (tx, rx) = mpsc::channel();
        if let Err(err) = media.start_capture(&device, input.channel, tx) {
            media.film_seek(self.park_s);
            return Err(format!("Cannot record from {device:?}: {err}"));
        }
        media.film_set_muted(false);
        media.film_set_gain_db(rec.film_gain_db);
        self.level = None;
        self.phase = Phase::Recording(Box::new(Take {
            line: line.id.clone(),
            line_start: line.start,
            snapshot,
            device,
            channel: input.channel,
            latency_s,
            offset_ms: f64::from(input.latency_offset_ms),
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
        }));
        Ok(())
    }

    /// Stops the take and commits it. A take without any audio is dropped
    /// (nothing was begun on disk). When the take cannot be saved, it is
    /// kept in memory and saving is retried; the error says so.
    pub fn finish_take(&mut self, media: &mut dyn Media, dir: &Path) -> Update {
        let Phase::Recording(take) = mem::replace(&mut self.phase, Phase::Idle) else {
            return Update::default();
        };
        media.stop_capture();
        let Take {
            line,
            line_start,
            snapshot,
            device,
            channel,
            latency_s,
            offset_ms,
            clock,
            writer,
            partial,
            film_ended_host_s,
            ..
        } = *take;
        let written = writer.finish();
        let unplayed_s = clock.unplayed_lateness_s(media, media.host_now_s());
        media.film_pause();
        media.film_seek(self.park_s);
        self.level = None;
        if written.samples.is_empty() {
            return Update::default();
        }
        if !clock.started() {
            return Update {
                committed: None,
                message: Some("Stopped before the film started; nothing was recorded.".to_owned()),
            };
        }
        // Placed with the film's timebase as read back (a late start moves
        // the take), by the same mapping the calibration measures.
        let first_film_s = written.first_host_s.and_then(|host| {
            sample_film_time(clock.history(), host, 0, RECORD_RATE, latency_s, offset_ms)
        });
        // Measured strictly before the film's end, so the player halting
        // there is no shift.
        let film_shift_ms = written.first_host_s.and_then(|host| {
            let from = host - latency_s;
            let to = (from + written.samples.len() as f64 / f64::from(RECORD_RATE))
                .min(film_ended_host_s.unwrap_or(f64::INFINITY));
            // A film that never played leaves the take on its schedule,
            // off by how late the film was when the take stopped.
            let ms = unplayed_s.unwrap_or_else(|| film_shift_s(clock.history(), from, to)) * 1000.0;
            (ms > FILM_SHIFT_TOLERANCE_MS).then_some(ms)
        });
        let dropouts = dropout_note(&written.dropouts, first_film_s);
        let shift_note = film_shift_ms.map(|ms| {
            format!("The film stalled during the take; placement may be off by {ms:.0} ms.")
        });
        // The take keeps its film position: placed against the line's slot
        // as it is now, which M or an agent may have moved during the take.
        // The text stays the one read during the take, so an edit made
        // meanwhile shows the take as recorded against old text.
        let snapshot = match Project::open(dir).ok().and_then(|p| p.snapshot(&line)) {
            Some(now) => Snapshot {
                text: snapshot.text,
                ..now
            },
            None => snapshot,
        };
        let first_film_s = first_film_s.unwrap_or(line_start);
        #[allow(clippy::cast_possible_truncation)]
        let input = RecordedInput {
            sample_rate: RECORD_RATE,
            device,
            channel,
            line_offset_s: line_offset_s(first_film_s, snapshot.start),
            snapshot,
            dropouts: Some(written.dropouts.len().min(u32::MAX as usize) as u32),
            film_shift_ms,
        };
        let Written {
            samples,
            complete,
            lock,
            ..
        } = written;
        let unsaved = Unsaved {
            line,
            input,
            samples,
            partial,
            lock,
            complete,
        };
        let mut update = self.save(dir, unsaved);
        update.message = join(join(update.message, dropouts), shift_note);
        update
    }

    /// Writes and commits a stopped take, or keeps it to retry.
    fn save(&mut self, dir: &Path, mut unsaved: Unsaved) -> Update {
        match save(dir, &mut unsaved) {
            Ok(committed) => Update {
                committed: Some(committed),
                message: None,
            },
            Err(err) => {
                let message = format!(
                    "The take of {} could not be saved ({err}); it is kept in memory and saving is retried.",
                    unsaved.line
                );
                self.unsaved.push(unsaved);
                Update {
                    committed: None,
                    message: Some(message),
                }
            }
        }
    }

    /// Tries once more to save the takes that could not be saved (when the
    /// window closes).
    pub fn save_unsaved_now(&mut self, dir: &Path) -> Update {
        self.unsaved_retry_at = f64::NEG_INFINITY;
        self.retry_unsaved(dir, f64::NEG_INFINITY)
    }

    /// Retries saving the takes that could not be saved; the first one
    /// saved is reported.
    fn retry_unsaved(&mut self, dir: &Path, now: f64) -> Update {
        if self.unsaved.is_empty() || now < self.unsaved_retry_at {
            return Update::default();
        }
        self.unsaved_retry_at = now + SAVE_RETRY_S;
        let mut update = Update::default();
        let mut left = Vec::new();
        for mut unsaved in mem::take(&mut self.unsaved) {
            match save(dir, &mut unsaved) {
                Ok(committed) if update.committed.is_none() => {
                    update.message = Some(format!(
                        "Saved the earlier take of {} as take {}.",
                        committed.line, committed.take
                    ));
                    update.committed = Some(committed);
                }
                Ok(_) => {}
                Err(_) => left.push(unsaved),
            }
        }
        self.unsaved = left;
        update
    }

    /// Plays line `idx` in context: pre-roll, the take locked to the film,
    /// and the tail. Starts once the take has loaded.
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
        self.phase = Phase::Playing(Playback {
            film_from_s: plan.film_from_s,
            hold_s: plan.hold_first_frame_s,
            stop_film_s: plan.capture_stop_film_s,
            take_first_film_s: line.start + take.line_offset_s,
            film_gain_db: rec.film_gain_db,
            opened_host_s: media.host_now_s(),
            started: None,
        });
        Ok(())
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
        if let Some(meter) = &self.meter {
            while let Ok(buffer) = meter.try_recv() {
                if !buffer.samples.is_empty() {
                    self.level = Some((peak_dbfs(&buffer.samples), now));
                }
            }
        }
        match &mut self.phase {
            Phase::Idle => self.retry_unsaved(dir, now),
            Phase::Recording(take) => {
                let progress = take.writer.progress();
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
                            Err(err) => failure = Some(format!("the film cannot play: {err}")),
                        }
                    } else if now - take.capture_started_host_s > FIRST_BUFFER_TIMEOUT_S {
                        failure = Some(format!(
                            "No audio arrived from {:?}; check the input device.",
                            take.device
                        ));
                    }
                }
                take.clock.track(media);
                let mut message = begin_on_disk(take, dir, &progress);
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
                let finished = self.finish_take(media, dir);
                Update {
                    committed: finished.committed,
                    message: match failure {
                        Some(failure) => join(
                            Some(format!("Recording stopped: {failure}")),
                            finished.message,
                        ),
                        None => finished.message,
                    },
                }
            }
            Phase::Playing(playback) => match playback.started {
                Some(tb) if tb.film_at(now) >= playback.stop_film_s => {
                    self.stop_playback(media);
                    Update {
                        ..Update::default()
                    }
                }
                None if media.take_ready() && media.film_ready() => {
                    match start_both(media, playback, now) {
                        Ok(tb) => {
                            playback.started = Some(tb);
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
    let take_host = tb.host_anchor_s + (playback.take_first_film_s - tb.film_anchor_s);
    media.take_play_at(begin - take_host, begin)?;
    Ok(tb)
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

/// Writes a stopped take's metadata and, when the partial WAV is not
/// complete, its audio from memory, then commits it.
fn save(dir: &Path, unsaved: &mut Unsaved) -> Result<TakeRef, String> {
    let partial = if let Some(partial) = &unsaved.partial {
        partial.clone()
    } else {
        let partial =
            begin_take(dir, &unsaved.line, &unsaved.input).map_err(|err| err.to_string())?;
        unsaved.partial = Some(partial.clone());
        partial
    };
    // An earlier commit renamed the take's files but failed before logging
    // it: finish that commit (the take's lock is still held).
    if !partial.meta.exists()
        && let Some(committed) =
            finish_interrupted_commit(dir, &partial).map_err(|err| err.to_string())?
    {
        return Ok(committed);
    }
    if !unsaved.complete {
        // Rewritten only while holding the file's lock (kept from the
        // recording, else taken now), so a repair never adopts it meanwhile.
        if unsaved.lock.is_none() {
            unsaved.lock = Some(WavLock::acquire(&partial.wav).map_err(|err| err.to_string())?);
        }
        write_take_wav(&partial.wav, &unsaved.samples, RECORD_RATE)
            .map_err(|err| err.to_string())?;
        unsaved.complete = true;
    }
    update_take_meta(&partial, &unsaved.input).map_err(|err| err.to_string())?;
    #[cfg(test)]
    if COMMITS_FAIL.get() {
        return Err("the commit failed (test)".to_owned());
    }
    commit_take(dir, partial).map_err(|err| err.to_string())
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
