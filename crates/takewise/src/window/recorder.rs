//! The recording session: takes, play in context, the input meter and the
//! timing that places captured samples on the film. Free of GPUI; the window
//! calls it from key actions and once per rendered frame.

use std::mem;
use std::path::Path;
use std::sync::mpsc::{self, Receiver};

use takewise_core::takes::FULL_SCALE;
use takewise_core::{
    PartialTake, Project, RECORD_RATE, RecordedInput, TakeRef, TakeState, Timebase,
    TimebaseHistory, begin_take, commit_take, line_offset_s, plan_take, sample_film_time,
    write_take_wav,
};
use takewise_media::CaptureBuffer;

use super::media::Media;

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
    /// The take's metadata; `line_offset_s` is set from the first buffer.
    input: RecordedInput,
    latency_s: f64,
    offset_ms: f64,
    history: TimebaseHistory,
    rx: Receiver<CaptureBuffer>,
    samples: Vec<i32>,
    /// Begun when the first buffer arrives.
    partial: Option<PartialTake>,
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
        }
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
        let (db, at) = self.level?;
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
            input: RecordedInput {
                sample_rate: RECORD_RATE,
                device,
                channel: input.channel,
                line_offset_s: 0.0,
                snapshot,
                dropouts: None,
            },
            latency_s,
            offset_ms: f64::from(input.latency_offset_ms),
            history,
            rx,
            samples: Vec::new(),
            partial: None,
            line_start_host_s: tb.host_anchor_s + (line.start - tb.film_anchor_s),
            stop_film_s: plan.capture_stop_film_s,
        }));
        Ok(())
    }

    /// Stops the take, writes its audio and commits it. A take without any
    /// audio is dropped (nothing was begun on disk).
    pub fn finish_take(
        &mut self,
        media: &mut dyn Media,
        dir: &Path,
    ) -> Result<Option<TakeRef>, String> {
        let Phase::Recording(mut take) = mem::replace(&mut self.phase, Phase::Idle) else {
            return Ok(None);
        };
        media.stop_capture();
        let drained = drain(&mut take, dir, &mut self.level);
        media.film_pause();
        media.film_seek(self.park_s);
        self.level = None;
        let Some(partial) = take.partial else {
            return drained.map(|()| None);
        };
        write_take_wav(&partial.wav, &take.samples, RECORD_RATE).map_err(|err| {
            format!(
                "The take's audio could not be written ({err}); it stays in {}",
                partial.wav.display()
            )
        })?;
        commit_take(dir, partial)
            .map(Some)
            .map_err(|err| format!("The take could not be committed: {err}"))
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
            Phase::Idle => Update::default(),
            Phase::Recording(take) => {
                let failure = drain(take, dir, &mut self.level)
                    .err()
                    .or_else(|| media.capture_error());
                let at_end = take
                    .history
                    .film_at(now)
                    .is_some_and(|film| film >= take.stop_film_s);
                if failure.is_none() && !at_end {
                    return Update::default();
                }
                let finished = self.finish_take(media, dir);
                let (committed, error) = match finished {
                    Ok(committed) => (committed, None),
                    Err(err) => (None, Some(err)),
                };
                Update {
                    committed,
                    message: match (failure, error) {
                        (Some(failure), _) => Some(format!("Recording stopped: {failure}")),
                        (None, error) => error,
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

/// Moves captured buffers into the take, beginning it on disk with the
/// first buffer, whose film time gives the take's `line_offset_s`.
fn drain(take: &mut Take, dir: &Path, level: &mut Option<(f64, f64)>) -> Result<(), String> {
    while let Ok(buffer) = take.rx.try_recv() {
        if buffer.samples.is_empty() {
            continue;
        }
        *level = Some((peak_dbfs(&buffer.samples), buffer.host_time_s));
        if take.partial.is_none() {
            let first = sample_film_time(
                &take.history,
                buffer.host_time_s,
                0,
                RECORD_RATE,
                take.latency_s,
                take.offset_ms,
            )
            .unwrap_or(take.line_start);
            take.input.line_offset_s = line_offset_s(first, take.line_start);
            let partial = begin_take(dir, &take.line, &take.input)
                .map_err(|err| format!("cannot start the take on disk: {err}"))?;
            take.partial = Some(partial);
        }
        take.samples.extend_from_slice(&buffer.samples);
    }
    Ok(())
}

/// Peak of `samples` in dBFS (24-bit full scale).
pub fn peak_dbfs(samples: &[i32]) -> f64 {
    let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    if peak == 0 {
        return f64::NEG_INFINITY;
    }
    20.0 * (f64::from(peak) / f64::from(FULL_SCALE)).log10()
}
