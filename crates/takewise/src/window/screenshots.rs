//! `takewise screenshots <work> <out>` (cargo feature `screenshots`): the
//! README's screenshots, made reproducibly; `mise run screenshots` fetches
//! the film and runs it.
//!
//! It builds a demo project in `<work>/earth.takewise` on `<work>/film.mp4`
//! with a script of its own, speaks its takes with macOS `say` and checks
//! them with the real checks (whisper-cli and the speech model must be
//! installed). Then it opens recording windows on the project, hidden, with
//! [`DemoMedia`] in place of the real media: the film's frames come from
//! AVPlayer, which is only ever seeked, never played; the host clock is
//! the harness's; the "microphone" is fed from the spoken files. Each window
//! is put into the state to show through its own methods and rendered
//! offscreen to `<out>/*.png`; the short loop `<out>/loop.webp` is a series
//! of such renders of two takes recorded and checked in a window, with
//! close-ups of their verdicts, put together with `img2webp`; the
//! repository's social preview `<out>/social-preview.png` sets a render
//! beside the app's icon and name. Last, the README's agent example runs
//! for real on a copy of the project (`<work>/cli/transcript.txt`), and its
//! export is drawn as `<out>/export.png`. Nothing plays, no microphone opens, no window
//! appears and no key or mouse event is sent. `scripts/screenshots`
//! compresses the PNGs afterwards.

use std::cell::RefCell;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Bounds, Entity, WindowBounds, WindowOptions,
    point, px, size,
};
use takewise_core::{self as tw, AvailableInput, Project, RECORD_RATE, TakeRef, Timebase};
use takewise_media::{
    CaptureBuffer, DeviceInfo, FilmPlayer, Frame, Monitor, OutputSource, Transport,
};

use super::checks::{Checker, SharedTranscriber, worker_tag};
use image::RgbaImage;

use super::{Media, RecordingWindow, bind_keys, load};

/// The demo script: a short documentary over NASA's public-domain "Earth
/// Views from the International Space Station", cut to 60 s from 0:16.
const SCRIPT: &str = r#"# Earth from orbit: the demo script of Takewise's screenshots.
# Film: NASA, "Earth Views from the International Space Station" (public domain).

[[line]]
id = "orbit"
start = 1.0
end = 5.5
text = "High above the clouds, the station circles the Earth again and again."

[[line]]
id = "coast"
start = 12.5
end = 15.5
text = "Clouds pile up where the ocean meets the land."

[[line]]
id = "capsule"
start = 25.0
end = 29.1
text = "A capsule waits at its port, ready to bring the crew home."
note = "Calm. Let it land on “home”."

[[line]]
id = "storm"
start = 32.5
end = 36.0
text = "From up here, a storm is something you look down on."

[[line]]
id = "ocean"
start = 41.0
end = 51.5
text = "Below, the open ocean, and rows of cloud that run for hundreds of miles. The wind lines them up like waves on a beach, one behind the other, all the way to the edge of the sky."

[[line]]
id = "desert"
start = 52.5
end = 57.5
text = "Then the desert, dry and red, and the next sunrise only minutes away."
"#;

/// The takes, in order: the line, what is spoken. `[[slnc 1000]]` is a
/// pause of one second in `say`.
const TAKES: &[(&str, &str)] = &[
    (
        "orbit",
        "High above the clouds, the station circles the Earth again and again.",
    ),
    ("coast", "Clouds pile up where the ocean meets the land."),
    // A wrong word: "dock" for "port".
    (
        "capsule",
        "A capsule waits at its dock, ready to bring the crew home.",
    ),
    // Too slow: a pause runs the line past its slot.
    (
        "capsule",
        "A capsule waits at its port, [[slnc 1100]] ready to bring the crew home.",
    ),
    // A word left out: "home".
    (
        "capsule",
        "A capsule waits at its port, ready to bring the crew.",
    ),
    (
        "capsule",
        "A capsule waits at its port, ready to bring the crew home.",
    ),
    (
        "storm",
        "From up here, a storm is something you look down on.",
    ),
    // Read a little faster than the others, to fit its slot with room.
    (
        "ocean",
        "[[rate 190]] Below, the open ocean, and rows of cloud that run for hundreds of miles. \
         The wind lines them up like waves on a beach, one behind the other, \
         all the way to the edge of the sky.",
    ),
    (
        "desert",
        "Then the desert, dry and red, and the next sunrise only minutes away.",
    ),
];

/// The voice of the takes, and how fast it reads (words per minute).
const VOICE: &str = "Samantha";
const RATE: &str = "170";
/// The demo's devices.
const INPUT: &str = "USB Audio Interface";
const SPEAKERS: &str = "MacBook Pro Speakers";
const LATENCY_S: f64 = 0.004;
/// Speech starts this long after the line's start in every take.
const LEAD_S: f64 = 0.25;
/// Speech peaks here, and the room's noise sits here (RMS).
const SPEECH_PEAK_DBFS: f64 = -9.0;
const ROOM_DBFS: f64 = -66.0;
const FULL_SCALE: f64 = 8_388_607.0;
/// The window's size, as Takewise opens it: the audio panel and the setup
/// check are cut from a window of this size.
const WINDOW: (f32, f32) = (1280.0, 820.0);
/// A smaller window for the hero and the loop, so their text stays legible
/// in a README's column.
const HERO: (f32, f32) = (1000.0, 640.0);
/// The takes of the loop: the indices in [`TAKES`] of the capsule line
/// without "home", and read right.
const LOOP_TAKES: [usize; 2] = [4, 5];
/// The loop's width in pixels.
const LOOP_WIDTH: u32 = 1000;
/// The loop's close-up of the takes and their verdicts, in logical pixels
/// of a [`HERO`] window: the side column's list of takes, which begins
/// halfway down the room the levels leave above the key hints (two rows
/// of them at this width).
const CLOSE_UP: (f32, f32, f32, f32) = (700.0, 245.5, 300.0, 128.0);

/// Runs `takewise screenshots <work> <out>`.
pub fn main(args: &[OsString]) -> ExitCode {
    let [work, out] = args else {
        eprintln!("usage: takewise screenshots <work-dir> <out-dir>");
        return ExitCode::from(2);
    };
    let (work, out) = (PathBuf::from(work), PathBuf::from(out));
    let demo = match prepare(&work) {
        Ok(demo) => demo,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(err) = std::fs::create_dir_all(&out) {
        eprintln!("error: {}: {err}", out.display());
        return ExitCode::FAILURE;
    }
    let ok = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&ok);
    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        bind_keys(cx);
        cx.spawn(async move |cx| {
            match shoot(&demo, &work, &out, cx).await {
                Ok(()) => done.store(true, Ordering::SeqCst),
                Err(err) => eprintln!("error: {err}"),
            }
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
    if ok.load(Ordering::SeqCst) {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The demo project and what the harness feeds its windows.
struct Demo {
    dir: PathBuf,
    /// The project of the loop: the demo before the capsule line's takes.
    anim: PathBuf,
    /// What is spoken in each of [`TAKES`], in order.
    spoken: Vec<Vec<i32>>,
    /// The spoken text of each line's last good take, by line id, and the
    /// setup check's sentence.
    voices: Vec<(String, Vec<i32>)>,
    sentence: Vec<i32>,
}

impl Demo {
    fn voice(&self, line: &str) -> Vec<i32> {
        self.voices
            .iter()
            .rev()
            .find(|(id, _)| id == line)
            .map(|(_, samples)| samples.clone())
            .unwrap_or_default()
    }
}

/// Builds the demo project afresh in `work` and checks its takes.
fn prepare(work: &Path) -> Result<Demo, String> {
    let film = std::fs::canonicalize(work.join("film.mp4")).map_err(|err| {
        format!(
            "{}: {err}; run `mise run screenshots`",
            work.join("film.mp4").display()
        )
    })?;
    let dir = work.join("earth.takewise");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    }
    let voices_dir = work.join("voices");
    std::fs::create_dir_all(&voices_dir)
        .map_err(|err| format!("{}: {err}", voices_dir.display()))?;

    let info = crate::film::probe(&film).map_err(|err| err.to_string())?;
    let project = Project::init(&dir, &film, info).map_err(|err| err.to_string())?;
    let fail = |err: tw::Error| err.to_string();
    tw::set_export(
        &dir,
        &tw::ExportConfig {
            processing: tw::Processing::Clean,
            ..tw::ExportConfig::default()
        },
    )
    .map_err(fail)?;
    tw::set_route(&dir, Some((INPUT, 1)), Some(INPUT)).map_err(fail)?;
    tw::set_calibration(
        &dir,
        -6,
        &tw::CalibrationInfo {
            input: INPUT.to_owned(),
            output: INPUT.to_owned(),
            buffer_frames: Some(256),
            date: jiff::ZonedDateTime::now().date().to_string(),
            from: None,
        },
    )
    .map_err(fail)?;
    std::fs::write(dir.join("script.toml"), SCRIPT).map_err(|err| err.to_string())?;
    let preroll_s = project.config.record.preroll_s;

    let anim = work.join("anim.takewise");
    let mut voices = Vec::new();
    let mut spoken = Vec::new();
    for (n, (line, text)) in TAKES.iter().enumerate() {
        if *line == "capsule" && !TAKES[..n].iter().any(|(l, _)| *l == "capsule") {
            // The loop starts before the capsule line has takes.
            copy_dir(&dir, &anim)?;
        }
        let wav = voices_dir.join(format!("{n:02}-{line}.wav"));
        let speech = speak(text, &wav)?;
        spoken.push(speech.clone());
        let samples = take_samples(&speech, preroll_s, u32::try_from(n).unwrap_or(0));
        commit(&dir, line, &samples, preroll_s)?;
        voices.push(((*line).to_owned(), speech));
    }
    let sentence = speak(super::setup::SENTENCE, &voices_dir.join("sentence.wav"))?;

    for project in [&dir, &anim] {
        check_all(project)?;
        tw::choose_take(project, "orbit", 1, "narrator").map_err(fail)?;
        tw::choose_take(project, "coast", 1, "narrator").map_err(fail)?;
    }
    // The loop's project was copied before these lines had takes.
    for line in ["ocean", "desert"] {
        tw::choose_take(&dir, line, 1, "narrator").map_err(fail)?;
    }
    Ok(Demo {
        dir,
        anim,
        spoken,
        voices,
        sentence,
    })
}

/// Speaks `text` with `say` into `wav` and returns it at 48 kHz, its peak
/// at [`SPEECH_PEAK_DBFS`].
fn speak(text: &str, wav: &Path) -> Result<Vec<i32>, String> {
    let status = Command::new("say")
        .args(["-v", VOICE, "-r", RATE, "--data-format=LEI24@48000", "-o"])
        .arg(wav)
        .arg(text)
        .status()
        .map_err(|err| format!("cannot run say: {err}"))?;
    if !status.success() {
        return Err(format!("say failed for {text:?}"));
    }
    let (samples, rate) = tw::read_take_wav(wav).map_err(|err| err.to_string())?;
    if rate != RECORD_RATE {
        return Err(format!("say wrote {rate} Hz, not 48 kHz"));
    }
    let peak = samples
        .iter()
        .map(|s| s.unsigned_abs())
        .max()
        .unwrap_or(1)
        .max(1);
    let gain = FULL_SCALE * 10f64.powf(SPEECH_PEAK_DBFS / 20.0) / f64::from(peak);
    #[allow(clippy::cast_possible_truncation)]
    Ok(samples
        .iter()
        .map(|&s| (f64::from(s) * gain).round() as i32)
        .collect())
}

/// A take as recorded: the room before the line (the pre-roll and the
/// lead-in), the speech, and the room after it, all over the room's noise.
fn take_samples(speech: &[i32], preroll_s: f64, seed: u32) -> Vec<i32> {
    let lead = seconds(preroll_s + LEAD_S);
    let tail = seconds(0.8);
    let mut room = Room::new(seed + 1);
    let mut samples: Vec<i32> = (0..lead).map(|_| room.next()).collect();
    samples.extend(speech.iter().map(|&s| s + room.next()));
    samples.extend((0..tail).map(|_| room.next()));
    samples
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn seconds(s: f64) -> usize {
    (s * f64::from(RECORD_RATE)).round() as usize
}

/// Adds a take of `line` whose audio is `samples`, placed so the line
/// starts `preroll_s` into it, as the window commits one.
fn commit(dir: &Path, line: &str, samples: &[i32], preroll_s: f64) -> Result<TakeRef, String> {
    let project = Project::open(dir).map_err(|err| err.to_string())?;
    let snapshot = project
        .snapshot(line)
        .ok_or_else(|| format!("no line {line}"))?;
    let input = tw::RecordedInput {
        sample_rate: RECORD_RATE,
        device: INPUT.to_owned(),
        channel: 1,
        line_offset_s: tw::line_offset_s(snapshot.start - preroll_s, snapshot.start),
        snapshot,
        dropouts: Some(0),
        film_shift_ms: None,
        passage: None,
    };
    let partial = tw::begin_take(dir, line, &input).map_err(|err| err.to_string())?;
    tw::write_take_wav(&partial.wav, samples, RECORD_RATE).map_err(|err| err.to_string())?;
    tw::commit_take(dir, partial).map_err(|err| err.to_string())
}

/// Checks every take with whisper-cli, as `takewise check` does.
fn check_all(dir: &Path) -> Result<(), String> {
    let project = Project::open(dir).map_err(|err| err.to_string())?;
    let whisper = tw::WhisperCli::discover(&project.config.check)
        .map_err(|err| format!("the screenshots need the word checks: {err}"))?;
    let ledger = tw::read(dir).map_err(|err| err.to_string())?;
    for unchecked in tw::unchecked_takes(&project, &ledger) {
        let checked = tw::check_take(dir, &unchecked.take, &whisper, env!("CARGO_PKG_VERSION"))
            .map_err(|err| err.to_string())?;
        let reason = checked
            .reasons
            .first()
            .map_or(String::new(), |r| format!(": {}", r.message));
        eprintln!(
            "{} take {}: {:?}{reason}",
            unchecked.take.line, unchecked.take.take, checked.verdict
        );
    }
    Ok(())
}

/// The room's noise: quiet, deterministic white noise at [`ROOM_DBFS`].
struct Room(u32);

impl Room {
    fn new(seed: u32) -> Self {
        Self(seed.wrapping_mul(2_654_435_761).max(1))
    }

    fn next(&mut self) -> i32 {
        // xorshift32: uniform in [-1, 1), RMS 1/√3.
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        let uniform = f64::from(self.0) / f64::from(u32::MAX) * 2.0 - 1.0;
        let rms = FULL_SCALE * 10f64.powf(ROOM_DBFS / 20.0);
        #[allow(clippy::cast_possible_truncation)]
        let sample = (uniform * rms * 3f64.sqrt()).round() as i32;
        sample
    }
}

/// The media of the screenshots' windows; see the module documentation.
struct DemoState {
    now: f64,
    film: FilmPlayer,
    /// The film time last seeked to, for the frame on screen.
    shown_s: Option<f64>,
    park_s: f64,
    /// The last scheduled play (`film_s`, `host_s`) until paused.
    playing: Option<(f64, f64)>,
    sink: Option<Sender<CaptureBuffer>>,
    /// Host time up to which the capture has been fed.
    fed_until: f64,
    /// What the "microphone" hears next, then the room.
    source: Vec<i32>,
    position: usize,
    room: Room,
    channel_meter: bool,
    levels: Vec<f32>,
}

#[derive(Clone)]
struct DemoMedia(Rc<RefCell<DemoState>>);

impl DemoMedia {
    fn open(film: &Path) -> Result<Self, String> {
        let film = FilmPlayer::open(film).map_err(|err| err.to_string())?;
        film.set_muted(true);
        Ok(Self(Rc::new(RefCell::new(DemoState {
            now: 1000.0,
            film,
            shown_s: None,
            park_s: 0.0,
            playing: None,
            sink: None,
            fed_until: 1000.0,
            source: Vec::new(),
            position: 0,
            room: Room::new(7),
            channel_meter: false,
            levels: vec![-15.0, f32::NEG_INFINITY],
        }))))
    }

    fn now(&self) -> f64 {
        self.0.borrow().now
    }

    /// The microphone hears `samples` from now on, then the room.
    fn hear(&self, samples: Vec<i32>) {
        let mut state = self.0.borrow_mut();
        state.source = samples;
        state.position = 0;
    }

    /// Moves the clock to `to`, feeding the capture what it heard until
    /// then in 10 ms buffers.
    fn advance(&self, to: f64) {
        const STEP: f64 = 0.01;
        let mut state = self.0.borrow_mut();
        let len = seconds(STEP);
        while state.fed_until + STEP <= to {
            let mut samples = Vec::with_capacity(len);
            for _ in 0..len {
                let room = state.room.next();
                let voice = state.source.get(state.position).copied().unwrap_or(0);
                state.position += 1;
                samples.push(voice + room);
            }
            let host_time_s = state.fed_until + LATENCY_S;
            if let Some(sink) = &state.sink {
                let _ = sink.send(CaptureBuffer {
                    host_time_s,
                    samples,
                });
            }
            state.fed_until += STEP;
        }
        state.now = to;
    }
}

impl Media for DemoMedia {
    fn host_now_s(&self) -> f64 {
        self.0.borrow().now
    }
    fn film_ready(&self) -> bool {
        self.0.borrow().film.is_ready()
    }
    fn film_open(&mut self, _: &Path) -> Result<(), String> {
        Err("the screenshots show one film".to_owned())
    }
    fn film_play_at(&mut self, film_s: f64, host_s: f64) -> Result<Timebase, String> {
        self.0.borrow_mut().playing = Some((film_s, host_s));
        Ok(Timebase {
            rate: 1.0,
            film_anchor_s: film_s,
            host_anchor_s: host_s,
        })
    }
    fn film_timebase(&self) -> Timebase {
        let state = self.0.borrow();
        match state.playing {
            Some((film_s, host_s)) => Timebase {
                rate: if state.now >= host_s { 1.0 } else { 0.0 },
                film_anchor_s: film_s,
                host_anchor_s: host_s,
            },
            None => Timebase {
                rate: 0.0,
                film_anchor_s: state.park_s,
                host_anchor_s: state.now,
            },
        }
    }
    fn film_pause(&mut self) {
        let mut state = self.0.borrow_mut();
        if let Some((film_s, host_s)) = state.playing.take() {
            state.park_s = film_s + (state.now - host_s).max(0.0);
        }
    }
    fn film_seek(&mut self, film_s: f64) {
        let mut state = self.0.borrow_mut();
        state.playing = None;
        state.park_s = film_s;
    }
    fn film_set_gain_db(&mut self, _: f64) {}
    fn film_set_muted(&mut self, _: bool) {}
    fn latest_frame(&mut self) -> Option<Frame> {
        let mut state = self.0.borrow_mut();
        let film_s = match state.playing {
            Some((film_s, host_s)) => film_s + (state.now - host_s).max(0.0),
            None => state.park_s,
        }
        .max(0.0);
        // Within a frame of what is shown: no new seek.
        if state
            .shown_s
            .is_none_or(|shown| (shown - film_s).abs() > 0.02)
        {
            state.film.seek(film_s);
            state.shown_s = Some(film_s);
        }
        state.film.latest_frame()
    }
    fn inputs(&self) -> Vec<AvailableInput> {
        vec![AvailableInput {
            name: INPUT.to_owned(),
            sample_rate: RECORD_RATE,
        }]
    }
    fn input_latency_s(&self, _: &str) -> Result<f64, String> {
        Ok(LATENCY_S)
    }
    fn start_capture(
        &mut self,
        device: &str,
        _: u16,
        sink: Sender<CaptureBuffer>,
    ) -> Result<(), String> {
        if device != INPUT {
            return Err(format!("no input device named {device:?}"));
        }
        let mut state = self.0.borrow_mut();
        state.sink = Some(sink);
        state.fed_until = state.now;
        Ok(())
    }
    fn stop_capture(&mut self) {
        self.0.borrow_mut().sink = None;
    }
    fn capture_error(&self) -> Option<String> {
        None
    }
    fn open_take(&mut self, _: &Path) -> Result<(), String> {
        Ok(())
    }
    fn take_ready(&self) -> bool {
        true
    }
    fn take_play_at(&mut self, _: f64, _: f64) -> Result<(), String> {
        Ok(())
    }
    fn take_set_gain_db(&mut self, _: f64) {}
    fn stop_take(&mut self) {}
    fn outputs(&self) -> Vec<DeviceInfo> {
        vec![
            DeviceInfo {
                name: INPUT.to_owned(),
                sample_rate: RECORD_RATE,
                channels: 2,
                transport: Transport::Usb,
                output_source: OutputSource::Unknown,
            },
            DeviceInfo {
                name: SPEAKERS.to_owned(),
                sample_rate: RECORD_RATE,
                channels: 2,
                transport: Transport::BuiltIn,
                output_source: OutputSource::Speakers,
            },
        ]
    }
    fn default_output(&self) -> Option<String> {
        Some(SPEAKERS.to_owned())
    }
    fn set_output(&mut self, device: &str) -> Result<(), String> {
        if self.outputs().iter().any(|output| output.name == device) {
            Ok(())
        } else {
            Err(format!("no output device named {device:?}"))
        }
    }
    fn start_channel_meter(&mut self, _: &str) -> Result<(), String> {
        self.0.borrow_mut().channel_meter = true;
        Ok(())
    }
    fn stop_channel_meter(&mut self) {
        self.0.borrow_mut().channel_meter = false;
    }
    fn channel_levels(&self) -> Vec<f32> {
        let state = self.0.borrow();
        if state.channel_meter {
            state.levels.clone()
        } else {
            Vec::new()
        }
    }
    fn set_nominal_rate(&mut self, _: &str, _: u32) -> Result<(), String> {
        Ok(())
    }
    fn buffer_frames(&self, _: &str) -> Option<u32> {
        Some(256)
    }
    fn monitor_unavailable(&self) -> Option<String> {
        Monitor::available().err().map(|err| err.to_string())
    }
    fn microphone_permission(&self) -> tw::MicPermission {
        tw::MicPermission::Granted
    }
}

/// A transcriber that starts a little later than the one it wraps, so a
/// render of the window can show "checking…" before the check is in.
struct Unhurried(SharedTranscriber);

impl tw::Transcriber for Unhurried {
    fn transcribe(&self, wav: &Path) -> tw::Transcript {
        std::thread::sleep(Duration::from_secs(2));
        self.0.transcribe(wav)
    }
    fn model(&self) -> String {
        self.0.model()
    }
}

/// A hidden recording window on the demo project, and its media.
struct Shot {
    window: AnyWindowHandle,
    view: Entity<RecordingWindow>,
    media: DemoMedia,
}

impl Shot {
    /// Opens a hidden window of `size` on the project in `dir` with
    /// [`DemoMedia`] and waits for its film to load. With `checks`, the
    /// window checks new takes as Takewise does; otherwise its takes are
    /// all checked before it opens.
    async fn open(
        dir: &Path,
        (width, height): (f32, f32),
        checks: bool,
        cx: &mut AsyncApp,
    ) -> Result<Self, String> {
        let loaded = load(dir).map_err(|err| err.to_string())?;
        let media = DemoMedia::open(&loaded.project.film_path())?;
        let checker = if checks {
            let mut checker = super::checker(&loaded.project);
            checker.transcriber = Arc::new(Unhurried(checker.transcriber));
            checker
        } else {
            Checker {
                transcriber: Arc::new(tw::Unavailable {
                    reason: "not checked in screenshots".to_owned(),
                    model: "none".to_owned(),
                }),
                tools: Vec::new(),
                tag: worker_tag(),
            }
        };
        let window_media = media.clone();
        let dir = dir.to_path_buf();
        let (window, view) = cx.update(|cx| {
            let options = hidden_window((width, height), cx);
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| {
                    RecordingWindow::new(
                        dir,
                        loaded,
                        Box::new(window_media),
                        None,
                        checker,
                        window,
                        cx,
                    )
                })
            })
            .map_err(|err| format!("cannot open the window: {err}"))
        })?;
        let shot = Self {
            window,
            view,
            media,
        };
        for _ in 0..250 {
            if shot.media.film_ready() {
                return Ok(shot);
            }
            wait(cx, 0.02).await;
        }
        Err("the film did not load".to_owned())
    }

    fn update(&self, cx: &mut AsyncApp, f: impl FnOnce(&mut RecordingWindow)) {
        cx.update(|cx| self.view.update(cx, |view, _| f(view)));
    }

    /// The film time the window plays at, while it plays.
    fn playhead(&self, cx: &mut AsyncApp) -> Option<f64> {
        cx.update(|cx| {
            let view = self.view.read(cx);
            view.recorder.playhead_s(&*view.media)
        })
    }

    /// Draws frames, the clock standing still, until `done` holds for the
    /// window; work off the UI thread (saving a take, checking it) runs
    /// meanwhile.
    async fn wait_for(
        &self,
        cx: &mut AsyncApp,
        what: &str,
        done: impl Fn(&RecordingWindow) -> bool,
    ) -> Result<(), String> {
        for _ in 0..1800 {
            self.draw(cx);
            if cx.update(|cx| done(self.view.read(cx))) {
                return Ok(());
            }
            wait(cx, 0.1).await;
        }
        Err(format!("timed out waiting for {what}"))
    }

    /// Draws a frame (the window's tick runs in it).
    fn draw(&self, cx: &mut AsyncApp) {
        cx.update(|cx| {
            let _ = self.window.update(cx, |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
        });
    }

    /// Moves the clock on to `to` in frames of 50 ms, feeding the capture.
    async fn run_until(&self, cx: &mut AsyncApp, to: f64) {
        while self.media.now() < to {
            let next = (self.media.now() + 0.05).min(to);
            self.media.advance(next);
            // The take writer runs on its own thread.
            wait(cx, 0.003).await;
            self.draw(cx);
        }
    }

    /// Lets the film's frame arrive (the clock stands still), then renders
    /// the window offscreen at the screen's scale.
    async fn render(&self, cx: &mut AsyncApp) -> Result<RgbaImage, String> {
        for _ in 0..10 {
            self.draw(cx);
            wait(cx, 0.05).await;
        }
        self.draw(cx);
        cx.update(|cx| {
            self.window
                .update(cx, |_, window, _| window.render_to_image())
        })
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())
    }

    fn close(self, cx: &mut AsyncApp) {
        cx.update(|cx| {
            let _ = self
                .window
                .update(cx, |_, window, _| window.remove_window());
        });
    }
}

async fn wait(cx: &mut AsyncApp, s: f64) {
    cx.background_executor()
        .timer(Duration::from_secs_f64(s))
        .await;
}

/// The part of a render of a window `window_width` wide at `x`, `y`,
/// `w`, `h` in logical pixels.
fn crop(image: &RgbaImage, window_width: f32, (x, y, w, h): (f32, f32, f32, f32)) -> RgbaImage {
    let scale = scale_of(image, window_width);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let px = |v: f32| (v * scale).round() as u32;
    image::imageops::crop_imm(image, px(x), px(y), px(w), px(h)).to_image()
}

/// Pixels per logical pixel in a render of a window `window_width` wide.
#[allow(clippy::cast_precision_loss)]
fn scale_of(image: &RgbaImage, window_width: f32) -> f32 {
    image.width() as f32 / window_width
}

/// `images` one below the other, centred, `gap` pixels apart, on `fill`.
fn stack(images: &[RgbaImage], gap: u32, fill: image::Rgba<u8>) -> RgbaImage {
    let gaps = gap * u32::try_from(images.len().saturating_sub(1)).unwrap_or(0);
    let width = images.iter().map(RgbaImage::width).max().unwrap_or(0);
    let height = images.iter().map(RgbaImage::height).sum::<u32>() + gaps;
    let mut out = RgbaImage::from_pixel(width, height, fill);
    let mut y = 0;
    for image in images {
        let x = (width - image.width()) / 2;
        image::imageops::replace(&mut out, image, i64::from(x), i64::from(y));
        y += image.height() + gap;
    }
    out
}

fn opaque(color: u32) -> image::Rgba<u8> {
    let [_, r, g, b] = color.to_be_bytes();
    image::Rgba([r, g, b, 255])
}

const TRANSPARENT: image::Rgba<u8> = image::Rgba([0, 0, 0, 0]);
/// The frame around every image: light enough to set the dark window
/// apart from a dark page, quiet on a light one.
const FRAME: [f32; 3] = [86.0, 86.0, 96.0];

/// `image` with rounded corners (transparent outside them) and a thin
/// light border; `scale` is pixels per logical pixel.
fn framed(image: &RgbaImage, scale: f32) -> RgbaImage {
    let radius = 6.0 * scale;
    let border = scale.max(1.0);
    #[allow(clippy::cast_precision_loss)]
    let (width, height) = (image.width() as f32, image.height() as f32);
    let mut out = image.clone();
    #[allow(clippy::cast_precision_loss)]
    for (x, y, pixel) in out.enumerate_pixels_mut() {
        // The signed distance of the pixel's centre from the rounded
        // rectangle's edge, negative inside.
        let qx = (x as f32 + 0.5 - width / 2.0).abs() - (width / 2.0 - radius);
        let qy = (y as f32 + 0.5 - height / 2.0).abs() - (height / 2.0 - radius);
        let outside = qx.max(0.0).hypot(qy.max(0.0));
        let distance = outside + qx.max(qy).min(0.0) - radius;
        let coverage = (0.5 - distance).clamp(0.0, 1.0);
        let edge = (distance + border + 0.5).clamp(0.0, 1.0);
        let [r, g, b, a] = pixel.0;
        let mix = |c: u8, f: f32| f32::from(c) * (1.0 - edge) + f * edge;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let byte = |v: f32| v.round().clamp(0.0, 255.0) as u8;
        *pixel = image::Rgba([
            byte(mix(r, FRAME[0])),
            byte(mix(g, FRAME[1])),
            byte(mix(b, FRAME[2])),
            byte(f32::from(a) * coverage),
        ]);
    }
    out
}

/// Writes `image` as a full-color PNG; `scripts/screenshots` compresses it
/// afterwards.
fn write_png(path: &Path, image: &RgbaImage) -> Result<(), String> {
    let fail = |err: &dyn std::fmt::Display| format!("{}: {err}", path.display());
    let file = std::fs::File::create(path).map_err(|err| fail(&err))?;
    let mut encoder =
        png::Encoder::new(std::io::BufWriter::new(file), image.width(), image.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header().map_err(|err| fail(&err))?;
    writer
        .write_image_data(image.as_raw())
        .map_err(|err| fail(&err))?;
    writer.finish().map_err(|err| fail(&err))?;
    eprintln!(
        "wrote {} ({}×{})",
        path.display(),
        image.width(),
        image.height()
    );
    Ok(())
}

/// Copies the project folder `from` to `to` (replacing it).
fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    if to.exists() {
        std::fs::remove_dir_all(to).map_err(|err| format!("{}: {err}", to.display()))?;
    }
    std::fs::create_dir_all(to).map_err(|err| format!("{}: {err}", to.display()))?;
    let entries = std::fs::read_dir(from).map_err(|err| format!("{}: {err}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| err.to_string())?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .map_err(|err| format!("{}: {err}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Selects line `id` and rests the film there, as ↑/↓ do.
fn select_line(view: &mut RecordingWindow, id: &str) {
    view.line = view.project.script.position(id).unwrap_or(0);
    view.take = None;
    view.scroll.reset();
    view.fix_take();
    view.park();
}

/// Every screenshot, in an order that keeps each one's state: the setup
/// check first (it saves `[setup]` and the room tone), the take list last
/// (it chooses a take). The loop records into its own project.
async fn shoot(demo: &Demo, work: &Path, out: &Path, cx: &mut AsyncApp) -> Result<(), String> {
    setup_check(demo, out, cx).await?;
    audio_panel(demo, out, cx).await?;
    recording(demo, work, out, cx).await?;
    social_preview(demo, work, out, cx).await?;
    take_loop(demo, work, out, cx).await?;
    take_list(demo, out, cx).await?;
    let cli = agent_session(demo, work)?;
    export_strip(&cli, out, cx).await
}

/// The setup check's voice and room steps with their results: the panel
/// of each, side by side.
async fn setup_check(demo: &Demo, out: &Path, cx: &mut AsyncApp) -> Result<(), String> {
    let shot = Shot::open(&demo.dir, WINDOW, false, cx).await?;
    shot.update(cx, |view| select_line(view, "orbit"));
    // The meter's capture starts with the first frames.
    let start = shot.media.now();
    shot.run_until(cx, start + 0.5).await;
    shot.update(cx, |view| {
        view.toggle_setup();
        view.setup_next();
    });
    let mut sentence = vec![0; seconds(0.3)];
    sentence.extend_from_slice(&demo.sentence);
    shot.media.hear(sentence);
    shot.update(cx, RecordingWindow::setup_listen);
    let now = shot.media.now();
    shot.run_until(cx, now + super::setup::CAPTURE_S + 0.2)
        .await;
    let voice = shot.render(cx).await?;

    shot.update(cx, RecordingWindow::setup_next);
    shot.media.hear(Vec::new());
    shot.update(cx, RecordingWindow::setup_listen);
    let now = shot.media.now();
    shot.run_until(cx, now + super::setup::CAPTURE_S + 0.2)
        .await;
    let room = shot.render(cx).await?;
    shot.update(cx, RecordingWindow::setup_next);
    shot.close(cx);

    // The setup panel sits between the film and the side column; each
    // step is an image of its own, so a narrow page stacks them.
    let x = WINDOW.0 - 300.0 - 380.0;
    let scale = scale_of(&voice, WINDOW.0);
    write_png(
        &out.join("setup-voice.png"),
        &framed(&crop(&voice, WINDOW.0, (x, 0.0, 380.0, 236.0)), scale),
    )?;
    write_png(
        &out.join("setup-room.png"),
        // As tall as the voice step, so the two line up side by side.
        &framed(&crop(&room, WINDOW.0, (x, 0.0, 380.0, 236.0)), scale),
    )
}

/// The audio panel's devices and channel meters, with the input meter
/// below it lit red by a peak near clipping.
async fn audio_panel(demo: &Demo, out: &Path, cx: &mut AsyncApp) -> Result<(), String> {
    let shot = Shot::open(&demo.dir, WINDOW, false, cx).await?;
    shot.update(cx, |view| {
        select_line(view, "storm");
        view.panel.open = true;
    });
    let start = shot.media.now();
    shot.run_until(cx, start + 0.5).await;
    // A plosive just below full scale, then the line read on.
    let mut voice = demo.voice("storm");
    #[allow(clippy::cast_possible_truncation)]
    let loud = (FULL_SCALE * 10f64.powf(-0.4 / 20.0)) as i32;
    if let Some(sample) = voice.get_mut(seconds(0.4)) {
        *sample = loud;
    }
    shot.media.hear(voice);
    let now = shot.media.now();
    shot.run_until(cx, now + 1.9).await;
    let image = shot.render(cx).await?;
    shot.close(cx);
    // The panel (between the film and the side column) down to the output,
    // and the meter row under the film: two cards, centred, each as wide as
    // what it holds.
    let scale = scale_of(&image, WINDOW.0);
    let panel = crop(
        &image,
        WINDOW.0,
        (WINDOW.0 - 300.0 - 320.0, 0.0, 319.0, 302.0),
    );
    let meter = crop(&image, WINDOW.0, (266.0, 500.0, 374.0, 44.0));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let gap = (12.0 * scale) as u32;
    let both = stack(
        &[framed(&panel, scale), framed(&meter, scale)],
        gap,
        TRANSPARENT,
    );
    write_png(&out.join("audio-panel.png"), &both)
}

/// A take of the long ocean line being recorded, the pace guide on its
/// third row, so the line has scrolled up by one.
async fn recording(demo: &Demo, work: &Path, out: &Path, cx: &mut AsyncApp) -> Result<(), String> {
    let image = mid_take(demo, work, "ocean", "like waves on a beach,", cx).await?;
    let scale = scale_of(&image, HERO.0);
    write_png(&out.join("recording.png"), &framed(&image, scale))
}

/// A render of a [`HERO`] window recording a take of `line`, the pace
/// guide filled up to the end of `filled_to`, while the voice reads the
/// line. It records into a copy of the project, so the take list after it
/// stays as it was.
async fn mid_take(
    demo: &Demo,
    work: &Path,
    line: &str,
    filled_to: &str,
    cx: &mut AsyncApp,
) -> Result<RgbaImage, String> {
    let copy = demo.dir.with_file_name("recording.takewise");
    copy_dir(&demo.dir, &copy)?;
    let shot = Shot::open(&copy, HERO, false, cx).await?;
    shot.update(cx, |view| select_line(view, line));
    let start = shot.media.now();
    shot.run_until(cx, start + 0.5).await;
    shot.update(cx, RecordingWindow::start_take);
    let (reading_start, reading_end, text) = cx.update(|cx| {
        let view = shot.view.read(cx);
        let line = &view.project.script.lines[view.line];
        let end = view
            .project
            .script
            .slot_end(view.line, view.project.film_duration());
        (line.start, end, line.text.clone())
    });
    let voice = speak(&text, &work.join("voices").join(format!("{line}.wav")))?;
    let reading = super::prompter::Reading {
        text: &text,
        start: reading_start,
        end: reading_end,
    };
    let filled = text
        .find(filled_to)
        .map_or(text.len() / 2, |at| at + filled_to.len());
    let mut shoot_at = reading_start;
    while super::prompter::filled_bytes(reading, shoot_at) < filled && shoot_at < reading_end {
        shoot_at += 0.005;
    }
    let mut speaking = false;
    for _ in 0..1000 {
        let film_s = shot.playhead(cx);
        if let Some(film_s) = film_s {
            if !speaking && film_s >= reading_start + LEAD_S {
                shot.media.hear(voice.clone());
                speaking = true;
            }
            if film_s >= shoot_at - 1e-6 {
                break;
            }
        }
        // Land on `shoot_at` exactly.
        let step = film_s.map_or(0.05, |film_s| (shoot_at - film_s).clamp(0.0, 0.05));
        let now = shot.media.now();
        shot.run_until(cx, now + step).await;
    }
    let image = shot.render(cx).await;
    shot.close(cx);
    image
}

/// The repository's social preview, 1280×640: the app's icon, name and
/// what it is beside a window recording the capsule line. Not linked from
/// the README; it is uploaded in the repository's settings.
async fn social_preview(
    demo: &Demo,
    work: &Path,
    out: &Path,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    const CARD: (f32, f32) = (1280.0, 640.0);
    let window = mid_take(demo, work, "capsule", "port,", cx).await?;
    let window = framed(&window, scale_of(&window, HERO.0));
    let icon = image::open(out.join("icon.png"))
        .map_err(|err| format!("{}: {err}", out.join("icon.png").display()))?
        .to_rgba8();
    let card = Card {
        icon: render_image(&icon),
        window: render_image(&window),
    };
    let image = render_view(card, CARD, cx).await?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let small = image::imageops::resize(
        &image,
        CARD.0 as u32,
        CARD.1 as u32,
        image::imageops::FilterType::Lanczos3,
    );
    write_png(&out.join("social-preview.png"), &small)
}

/// Options for a hidden window of `width`×`height` logical pixels on the
/// display with the highest scale, so the renders stay sharp when the main
/// display is a 1x monitor.
fn hidden_window((width, height): (f32, f32), cx: &mut App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(width), px(height)),
        })),
        focus: false,
        show: false,
        display_id: sharpest_display(cx),
        ..WindowOptions::default()
    }
}

thread_local! {
    static SHARPEST: std::cell::Cell<Option<gpui_kit::DisplayId>> =
        const { std::cell::Cell::new(None) };
}

/// The display whose windows have the highest scale, found once by opening
/// a hidden window on each; `None`, without displays, leaves the choice to
/// GPUI.
fn sharpest_display(cx: &mut App) -> Option<gpui_kit::DisplayId> {
    if let Some(known) = SHARPEST.get() {
        return Some(known);
    }
    let mut best: Option<(gpui_kit::DisplayId, f32)> = None;
    for display in cx.displays() {
        let id = display.id();
        let options = WindowOptions {
            focus: false,
            show: false,
            display_id: Some(id),
            ..WindowOptions::default()
        };
        let mut scale = 0.0;
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            scale = window.scale_factor();
            cx.new(|_| Probe)
        });
        if let Ok((handle, _)) = opened {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
        if best.is_none_or(|(_, top)| scale > top) {
            best = Some((id, scale));
        }
    }
    let chosen = best.map(|(id, _)| id);
    SHARPEST.set(chosen);
    chosen
}

/// An empty view for [`sharpest_display`]'s probe windows.
struct Probe;

impl gpui_kit::Render for Probe {
    fn render(
        &mut self,
        _: &mut gpui_kit::Window,
        _: &mut gpui_kit::Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        gpui_kit::div()
    }
}

/// Renders `view` offscreen in a hidden window of `width`×`height`
/// logical pixels, at the screen's scale.
async fn render_view<V: gpui_kit::Render + 'static>(
    view: V,
    (width, height): (f32, f32),
    cx: &mut AsyncApp,
) -> Result<RgbaImage, String> {
    let (handle, _) = cx.update(|cx| {
        let options = hidden_window((width, height), cx);
        gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| view))
            .map_err(|err| format!("cannot open the window: {err}"))
    })?;
    let draw = |cx: &mut AsyncApp| {
        cx.update(|cx| {
            let _ = handle.update(cx, |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx);
            });
        });
    };
    for _ in 0..10 {
        draw(cx);
        wait(cx, 0.05).await;
    }
    draw(cx);
    let image = cx
        .update(|cx| handle.update(cx, |_, window, _| window.render_to_image()))
        .map_err(|err| err.to_string())?
        .map_err(|err| err.to_string())?;
    cx.update(|cx| {
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    });
    Ok(image)
}

/// An RGBA image as GPUI draws it (BGRA).
fn render_image(image: &RgbaImage) -> Arc<gpui_kit::RenderImage> {
    let mut bgra = image.clone();
    for pixel in bgra.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Arc::new(gpui_kit::RenderImage::new(vec![image::Frame::new(bgra)]))
}

/// The social preview's layout.
struct Card {
    icon: Arc<gpui_kit::RenderImage>,
    window: Arc<gpui_kit::RenderImage>,
}

impl gpui_kit::Render for Card {
    fn render(
        &mut self,
        _: &mut gpui_kit::Window,
        _: &mut gpui_kit::Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        use gpui_kit::{FontWeight, ParentElement as _, Styled as _, div, img, rgb};
        div()
            .size_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(44.))
            .px(px(56.))
            .bg(rgb(super::colors::BACKGROUND))
            .text_color(rgb(super::colors::TEXT))
            .child(
                div()
                    .w(px(400.))
                    .flex()
                    .flex_col()
                    .gap(px(18.))
                    .child(img(self.icon.clone()).size(px(104.)))
                    .child(
                        div()
                            .text_size(px(64.))
                            .font_weight(FontWeight::BOLD)
                            .child("Takewise"),
                    )
                    .child(
                        div()
                            .text_size(px(27.))
                            .line_height(px(36.))
                            .text_color(rgb(super::colors::DIM))
                            .child("A Mac app for recording voiceover over a finished film."),
                    ),
            )
            .child(img(self.window.clone()).w(px(724.)).h(px(463.36)))
    }
}

/// The loop's frames and how long each shows, in milliseconds.
#[derive(Default)]
struct Frames(Vec<(RgbaImage, u32)>);

impl Frames {
    /// Adds a render of the window, scaled to [`LOOP_WIDTH`] and dimmed,
    /// with `region` (logical pixels) of it on top at the render's full
    /// resolution: a close-up whose text stays sharp in the README.
    fn add_close_up(&mut self, image: &RgbaImage, region: (f32, f32, f32, f32), ms: u32) {
        const DIM: f32 = 0.35;
        let height = image.height() * LOOP_WIDTH / image.width();
        let mut base = image::imageops::resize(
            image,
            LOOP_WIDTH,
            height,
            image::imageops::FilterType::Lanczos3,
        );
        for pixel in base.pixels_mut() {
            for channel in &mut pixel.0[..3] {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let dimmed = (f32::from(*channel) * DIM).round() as u8;
                *channel = dimmed;
            }
        }
        let part = framed(&crop(image, HERO.0, region), scale_of(image, HERO.0));
        let x = (base.width().saturating_sub(part.width())) / 2;
        let y = (base.height().saturating_sub(part.height())) / 2;
        image::imageops::overlay(&mut base, &part, i64::from(x), i64::from(y));
        self.0.push((framed(&base, scale_of(&base, HERO.0)), ms));
    }

    /// Adds a render of the window, scaled to [`LOOP_WIDTH`] and framed.
    fn add(&mut self, image: &RgbaImage, ms: u32) {
        let height = image.height() * LOOP_WIDTH / image.width();
        let small = image::imageops::resize(
            image,
            LOOP_WIDTH,
            height,
            image::imageops::FilterType::Lanczos3,
        );
        self.0.push((framed(&small, scale_of(&small, HERO.0)), ms));
    }

    /// Writes the frames into `dir` and puts them together as the
    /// looping animated WebP `out` with `img2webp`.
    fn write_webp(&self, dir: &Path, out: &Path) -> Result<(), String> {
        if dir.exists() {
            std::fs::remove_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        }
        std::fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        let mut args: Vec<OsString> = vec!["-loop".into(), "0".into(), "-lossy".into()];
        args.extend(["-q", "80", "-m", "6"].map(OsString::from));
        for (n, (image, ms)) in self.0.iter().enumerate() {
            let path = dir.join(format!("{n:03}.png"));
            image
                .save(&path)
                .map_err(|err| format!("{}: {err}", path.display()))?;
            args.extend(["-d".into(), ms.to_string().into(), path.into_os_string()]);
        }
        args.extend(["-o".into(), out.as_os_str().to_owned()]);
        let status = Command::new("img2webp")
            .args(&args)
            .stdout(std::process::Stdio::null())
            .status()
            .map_err(|err| format!("cannot run img2webp (brew install webp): {err}"))?;
        if !status.success() {
            return Err(format!("img2webp failed for {}", out.display()));
        }
        let bytes = std::fs::metadata(out).map_or(0, |m| m.len());
        eprintln!(
            "wrote {} ({} frames, {} KB)",
            out.display(),
            self.0.len(),
            bytes / 1024
        );
        Ok(())
    }
}

/// The loop: two takes of the capsule line recorded and checked in a
/// window whose checks run as in Takewise; the first misses "home", the
/// second is kept and chosen. The takes play at twice their speed.
async fn take_loop(demo: &Demo, work: &Path, out: &Path, cx: &mut AsyncApp) -> Result<(), String> {
    // The setup check ran on the demo project; the loop's has the same
    // devices and settings, and its room tone.
    let config = demo.dir.join("project.toml");
    std::fs::copy(&config, demo.anim.join("project.toml"))
        .map_err(|err| format!("{}: {err}", config.display()))?;
    let room = Path::new("takes").join(".room");
    copy_dir(&demo.dir.join(&room), &demo.anim.join(&room))?;
    let shot = Shot::open(&demo.anim, HERO, true, cx).await?;
    shot.update(cx, |view| select_line(view, "capsule"));
    let start = shot.media.now();
    shot.run_until(cx, start + 0.5).await;
    let mut frames = Frames::default();
    for (n, &spoken) in LOOP_TAKES.iter().enumerate() {
        shot.update(cx, RecordingWindow::start_take);
        record(&shot, &demo.spoken[spoken], &mut frames, cx).await?;
        shot.update(cx, RecordingWindow::finish_take);
        shot.wait_for(cx, "the take to be saved", |view| {
            view.line_state().is_some_and(|line| line.takes.len() > n)
        })
        .await?;
        frames.add(&shot.render(cx).await?, if n == 0 { 900 } else { 700 });
        shot.wait_for(cx, "the take's check", |view| {
            view.line_state()
                .and_then(|line| line.takes.get(n))
                .is_some_and(|take| take.effective.is_some())
                && view.checks.pending_of("capsule").is_empty()
        })
        .await?;
        let checked = shot.render(cx).await?;
        frames.add(&checked, 600);
        frames.add_close_up(&checked, CLOSE_UP, if n == 0 { 2000 } else { 1400 });
    }
    // Enter: the narrator chooses the kept take.
    let chosen = u32::try_from(LOOP_TAKES.len()).unwrap_or(1);
    shot.update(cx, |view| {
        view.take = Some(chosen);
        match tw::choose_take(&view.dir, "capsule", chosen, "narrator") {
            Ok(()) => {
                view.reload();
                view.message = Some(format!("Chose capsule take {chosen}."));
            }
            Err(err) => view.message = Some(err.to_string()),
        }
    });
    let chosen = shot.render(cx).await?;
    frames.add(&chosen, 900);
    frames.add_close_up(&chosen, CLOSE_UP, 3000);
    shot.close(cx);
    frames.write_webp(&work.join("loop-frames"), &out.join("loop.webp"))
}

/// Runs a take in `shot` while the voice says `speech` from just after the
/// line's start, adding a frame every 0.25 s of film from the count-in on;
/// stops when the speech has ended.
async fn record(
    shot: &Shot,
    speech: &[i32],
    frames: &mut Frames,
    cx: &mut AsyncApp,
) -> Result<(), String> {
    const STEP_S: f64 = 0.05;
    const FRAME_EVERY: u32 = 5;
    let line_start = cx.update(|cx| {
        let view = shot.view.read(cx);
        view.project.script.lines[view.line].start
    });
    #[allow(clippy::cast_precision_loss)]
    let speech_s = speech.len() as f64 / f64::from(RECORD_RATE);
    let mut speaking = false;
    for step in 1..=600 {
        let now = shot.media.now();
        shot.run_until(cx, now + STEP_S).await;
        if let Some(film_s) = shot.playhead(cx) {
            if !speaking && film_s >= line_start + LEAD_S {
                shot.media.hear(speech.to_vec());
                speaking = true;
            }
            if film_s >= line_start + LEAD_S + speech_s + 0.4 {
                return Ok(());
            }
        }
        // The take's frames start with the count-in's 3, so the loop opens
        // on it; the moment before the film starts shows no digit.
        let counting = cx.update(|cx| {
            let view = shot.view.read(cx);
            view.recorder.count_in(&*view.media).is_some()
                || view
                    .recorder
                    .playhead_s(&*view.media)
                    .is_some_and(|film_s| film_s >= line_start)
        });
        if step % FRAME_EVERY == 0 && counting {
            frames.add(&shot.render(cx).await?, 125);
        }
    }
    Err("the take did not reach the end of its speech".to_owned())
}

/// The capsule line's takes after the narrator chose the proposed one.
async fn take_list(demo: &Demo, out: &Path, cx: &mut AsyncApp) -> Result<(), String> {
    tw::choose_take(&demo.dir, "capsule", 4, "narrator").map_err(|err| err.to_string())?;
    let shot = Shot::open(&demo.dir, WINDOW, false, cx).await?;
    shot.update(cx, |view| {
        select_line(view, "capsule");
        view.take = Some(1);
    });
    let start = shot.media.now();
    shot.run_until(cx, start + 0.5).await;
    let image = shot.render(cx).await?;
    shot.close(cx);
    let scale = scale_of(&image, WINDOW.0);
    write_png(
        &out.join("takes.png"),
        &framed(
            &crop(&image, WINDOW.0, (WINDOW.0 - 300.0, 344.0, 300.0, 230.0)),
            scale,
        ),
    )
}

/// The agent's part of the README's CLI example, run for real with this
/// binary on a copy of the demo project named like it
/// (`<work>/cli/earth.takewise`): `status`, then it judges and chooses the
/// storm line's take and exports. The transcript goes to
/// `<work>/cli/transcript.txt`, for the README. Returns the copy.
fn agent_session(demo: &Demo, work: &Path) -> Result<PathBuf, String> {
    const STEPS: &[&[&str]] = &[
        &["status", "earth.takewise"],
        &[
            "judge",
            "earth.takewise",
            "storm",
            "1",
            "--verdict",
            "keep",
            "--reason",
            "clear, unhurried",
            "--by",
            "claude-opus-5-5",
            "--score",
            "0.8",
        ],
        &[
            "choose",
            "earth.takewise",
            "storm",
            "1",
            "--by",
            "claude-opus-5-5",
        ],
        &["export", "earth.takewise"],
    ];
    let cli = work.join("cli");
    let project = cli.join("earth.takewise");
    copy_dir(&demo.dir, &project)?;
    let exe = std::env::current_exe().map_err(|err| format!("this binary: {err}"))?;
    let mut transcript = String::new();
    for args in STEPS {
        let output = Command::new(&exe)
            .args(*args)
            .current_dir(&cli)
            .output()
            .map_err(|err| format!("cannot run takewise {}: {err}", args[0]))?;
        let shown: Vec<String> = args
            .iter()
            .map(|a| {
                if a.contains(' ') {
                    format!("\"{a}\"")
                } else {
                    (*a).to_owned()
                }
            })
            .collect();
        let _ = writeln!(transcript, "$ takewise {}", shown.join(" "));
        transcript.push_str(&String::from_utf8_lossy(&output.stdout));
        transcript.push_str(&String::from_utf8_lossy(&output.stderr));
        if !output.status.success() && args[0] != "status" {
            return Err(format!("takewise {} failed:\n{transcript}", args[0]));
        }
    }
    let path = cli.join("transcript.txt");
    std::fs::write(&path, &transcript).map_err(|err| format!("{}: {err}", path.display()))?;
    eprintln!("wrote {}", path.display());
    Ok(project)
}

/// The narration the agent exported, drawn as one strip as long as the
/// film, each line's id where the manifest places its take.
async fn export_strip(project: &Path, out: &Path, cx: &mut AsyncApp) -> Result<(), String> {
    // Type about twice the window's, so the strip still reads when a
    // phone shows it about 324 px wide.
    const SIZE: (f32, f32) = (830.0, 310.0);
    const PAD: f32 = 20.0;
    const WAVE_H: f32 = 96.0;
    /// Drawn at this many pixels per logical pixel; GPUI scales it.
    const WAVE_SCALE: f32 = 2.0;
    let export = project.join("export");
    let manifest_path = export.join("manifest.json");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|err| format!("{}: {err}", manifest_path.display()))?;
    let manifest: serde_json::Value =
        serde_json::from_str(&text).map_err(|err| format!("{}: {err}", manifest_path.display()))?;
    let file = manifest["narration"]["file"]
        .as_str()
        .ok_or("the manifest names no narration file")?;
    let (samples, rate) = tw::read_take_wav(&export.join(file)).map_err(|err| err.to_string())?;
    #[allow(clippy::cast_precision_loss)]
    let total = samples.len().max(1) as f64;
    let lines: Vec<(String, f64, f64)> = manifest["lines"]
        .as_array()
        .ok_or("the manifest has no lines")?
        .iter()
        .filter_map(|line| {
            let id = line["id"].as_str()?.to_owned();
            let dest = line["dest_samples"].as_array()?;
            #[allow(clippy::cast_precision_loss)]
            let at = |n: usize| dest.get(n)?.as_u64().map(|v| v as f64 / total);
            Some((id, at(0)?, at(1)?))
        })
        .collect();

    let inner = SIZE.0 - 2.0 * PAD;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (width, height) = ((inner * WAVE_SCALE) as u32, (WAVE_H * WAVE_SCALE) as u32);
    let mut wave = RgbaImage::from_pixel(width, height, opaque(super::colors::PANEL));
    let band = opaque(super::colors::SELECTED);
    let ink = opaque(super::colors::ACCENT);
    let rule = opaque(super::colors::FAINT);
    let mid = height / 2;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    for x in 0..width {
        let from = (f64::from(x) / f64::from(width) * total) as usize;
        let to = ((f64::from(x + 1) / f64::from(width) * total) as usize).min(samples.len());
        let at = f64::from(x) / f64::from(width);
        if lines.iter().any(|(_, a, b)| at >= *a && at < *b) {
            for y in 0..height {
                wave.put_pixel(x, y, band);
            }
        }
        let peak = samples[from.min(to)..to]
            .iter()
            .map(|s| s.unsigned_abs())
            .max()
            .unwrap_or(0);
        let reach = ((f64::from(peak) / FULL_SCALE).min(1.0) * f64::from(mid - 2)) as u32;
        wave.put_pixel(x, mid, rule);
        for y in mid.saturating_sub(reach)..=(mid + reach).min(height - 1) {
            wave.put_pixel(x, y, ink);
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let film_s = total / f64::from(rate);
    let level = match (
        manifest["loudness_lufs"].as_f64(),
        manifest["true_peak_dbtp"].as_f64(),
    ) {
        (Some(lufs), Some(peak)) => format!(" · {lufs:.1} LUFS · true peak {peak:.1} dBTP"),
        _ => String::new(),
    };
    #[allow(clippy::cast_possible_truncation)]
    let strip = Strip {
        wave: render_image(&wave),
        title: format!("export/{file}"),
        detail: format!("{film_s:.1} s, as long as the film{level}"),
        labels: lines
            .iter()
            .map(|(id, at, _)| (id.clone(), (*at * f64::from(inner)) as f32))
            .collect(),
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        ticks: (0..=(film_s / 10.0).floor() as u32)
            .map(|n| {
                let s = f64::from(n * 10);
                (
                    format!("{}:{:02}", n * 10 / 60, n * 10 % 60),
                    (s / film_s * f64::from(inner)) as f32,
                )
            })
            .collect(),
        inner,
        pad: PAD,
        wave_h: WAVE_H,
    };
    let image = render_view(strip, SIZE, cx).await?;
    let scale = scale_of(&image, SIZE.0);
    write_png(&out.join("export.png"), &framed(&image, scale))
}

/// The export image's layout: the file and its measurements, the line ids
/// at their places, the waveform with each line's take shaded, and the
/// film's time below.
struct Strip {
    wave: Arc<gpui_kit::RenderImage>,
    title: String,
    detail: String,
    /// Each line's id and its x in logical pixels from the strip's left.
    labels: Vec<(String, f32)>,
    /// Each tick's label and x.
    ticks: Vec<(String, f32)>,
    inner: f32,
    pad: f32,
    wave_h: f32,
}

impl gpui_kit::Render for Strip {
    fn render(
        &mut self,
        _: &mut gpui_kit::Window,
        _: &mut gpui_kit::Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        use gpui_kit::{ParentElement as _, Styled as _, div, img, rgb};
        let row = |height: f32, items: &[(String, f32)], color: u32, size: f32| {
            div()
                .relative()
                .w(px(self.inner))
                .h(px(height))
                .flex_none()
                .children(items.iter().map(|(text, x)| {
                    let label = div().absolute().top(px(0.)).whitespace_nowrap();
                    // A label at the very end ends there instead.
                    let label = if *x > self.inner - 70.0 {
                        label.right(px(0.))
                    } else {
                        label.left(px(*x))
                    };
                    label
                        .text_size(px(size))
                        .text_color(rgb(color))
                        .child(text.clone())
                }))
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .p(px(self.pad))
            .gap(px(6.))
            .bg(rgb(super::colors::BACKGROUND))
            .text_color(rgb(super::colors::TEXT))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .flex_col()
                    .gap(px(2.))
                    .pb(px(8.))
                    .child(div().text_size(px(30.)).child(self.title.clone()))
                    .child(
                        div()
                            .text_size(px(24.))
                            .text_color(rgb(super::colors::DIM))
                            .child(self.detail.clone()),
                    ),
            )
            .child(row(32.0, &self.labels, super::colors::TEXT, 24.0))
            .child(
                img(self.wave.clone())
                    .flex_none()
                    .w(px(self.inner))
                    .h(px(self.wave_h)),
            )
            .child(row(28.0, &self.ticks, super::colors::DIM, 22.0))
    }
}
