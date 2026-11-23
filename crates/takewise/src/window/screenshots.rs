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
//! offscreen to `<out>/*.png`. Nothing plays, no microphone opens, no window
//! appears and no key or mouse event is sent.

use std::cell::RefCell;
use std::ffi::OsString;
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
use takewise_media::{CaptureBuffer, DeviceInfo, FilmPlayer, Frame, Monitor, Transport};

use super::checks::{Checker, worker_tag};
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
start = 22.5
end = 26.6
text = "A capsule waits at its port, ready to bring the crew home."
note = "Calm. Let it land on “home”."

[[line]]
id = "storm"
start = 32.5
end = 36.0
text = "From up here, a storm is something you look down on."

[[line]]
id = "ocean"
start = 42.5
end = 47.8
text = "Below, the open ocean, and rows of cloud that run for hundreds of miles."

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
/// The window's size, as Takewise opens it.
const WINDOW: (f32, f32) = (1280.0, 820.0);

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
            match shoot(&demo, &out, cx).await {
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
        },
    )
    .map_err(fail)?;
    std::fs::write(dir.join("script.toml"), SCRIPT).map_err(|err| err.to_string())?;
    let preroll_s = project.config.record.preroll_s;

    let mut voices = Vec::new();
    for (n, (line, text)) in TAKES.iter().enumerate() {
        let wav = voices_dir.join(format!("{n:02}-{line}.wav"));
        let speech = speak(text, &wav)?;
        let samples = take_samples(&speech, preroll_s, u32::try_from(n).unwrap_or(0));
        commit(&dir, line, &samples, preroll_s)?;
        voices.push(((*line).to_owned(), speech));
    }
    let sentence = speak(super::setup::SENTENCE, &voices_dir.join("sentence.wav"))?;

    check_all(&dir)?;
    tw::choose_take(&dir, "orbit", 1, "narrator").map_err(fail)?;
    tw::choose_take(&dir, "coast", 1, "narrator").map_err(fail)?;
    Ok(Demo {
        dir,
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
    fn stop_take(&mut self) {}
    fn outputs(&self) -> Vec<DeviceInfo> {
        vec![
            DeviceInfo {
                name: INPUT.to_owned(),
                sample_rate: RECORD_RATE,
                channels: 2,
                transport: Transport::Usb,
            },
            DeviceInfo {
                name: SPEAKERS.to_owned(),
                sample_rate: RECORD_RATE,
                channels: 2,
                transport: Transport::BuiltIn,
            },
        ]
    }
    fn default_output(&self) -> Option<String> {
        Some(SPEAKERS.to_owned())
    }
    fn film_set_output(&mut self, device: &str) -> Result<(), String> {
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

/// A hidden recording window on the demo project, and its media.
struct Shot {
    window: AnyWindowHandle,
    view: Entity<RecordingWindow>,
    media: DemoMedia,
}

impl Shot {
    /// Opens a hidden window on the project in `dir` with [`DemoMedia`]
    /// and waits for its film to load.
    async fn open(dir: &Path, cx: &mut AsyncApp) -> Result<Self, String> {
        let loaded = load(dir).map_err(|err| err.to_string())?;
        let media = DemoMedia::open(&loaded.project.film_path())?;
        let window_media = media.clone();
        let dir = dir.to_path_buf();
        let (window, view) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.), px(0.)),
                    size: size(px(WINDOW.0), px(WINDOW.1)),
                })),
                focus: false,
                show: false,
                ..WindowOptions::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| {
                    RecordingWindow::new(
                        dir,
                        loaded,
                        Box::new(window_media),
                        None,
                        // Every take is checked before the windows open.
                        Checker {
                            transcriber: Arc::new(tw::Unavailable {
                                reason: "not checked in screenshots".to_owned(),
                                model: "none".to_owned(),
                            }),
                            tools: Vec::new(),
                            tag: worker_tag(),
                        },
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

/// The part of a window's `image` at `x`, `y`, `w`, `h` in logical pixels.
fn crop(image: &RgbaImage, (x, y, w, h): (f32, f32, f32, f32)) -> RgbaImage {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scale = |v: f32| (v * image.width() as f32 / WINDOW.0).round() as u32;
    image::imageops::crop_imm(image, scale(x), scale(y), scale(w), scale(h)).to_image()
}

/// `images` one below the other, divided by a line in the panels' border
/// color.
fn stack(images: &[RgbaImage]) -> RgbaImage {
    let divider = 2;
    let width = images.iter().map(RgbaImage::width).max().unwrap_or(0);
    let height = images.iter().map(RgbaImage::height).sum::<u32>()
        + divider * u32::try_from(images.len().saturating_sub(1)).unwrap_or(0);
    let [r, g, b] = rgb_bytes(super::colors::SELECTED);
    let mut out = RgbaImage::from_pixel(width, height, image::Rgba([r, g, b, 255]));
    let mut y = 0;
    for image in images {
        image::imageops::replace(&mut out, image, 0, i64::from(y));
        y += image.height() + divider;
    }
    out
}

fn rgb_bytes(color: u32) -> [u8; 3] {
    let [_, r, g, b] = color.to_be_bytes();
    [r, g, b]
}

/// Writes `image` as an optimized PNG: reduced to a 256-color palette
/// (`NeuQuant`), which keeps the screenshots small enough for a README.
fn write_png(path: &Path, image: &RgbaImage) -> Result<(), String> {
    let fail = |err: &dyn std::fmt::Display| format!("{}: {err}", path.display());
    let pixels = image.as_raw();
    let quant = color_quant::NeuQuant::new(3, 256, pixels);
    let palette: Vec<u8> = quant.color_map_rgb().into_iter().collect();
    #[allow(clippy::cast_possible_truncation)]
    let indices: Vec<u8> = pixels
        .as_chunks::<4>()
        .0
        .iter()
        .map(|pixel| quant.index_of(pixel) as u8)
        .collect();
    let file = std::fs::File::create(path).map_err(|err| fail(&err))?;
    let mut encoder =
        png::Encoder::new(std::io::BufWriter::new(file), image.width(), image.height());
    encoder.set_color(png::ColorType::Indexed);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_palette(palette);
    encoder.set_compression(png::Compression::High);
    let mut writer = encoder.write_header().map_err(|err| fail(&err))?;
    writer
        .write_image_data(&indices)
        .map_err(|err| fail(&err))?;
    writer.finish().map_err(|err| fail(&err))?;
    let bytes = std::fs::metadata(path).map_or(0, |m| m.len());
    eprintln!(
        "wrote {} ({}×{}, {} KB)",
        path.display(),
        image.width(),
        image.height(),
        bytes / 1024
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
/// (it chooses a take).
async fn shoot(demo: &Demo, out: &Path, cx: &mut AsyncApp) -> Result<(), String> {
    setup_check(demo, out, cx).await?;
    audio_panel(demo, out, cx).await?;
    recording(demo, out, cx).await?;
    take_list(demo, out, cx).await
}

/// The setup check's voice and room steps with their results: the panel
/// of each, one below the other.
async fn setup_check(demo: &Demo, out: &Path, cx: &mut AsyncApp) -> Result<(), String> {
    let shot = Shot::open(&demo.dir, cx).await?;
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

    // The setup panel sits between the film and the side column.
    let x = WINDOW.0 - 300.0 - 380.0;
    let image = stack(&[
        crop(&voice, (x, 0.0, 380.0, 236.0)),
        crop(&room, (x, 0.0, 380.0, 168.0)),
    ]);
    write_png(&out.join("setup-check.png"), &image)
}

/// The audio panel, the meter lit red by a peak near clipping.
async fn audio_panel(demo: &Demo, out: &Path, cx: &mut AsyncApp) -> Result<(), String> {
    let shot = Shot::open(&demo.dir, cx).await?;
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
    write_png(&out.join("audio-panel.png"), &image)
}

/// The hero: a fifth take of the capsule line being recorded, the line
/// filling over the film's frame. It records into a copy of the project,
/// so the take list after it stays at four takes.
async fn recording(demo: &Demo, out: &Path, cx: &mut AsyncApp) -> Result<(), String> {
    let copy = demo.dir.with_file_name("recording.takewise");
    copy_dir(&demo.dir, &copy)?;
    let shot = Shot::open(&copy, cx).await?;
    shot.update(cx, |view| select_line(view, "capsule"));
    let start = shot.media.now();
    shot.run_until(cx, start + 0.5).await;
    shot.update(cx, RecordingWindow::start_take);
    let line_start = 22.5;
    let speech_at = line_start + LEAD_S;
    let shoot_at = line_start + 1.45;
    let mut speaking = false;
    for _ in 0..400 {
        let now = shot.media.now();
        shot.run_until(cx, now + 0.05).await;
        let film_s = cx.update(|cx| {
            let view = shot.view.read(cx);
            view.recorder.playhead_s(&*view.media)
        });
        let Some(film_s) = film_s else {
            continue;
        };
        if !speaking && film_s >= speech_at {
            shot.media.hear(demo.voice("capsule"));
            speaking = true;
        }
        if film_s >= shoot_at {
            break;
        }
    }
    let image = shot.render(cx).await?;
    shot.close(cx);
    write_png(&out.join("recording.png"), &image)
}

/// The capsule line's takes after the narrator chose the proposed one.
async fn take_list(demo: &Demo, out: &Path, cx: &mut AsyncApp) -> Result<(), String> {
    tw::choose_take(&demo.dir, "capsule", 4, "narrator").map_err(|err| err.to_string())?;
    let shot = Shot::open(&demo.dir, cx).await?;
    shot.update(cx, |view| {
        select_line(view, "capsule");
        view.take = Some(1);
    });
    let start = shot.media.now();
    shot.run_until(cx, start + 0.5).await;
    let image = shot.render(cx).await?;
    shot.close(cx);
    write_png(
        &out.join("takes.png"),
        &crop(&image, (WINDOW.0 - 300.0, 392.0, 300.0, 230.0)),
    )
}
