//! The recording session: takes, play in context, the input meter and the
//! timing that places captured samples on the film. Free of GPUI; the window
//! calls it from key actions and once per rendered frame.

use std::mem;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};

use takewise_core::takes::FULL_SCALE;
use takewise_core::{
    Dropout, PartialTake, Project, RECORD_RATE, RecordedInput, Snapshot, TakeRef, TakeState,
    Timebase, TimebaseHistory, WavLock, begin_take, commit_take, line_offset_s, plan_take,
    sample_film_time, update_take_meta, write_take_wav,
};
use takewise_media::CaptureBuffer;

use super::media::Media;
use super::writer::{Progress, TakeWriter, Written};

/// How far ahead a start is scheduled, so the players are set up before it.
pub const START_LEAD_S: f64 = 0.15;
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
    /// The partial WAV's lock, held until the take is committed.
    _lock: Option<WavLock>,
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
    history: TimebaseHistory,
    /// Receives the capture and keeps and writes the audio.
    writer: TakeWriter,
    /// The take on disk: begun once the first buffer places the take.
    partial: Option<PartialTake>,
    /// Film time of the first sample, once known.
    first_film_s: Option<f64>,
    /// Why the take could not be begun on disk (it is kept in memory).
    begin_error: Option<String>,
    /// Whether the writer's error was shown.
    write_error_shown: bool,
    /// Host time at which the film reaches the line's start.
    line_start_host_s: f64,
    stop_film_s: f64,
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
            Phase::Recording(take) => take.history.film_at(now),
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
        let left = take.line_start_host_s - media.host_now_s();
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

    /// Starts a take of line `idx`: the film plays from the pre-roll (holding
    /// its first frame when the line starts within the pre-roll) and capture
    /// starts now, with the count-in.
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
        let now = media.host_now_s();
        let begin = now + START_LEAD_S;
        media.film_set_muted(false);
        media.film_set_gain_db(rec.film_gain_db);
        let tb = media.film_play_at(plan.film_from_s, begin + plan.hold_first_frame_s)?;
        let mut history = TimebaseHistory::new();
        // The scheduled mapping holds for every sample of this take, also
        // for those whose latency-corrected time lies just before this call.
        history.record(now - 1.0, tb);

        let (tx, rx) = mpsc::channel();
        if let Err(err) = media.start_capture(&device, input.channel, tx) {
            media.film_pause();
            media.film_seek(self.park_s);
            return Err(format!("Cannot record from {device:?}: {err}"));
        }
        self.level = None;
        self.phase = Phase::Recording(Box::new(Take {
            line: line.id.clone(),
            line_start: line.start,
            snapshot,
            device,
            channel: input.channel,
            latency_s,
            offset_ms: f64::from(input.latency_offset_ms),
            history,
            writer: TakeWriter::spawn(rx),
            partial: None,
            first_film_s: None,
            begin_error: None,
            write_error_shown: false,
            line_start_host_s: tb.host_anchor_s + (line.start - tb.film_anchor_s),
            stop_film_s: plan.capture_stop_film_s,
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
            history,
            writer,
            partial,
            first_film_s,
            ..
        } = *take;
        let written = writer.finish();
        media.film_pause();
        media.film_seek(self.park_s);
        self.level = None;
        if written.samples.is_empty() {
            return Update::default();
        }
        let first_film_s = first_film_s.or_else(|| {
            let host = written.first_host_s?;
            sample_film_time(&history, host, 0, RECORD_RATE, latency_s, offset_ms)
        });
        let dropouts = dropout_note(&written.dropouts, first_film_s);
        #[allow(clippy::cast_possible_truncation)]
        let input = RecordedInput {
            sample_rate: RECORD_RATE,
            device,
            channel,
            line_offset_s: line_offset_s(first_film_s.unwrap_or(line_start), line_start),
            snapshot,
            dropouts: Some(written.dropouts.len().min(u32::MAX as usize) as u32),
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
            _lock: lock,
            complete,
        };
        let mut update = self.save(dir, unsaved);
        update.message = join(update.message, dropouts);
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
                let failure = media.capture_error();
                let at_end = take
                    .history
                    .film_at(now)
                    .is_some_and(|film| film >= take.stop_film_s);
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
        &take.history,
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
    if take.partial.is_some() || take.begin_error.is_some() {
        return None;
    }
    let first = first_sample_film_s(take, progress.first_host_s?).unwrap_or(take.line_start);
    take.first_film_s = Some(first);
    let input = RecordedInput {
        sample_rate: RECORD_RATE,
        device: take.device.clone(),
        channel: take.channel,
        line_offset_s: line_offset_s(first, take.line_start),
        snapshot: take.snapshot.clone(),
        dropouts: None,
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
    if !unsaved.complete {
        write_take_wav(&partial.wav, &unsaved.samples, RECORD_RATE)
            .map_err(|err| err.to_string())?;
        unsaved.complete = true;
    }
    update_take_meta(&partial, &unsaved.input).map_err(|err| err.to_string())?;
    commit_take(dir, partial).map_err(|err| err.to_string())
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
