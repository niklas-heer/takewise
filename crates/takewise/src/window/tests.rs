//! The window in a headless GPUI test window, driven by key presses, with
//! the media replaced by a fake whose host clock the test sets.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use gpui_kit::base::slider::{SliderEvent, SliderValue};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Entity, TestAppContext, VisualTestContext,
    Window, WindowBounds, WindowOptions, point, px, size,
};
use takewise_core::{
    self as tw, AvailableInput, EventKind, Field, FilmInfo, Project, RecordedInput, Timebase,
};
use takewise_media::{CaptureBuffer, DeviceInfo, DeviceLookup, Frame, OutputSource, Transport};

use super::checks::{Checker, Checks, SharedTranscriber, worker_tag};
use super::clock::FilmClock;
use super::levels::Level;
use super::{Media, RecordingWindow, bind_keys, load};

mod passages;
mod rescue;

const DEVICE: &str = "Test Mic";
const LATENCY_S: f64 = 0.05;

/// The fake's state, shared between the window and the test.
#[allow(clippy::struct_excessive_bools)]
struct Fake {
    now: f64,
    inputs: Vec<AvailableInput>,
    sink: Option<Sender<CaptureBuffer>>,
    /// Buffers sent to the current capture.
    sent: u64,
    capture_error: Option<String>,
    /// Every `film_play_at` call: (`film_s`, `host_s`).
    plays: Vec<(f64, f64)>,
    /// The player's timebase as read back; by default the last scheduled
    /// one once its start has passed, and stopped before.
    timebase: Option<Timebase>,
    /// Why `film_play_at` fails, when it does.
    play_error: Option<String>,
    outputs: Vec<DeviceInfo>,
    /// Outputs that are connected but missing from `outputs()`, as a
    /// device whose configuration cannot be read just then.
    hidden_outputs: Vec<String>,
    /// Every output set, in order (`None`: the system's default output),
    /// and how many were tried, also those that failed.
    output_sets: Vec<Option<String>>,
    output_attempts: usize,
    /// Why setting any output fails, when it does.
    output_error: Option<String>,
    /// The device generation: bumped whenever a test changes the outputs,
    /// as CoreAudio's listener would.
    generation: u64,
    /// Each output's CoreAudio object, by name (1 when not set): a new one
    /// when it is connected again.
    objects: std::collections::HashMap<String, u32>,
    /// The system's default output.
    default_output: String,
    /// Whether every device reads as absent (gone again just after it was
    /// found), or every read fails (CoreAudio not answering), and how many
    /// device lookups were made.
    objects_unreadable: bool,
    lookups_fail: bool,
    lookups: usize,
    /// How many times the output devices were listed (by name), and
    /// whether listing them fails (finds nothing).
    listings: usize,
    listing_fails: bool,
    /// Whether the default output's name and UID cannot be read.
    default_name_fails: bool,
    /// Whether there is no system output (none connected).
    no_default_output: bool,
    /// Whether the film is still loading.
    film_loading: bool,
    /// Whether an opened take is still loading.
    take_loading: bool,
    /// Why the film's or the take's player failed (as AVFoundation fails
    /// a playing item whose output went away), and why reopening the film
    /// fails, when it does.
    film_failed: Option<String>,
    take_failed: Option<String>,
    film_open_error: Option<String>,
    /// Where the film plays (`None`: the system default).
    film_output: Option<String>,
    /// The take open for playback, if one is.
    take: Option<OpenTake>,
    /// Why `set_output` fails while a take is open, when it does: it then
    /// changes nothing, as the real media.
    take_output_error: Option<String>,
    film_gain_db: Option<f64>,
    /// The device and channel of the last capture started.
    capture_input: Option<(String, u16)>,
    /// The device whose channels are metered, and their levels.
    channel_meter: Option<String>,
    levels: Vec<f32>,
    /// Why switching the sample rate fails, when it does.
    rate_error: Option<String>,
    mic: tw::MicPermission,
    /// Why monitoring cannot run; `None` once it can.
    monitor_unavailable: Option<String>,
    /// Every film opened after the first, in order.
    films: Vec<PathBuf>,
    muted: bool,
    /// The plays heard (not muted): (`film_s`, `host_s`).
    audible: Vec<(f64, f64)>,
    /// Host times of every pause.
    pauses: Vec<f64>,
    /// Every file opened for playback, in order.
    opened_takes: Vec<PathBuf>,
    /// Every `take_play_at` call: (`take_s`, `host_s`).
    take_plays: Vec<(f64, f64)>,
}

/// A take open in the fake, as the real media opens it.
#[derive(Debug, Clone, PartialEq)]
struct OpenTake {
    /// Where it plays (`None`: the system default): the output last set,
    /// when it was opened or since.
    output: Option<String>,
    /// Every level set on it, in order; it opens at full level.
    gains_db: Vec<f64>,
}

#[derive(Clone)]
struct FakeMedia(Rc<RefCell<Fake>>);

impl FakeMedia {
    fn new(inputs: &[(&str, u32)]) -> Self {
        Self(Rc::new(RefCell::new(Fake {
            now: 100.0,
            inputs: inputs
                .iter()
                .map(|(name, rate)| AvailableInput {
                    name: (*name).to_owned(),
                    sample_rate: *rate,
                })
                .collect(),
            sink: None,
            sent: 0,
            capture_error: None,
            plays: Vec::new(),
            timebase: None,
            play_error: None,
            outputs: vec![
                output("Test Phones", Transport::Usb, OutputSource::Unknown),
                output(
                    "MacBook Speakers",
                    Transport::BuiltIn,
                    OutputSource::Speakers,
                ),
                output(
                    "External Headphones",
                    Transport::BuiltIn,
                    OutputSource::Headphones,
                ),
                output("Built-in Output", Transport::BuiltIn, OutputSource::Unknown),
            ],
            hidden_outputs: Vec::new(),
            output_sets: Vec::new(),
            output_attempts: 0,
            output_error: None,
            generation: 0,
            objects: std::collections::HashMap::new(),
            default_output: "MacBook Speakers".to_owned(),
            objects_unreadable: false,
            lookups_fail: false,
            lookups: 0,
            listings: 0,
            listing_fails: false,
            default_name_fails: false,
            no_default_output: false,
            film_loading: false,
            take_loading: false,
            film_failed: None,
            take_failed: None,
            film_open_error: None,
            film_output: None,
            take: None,
            take_output_error: None,
            film_gain_db: None,
            capture_input: None,
            channel_meter: None,
            levels: vec![-40.0, f32::NEG_INFINITY],
            rate_error: None,
            mic: tw::MicPermission::Granted,
            monitor_unavailable: Some(
                "not available yet: monitoring needs a test on your setup".to_owned(),
            ),
            films: Vec::new(),
            muted: false,
            audible: Vec::new(),
            pauses: Vec::new(),
            opened_takes: Vec::new(),
            take_plays: Vec::new(),
        })))
    }

    /// Delivers a buffer whose every sample is `value`.
    fn send_value(&self, host_s: f64, len: usize, value: i32) {
        let mut fake = self.0.borrow_mut();
        let sink = fake.sink.as_ref().expect("a capture is running");
        sink.send(CaptureBuffer {
            host_time_s: host_s,
            samples: vec![value; len],
        })
        .expect("the window receives");
        fake.sent += 1;
    }

    fn set_now(&self, now: f64) {
        self.0.borrow_mut().now = now;
    }

    /// Delivers a buffer of `len` samples whose first sample has callback
    /// host time `host_s`, as the capture would.
    fn send(&self, host_s: f64, len: usize) {
        let mut fake = self.0.borrow_mut();
        let sink = fake.sink.as_ref().expect("a capture is running");
        sink.send(CaptureBuffer {
            host_time_s: host_s,
            samples: vec![100_000; len],
        })
        .expect("the window receives");
        fake.sent += 1;
    }

    fn sent(&self) -> u64 {
        self.0.borrow().sent
    }

    fn capturing(&self) -> bool {
        self.0.borrow().sink.is_some()
    }
}

fn output(name: &str, transport: Transport, output_source: OutputSource) -> DeviceInfo {
    DeviceInfo {
        name: name.to_owned(),
        sample_rate: 48_000,
        channels: 2,
        transport,
        output_source,
    }
}

impl Media for FakeMedia {
    fn host_now_s(&self) -> f64 {
        self.0.borrow().now
    }
    fn film_ready(&self) -> bool {
        let fake = self.0.borrow();
        fake.film_failed.is_none() && !fake.film_loading
    }
    fn film_failure(&self) -> Option<String> {
        self.0.borrow().film_failed.clone()
    }
    fn take_failure(&self) -> Option<String> {
        self.0.borrow().take_failed.clone()
    }
    fn film_play_at(&mut self, film_s: f64, host_s: f64) -> Result<Timebase, String> {
        if let Some(err) = self.0.borrow().play_error.clone() {
            return Err(err);
        }
        let mut fake = self.0.borrow_mut();
        fake.plays.push((film_s, host_s));
        if !fake.muted {
            fake.audible.push((film_s, host_s));
        }
        drop(fake);
        Ok(Timebase {
            rate: 1.0,
            film_anchor_s: film_s,
            host_anchor_s: host_s,
        })
    }
    fn film_timebase(&self) -> Timebase {
        let fake = self.0.borrow();
        if let Some(tb) = fake.timebase {
            return tb;
        }
        let (film_s, host_s) = fake.plays.last().copied().unwrap_or((0.0, 0.0));
        // A failed player's clock stops, as AVFoundation's does.
        let running = fake.now >= host_s && fake.film_failed.is_none();
        Timebase {
            rate: if running { 1.0 } else { 0.0 },
            film_anchor_s: film_s,
            host_anchor_s: host_s,
        }
    }
    fn film_open(&mut self, path: &Path) -> Result<(), String> {
        let mut fake = self.0.borrow_mut();
        if let Some(err) = fake.film_open_error.clone() {
            return Err(err);
        }
        fake.films.push(path.to_owned());
        // A new player: not failed.
        fake.film_failed = None;
        Ok(())
    }
    fn film_pause(&mut self) {
        let mut fake = self.0.borrow_mut();
        let now = fake.now;
        fake.pauses.push(now);
    }
    fn film_seek(&mut self, _: f64) {}
    fn film_set_gain_db(&mut self, db: f64) {
        self.0.borrow_mut().film_gain_db = Some(db);
    }
    fn film_set_muted(&mut self, muted: bool) {
        self.0.borrow_mut().muted = muted;
    }
    fn latest_frame(&mut self) -> Option<Frame> {
        None
    }
    fn inputs(&self) -> Vec<AvailableInput> {
        self.0.borrow().inputs.clone()
    }
    fn input_latency_s(&self, _: &str) -> Result<f64, String> {
        Ok(LATENCY_S)
    }
    fn start_capture(
        &mut self,
        device: &str,
        channel: u16,
        sink: Sender<CaptureBuffer>,
    ) -> Result<(), String> {
        let mut fake = self.0.borrow_mut();
        if !fake.inputs.iter().any(|input| input.name == device) {
            return Err(format!("no input device named {device:?}"));
        }
        fake.capture_input = Some((device.to_owned(), channel));
        fake.sink = Some(sink);
        fake.sent = 0;
        fake.capture_error = None;
        Ok(())
    }
    fn stop_capture(&mut self) {
        self.0.borrow_mut().sink = None;
    }
    fn capture_error(&self) -> Option<String> {
        self.0.borrow().capture_error.clone()
    }
    fn open_take(&mut self, path: &Path) -> Result<(), String> {
        // On the device the film was last set to, as the real media: it is
        // not looked up again. A new player: not failed.
        let mut fake = self.0.borrow_mut();
        fake.take_failed = None;
        let output = fake.film_output.clone();
        fake.opened_takes.push(path.to_owned());
        fake.take = Some(OpenTake {
            output,
            gains_db: Vec::new(),
        });
        Ok(())
    }
    fn take_ready(&self) -> bool {
        let fake = self.0.borrow();
        !fake.take_loading && fake.take_failed.is_none()
    }
    fn take_play_at(&mut self, take_s: f64, host_s: f64) -> Result<(), String> {
        self.0.borrow_mut().take_plays.push((take_s, host_s));
        Ok(())
    }
    fn take_set_gain_db(&mut self, db: f64) {
        if let Some(take) = &mut self.0.borrow_mut().take {
            take.gains_db.push(db);
        }
    }
    fn stop_take(&mut self) {}
    fn outputs(&self) -> Vec<DeviceInfo> {
        self.0.borrow().outputs.clone()
    }
    fn default_output(&self) -> Option<String> {
        Some(self.0.borrow().default_output.clone())
    }
    fn device_generation(&self) -> u64 {
        self.0.borrow().generation
    }
    fn applied_output_uid(&self) -> Option<String> {
        self.0
            .borrow()
            .film_output
            .as_ref()
            .map(|name| format!("uid:{name}"))
    }
    fn output_object(&self, uid: &str) -> DeviceLookup {
        let Some(name) = uid.strip_prefix("uid:") else {
            return DeviceLookup::Absent;
        };
        let mut fake = self.0.borrow_mut();
        fake.lookups += 1;
        if fake.lookups_fail {
            return DeviceLookup::Unknown;
        }
        if fake.objects_unreadable {
            return DeviceLookup::Absent;
        }
        let connected = fake.outputs.iter().any(|output| output.name == name)
            || fake.hidden_outputs.iter().any(|hidden| hidden == name);
        if connected {
            DeviceLookup::Present(fake.objects.get(name).copied().unwrap_or(1))
        } else {
            DeviceLookup::Absent
        }
    }
    fn default_output_object(&self) -> DeviceLookup {
        let fake = self.0.borrow();
        if fake.lookups_fail {
            return DeviceLookup::Unknown;
        }
        if fake.no_default_output {
            return DeviceLookup::Absent;
        }
        // Another object for another device, and for the same one plugged
        // in again.
        let name = &fake.default_output;
        let base: u32 = name.bytes().map(u32::from).sum();
        DeviceLookup::Present(base * 1000 + fake.objects.get(name).copied().unwrap_or(1))
    }
    fn default_output_uid(&self) -> Option<(String, String)> {
        self.0.borrow_mut().listings += 1;
        if self.0.borrow().default_name_fails {
            return None;
        }
        let name = self.0.borrow().default_output.clone();
        Some((name.clone(), format!("uid:{name}")))
    }
    fn output_connected(&self, device: &str) -> bool {
        self.0.borrow_mut().listings += 1;
        let fake = self.0.borrow();
        if fake.listing_fails {
            return false;
        }
        fake.outputs.iter().any(|output| output.name == device)
            || fake.hidden_outputs.iter().any(|name| name == device)
    }
    fn set_output(&mut self, device: Option<&str>) -> Result<(), String> {
        self.0.borrow_mut().output_attempts += 1;
        if let Some(device) = device
            && !self.output_connected(device)
        {
            return Err(format!("no output device named {device:?}"));
        }
        let mut fake = self.0.borrow_mut();
        if let Some(err) = fake.output_error.clone() {
            return Err(err);
        }
        if fake.take.is_some()
            && let Some(err) = fake.take_output_error.clone()
        {
            return Err(err);
        }
        fake.film_output = device.map(str::to_owned);
        fake.output_sets.push(device.map(str::to_owned));
        if let Some(take) = &mut fake.take {
            take.output = device.map(str::to_owned);
        }
        Ok(())
    }
    fn start_channel_meter(&mut self, device: &str) -> Result<(), String> {
        self.0.borrow_mut().channel_meter = Some(device.to_owned());
        Ok(())
    }
    fn stop_channel_meter(&mut self) {
        self.0.borrow_mut().channel_meter = None;
    }
    fn channel_levels(&self) -> Vec<f32> {
        let fake = self.0.borrow();
        if fake.channel_meter.is_some() {
            fake.levels.clone()
        } else {
            Vec::new()
        }
    }
    fn set_nominal_rate(&mut self, device: &str, rate: u32) -> Result<(), String> {
        let mut fake = self.0.borrow_mut();
        if let Some(err) = fake.rate_error.clone() {
            return Err(err);
        }
        for input in fake.inputs.iter_mut().filter(|i| i.name == device) {
            input.sample_rate = rate;
        }
        Ok(())
    }
    fn buffer_frames(&self, _: &str) -> Option<u32> {
        Some(512)
    }
    fn monitor_unavailable(&self) -> Option<String> {
        self.0.borrow().monitor_unavailable.clone()
    }
    fn microphone_permission(&self) -> tw::MicPermission {
        self.0.borrow().mic
    }
}

/// Lines close together, so the pre-roll of `b` holds the first frame.
const SCRIPT: &str = r#"# test script
[[line]]
id = "a"
start = 0.0
text = "First line."

[[line]]
id = "b"
start = 1.0
text = "Second line."

[[line]]
id = "c"
start = 1.5
text = "Third line."

[[line]]
id = "d"
start = 8.0
text = "Fourth line."
"#;

struct Fixture {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    window: AnyWindowHandle,
    view: Entity<RecordingWindow>,
    media: FakeMedia,
}

fn project(device: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("temp dir");
    let dir = tmp.path().join("test.takewise");
    let film = tmp.path().join("film.mp4");
    fs::write(&film, b"").expect("film");
    let info = FilmInfo {
        duration_s: 20.0,
        size: 0,
        modified: "2026-11-22T00:00:00Z".to_owned(),
    };
    Project::init(&dir, &film, info).expect("init");
    fs::write(dir.join("script.toml"), SCRIPT).expect("script");
    let config = dir.join("project.toml");
    let text = fs::read_to_string(&config).expect("project.toml");
    let text = text.replacen("[input]\n", &format!("[input]\ndevice = {device:?}\n"), 1);
    // The take timings below were written for a 2 s pre-roll, which
    // projects made before 0.1.2 keep.
    let text = text.replacen("preroll_s = 3.0\n", "preroll_s = 2.0\n", 1);
    fs::write(&config, text).expect("project.toml");
    (tmp, dir)
}

/// Sets `[record] preroll_s` of the project in `dir`.
fn set_preroll(dir: &Path, preroll_s: f64) {
    let config = dir.join("project.toml");
    let text = fs::read_to_string(&config).expect("project.toml");
    let text = text.replacen(
        "preroll_s = 2.0\n",
        &format!("preroll_s = {preroll_s:?}\n"),
        1,
    );
    fs::write(&config, text).expect("project.toml");
}

fn open(cx: &mut TestAppContext, inputs: &[(&str, u32)]) -> Fixture {
    open_with(cx, inputs, |_| {})
}

/// Takes are checked without words in tests unless a test says otherwise.
fn no_transcriber() -> SharedTranscriber {
    Arc::new(tw::Unavailable {
        reason: "no transcriber in tests".to_owned(),
        model: "ggml-test.bin".to_owned(),
    })
}

/// Opens the window after `prepare` ran on the new project.
fn open_with(
    cx: &mut TestAppContext,
    inputs: &[(&str, u32)],
    prepare: impl FnOnce(&Path),
) -> Fixture {
    open_checked(cx, inputs, prepare, no_transcriber())
}

/// [`open_with`] with the transcriber the window's checks use.
fn open_checked(
    cx: &mut TestAppContext,
    inputs: &[(&str, u32)],
    prepare: impl FnOnce(&Path),
    transcriber: SharedTranscriber,
) -> Fixture {
    open_tooled(cx, inputs, prepare, transcriber, Vec::new(), worker_tag())
}

/// [`open_checked`] with the executables the transcriber runs as child
/// processes.
fn open_tooled(
    cx: &mut TestAppContext,
    inputs: &[(&str, u32)],
    prepare: impl FnOnce(&Path),
    transcriber: SharedTranscriber,
    tools: Vec<PathBuf>,
    tag: String,
) -> Fixture {
    super::start::isolate_settings();
    let (tmp, dir) = project(DEVICE);
    prepare(&dir);
    let media = FakeMedia::new(inputs);
    let loaded = load(&dir).expect("load");
    let window_dir = dir.clone();
    let window_media = media.clone();
    let (window, view) = cx.update(|cx| {
        gpui_kit::init(cx);
        bind_keys(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(0.), px(0.)),
                size: size(px(1200.), px(800.)),
            })),
            ..WindowOptions::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| {
                RecordingWindow::new(
                    window_dir,
                    loaded,
                    Box::new(window_media),
                    None,
                    Checker {
                        transcriber,
                        tools,
                        tag,
                    },
                    window,
                    cx,
                )
            })
        })
        .expect("open test window")
    });
    Fixture {
        _tmp: tmp,
        dir,
        window,
        view,
        media,
    }
}

/// Renders a frame (the window's tick), then runs `f` with the window.
fn act(cx: &mut TestAppContext, window: AnyWindowHandle, f: impl FnOnce(&mut Window, &mut App)) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        f(window, cx);
    })
    .expect("window is open");
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, keys: &[&str]) {
    act(cx, window, |window, cx| {
        for key in keys {
            window.press(key, cx);
        }
    });
}

fn frame(cx: &mut TestAppContext, window: AnyWindowHandle) {
    act(cx, window, |_, _| {});
}

fn recorded(dir: &Path) -> Vec<(String, u32, tw::Recorded)> {
    tw::read(dir)
        .expect("ledger")
        .events
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Recorded(recorded) => Some((event.line, event.take, recorded)),
            _ => None,
        })
        .collect()
}

fn start_of(dir: &Path, id: &str) -> f64 {
    let project = Project::open(dir).expect("project");
    let line = project.script.lines.iter().find(|line| line.id == id);
    line.expect("line").start
}

/// Waits (on the real clock) until `done` holds for the window, such as
/// the take writer having received every buffer sent.
fn wait_for(
    cx: &mut TestAppContext,
    view: &Entity<RecordingWindow>,
    done: impl Fn(&RecordingWindow) -> bool,
) {
    for _ in 0..2000 {
        if view.read_with(cx, |view, _| done(view)) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("timed out waiting for the window");
}

/// Waits until the take writer received every buffer sent.
fn settle(cx: &mut TestAppContext, f: &Fixture) {
    let sent = f.media.sent();
    wait_for(cx, &f.view, |view| {
        view.recorder.progress().is_some_and(|p| p.buffers >= sent)
    });
}

/// The take's partial WAV while it is recorded.
fn partial_wav(dir: &Path, line: &str) -> Option<PathBuf> {
    fs::read_dir(dir.join("takes").join(line))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.to_string_lossy().ends_with(".wav.partial"))
}

fn message(cx: &mut TestAppContext, view: &Entity<RecordingWindow>) -> String {
    view.read_with(cx, |view, _| view.message.clone().unwrap_or_default())
}

/// Starts a take of the line selected now: Space at host `at`, a first
/// buffer of 50 ms stamped `at + 0.05` (its first sample was heard at host
/// `at`, after the 50 ms latency), and the frame at `at + 0.1` that sees it
/// and schedules the film to start at `at + 0.4` (plus any held first frame).
fn begin(cx: &mut TestAppContext, f: &Fixture, at: f64) {
    f.media.set_now(at);
    press(cx, f.window, &["space"]);
    f.media.send(at + 0.05, 2400);
    settle(cx, f);
    f.media.set_now(at + 0.1);
    frame(cx, f.window);
}

/// Records one take of the line selected now: begun at host 100, stopped
/// with Space at host 100.5; 2400 samples.
fn record_take(cx: &mut TestAppContext, f: &Fixture) {
    begin(cx, f, 100.0);
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
}

fn last_play(f: &Fixture) -> (f64, f64) {
    f.media.0.borrow().plays.last().copied().expect("a play")
}

#[gpui_kit::test]
fn space_records_and_commits_a_take(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d: start 8.0, pre-roll from 6.0
    begin(cx, &f, 100.0);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    // The film is scheduled from 6.0 at host 100.4, 0.3 s after the frame
    // that saw the first buffer.
    let (film_s, host_s) = last_play(&f);
    assert!(
        close(film_s, 6.0) && close(host_s, 100.4),
        "{film_s} {host_s}"
    );
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);

    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 1, "one recorded event");
    let (line, take, event) = &takes[0];
    assert_eq!((line.as_str(), *take), ("d", 1));
    // First sample: host 100.0 = film 5.6, 2.4 s before the line.
    assert!(close(event.line_offset_s, -2.4), "{}", event.line_offset_s);
    assert_eq!(event.samples, 2400);
    assert_eq!(event.device, DEVICE);
    assert_eq!(event.snapshot.text, "Fourth line.");
    assert!(f.dir.join("takes/d/001.wav").is_file());
    f.view.read_with(cx, |view, _| {
        assert!(!view.recorder.is_recording());
        assert_eq!(view.take, Some(1), "the new take is selected");
    });
}

#[gpui_kit::test]
fn first_line_take_starts_before_film_zero(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    // a: start 0.0; the film holds frame 0 for the whole 2 s pre-roll.
    record_take(cx, &f);
    let (film_s, host_s) = last_play(&f);
    assert!(
        close(film_s, 0.0) && close(host_s, 102.4),
        "{film_s} {host_s}"
    );
    let takes = recorded(&f.dir);
    // The first sample, at host 100.0, is 2.4 s before film 0 at 102.4.
    assert!(
        close(takes[0].2.line_offset_s, -2.4),
        "{}",
        takes[0].2.line_offset_s
    );
}

#[gpui_kit::test]
fn take_stops_itself_after_the_tail(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down"]); // c: 1.5 .. 8.0, tail 1.5 => stop at film 9.5
    begin(cx, &f, 100.0);
    // Film 0 at host 100.9 (hold 0.5 s), so film 9.5 at host 110.4.
    f.media.set_now(110.35);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    f.media.set_now(110.45);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(recorded(&f.dir).len(), 1);
}

/// The film's bounds in the window's last frame.
fn film_bounds(cx: &mut TestAppContext, f: &Fixture) -> Bounds<gpui_kit::Pixels> {
    frame(cx, f.window);
    VisualTestContext::from_window(f.window, cx)
        .debug_bounds("film")
        .expect("the film is drawn")
}

#[gpui_kit::test]
fn film_keeps_its_size_when_a_message_appears(cx: &mut TestAppContext) {
    // A line of three rows with a note fills the prompter, so a new row
    // below it would take its height from the film.
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        let text = "A first line long enough to wrap onto three rows of the prompter, \
            so that the prompter is taller than its least height and every row \
            under it counts.";
        tw::set_line_field(dir, "a", Field::Text(text.to_owned()), None).expect("text");
        tw::set_line_field(dir, "a", Field::Note(Some("Slowly.".to_owned())), None).expect("note");
    });
    let before = film_bounds(cx, &f);
    record_take(cx, &f);
    wait_for(cx, &f.view, |view| view.message.is_some());
    let after = film_bounds(cx, &f);
    let shown = message(cx, &f.view);
    assert!(shown.starts_with("Recorded a take 1"), "{shown}");
    assert_eq!(before, after, "the message row is kept while empty");
}

#[gpui_kit::test]
fn enter_chooses_selected_take(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    record_take(cx, &f);
    record_take(cx, &f);
    // The newest take is selected; ← selects take 1.
    press(cx, f.window, &["left", "enter"]);
    let ledger = tw::read(&f.dir).expect("ledger");
    let chosen: Vec<_> = ledger
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Chosen(chosen) => Some((event.line.clone(), event.take, chosen.by.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(chosen, vec![("a".to_owned(), 1, "narrator".to_owned())]);
}

#[gpui_kit::test]
fn mark_start_clamps(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down"]); // b: start 1.0, between a (0.0) and c (1.5)
    begin(cx, &f, 100.0);
    // During the count-in the playhead is before film zero; M clamps to just
    // after the previous line's start.
    press(cx, f.window, &["m"]);
    assert_eq!(start_of(&f.dir, "b"), 0.001);
    // Film 0 is at host 101.4; at host 104.25 the playhead (2.85) is past the
    // slot's end (c's start), so M clamps to just before it.
    f.media.set_now(104.25);
    press(cx, f.window, &["m"]);
    assert_eq!(start_of(&f.dir, "b"), 1.499);
    press(cx, f.window, &["space"]);
}

#[gpui_kit::test]
fn mark_start_needs_the_film_playing(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["m"]);
    assert_eq!(start_of(&f.dir, "a"), 0.0);
    assert!(message(cx, &f.view).contains("while the film plays"));
}

#[gpui_kit::test]
fn stale_edit_is_refused_and_reported(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    // An agent edits the script while the window is open (no watcher here,
    // so the window still holds the old version).
    tw::set_line_field(&f.dir, "c", Field::Text("Agent text.".to_owned()), None).expect("edit");
    press(cx, f.window, &["down"]);
    begin(cx, &f, 100.0); // b: film 0 at host 101.4
    f.media.set_now(101.75);
    press(cx, f.window, &["m"]);

    assert_eq!(start_of(&f.dir, "b"), 1.0, "the stale edit is not written");
    let script = Project::open(&f.dir).expect("project").script;
    assert_eq!(
        script.lines[2].text, "Agent text.",
        "the agent's edit stays"
    );
    let shown = message(cx, &f.view);
    assert!(shown.contains("changed on disk"), "{shown}");
    // The window reloaded, so the next M is made against the new version.
    f.view.read_with(cx, |view, _| {
        assert_eq!(view.project.script.lines[2].text, "Agent text.");
    });
    press(cx, f.window, &["m"]);
    assert_eq!(start_of(&f.dir, "b"), 0.35);
    press(cx, f.window, &["space"]);
}

#[gpui_kit::test]
fn edit_replaces_the_line_text(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["e", "cmd-a"]);
    act(cx, f.window, |window, cx| window.input("New words.", cx));
    press(cx, f.window, &["enter"]);
    let script = Project::open(&f.dir).expect("project").script;
    assert_eq!(script.lines[0].text, "New words.");
    assert!(f.view.read_with(cx, |view, _| view.editing.is_none()));
    // The recording keys work again.
    press(cx, f.window, &["down"]);
    assert_eq!(f.view.read_with(cx, |view, _| view.line), 1);
}

#[gpui_kit::test]
fn close_mid_take_commits(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    begin(cx, &f, 100.0);
    f.media.send(100.1, 4800);
    assert!(VisualTestContext::from_window(f.window, cx).simulate_close());
    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 1, "the partial take is kept");
    assert_eq!(takes[0].2.samples, 7200);
}

#[gpui_kit::test]
fn close_before_any_audio_leaves_nothing(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    assert!(VisualTestContext::from_window(f.window, cx).simulate_close());
    assert_eq!(recorded(&f.dir).len(), 0);
    assert!(!f.dir.join("takes/a").exists());
}

#[gpui_kit::test]
fn blocked_device_ignores_space(cx: &mut TestAppContext) {
    let f = open(cx, &[("Bluetooth Headset", 16_000), ("Other Mic", 48_000)]);
    frame(cx, f.window);
    let blocked = f
        .view
        .read_with(cx, |view, _| view.blocked().map(str::to_owned))
        .expect("recording is blocked");
    assert!(blocked.contains(DEVICE), "{blocked}");
    assert!(blocked.contains("Other Mic"), "{blocked}");
    assert!(!f.media.capturing(), "no meter on a missing device");

    press(cx, f.window, &["space"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert!(!f.media.capturing());
    assert_eq!(recorded(&f.dir).len(), 0);
}

#[gpui_kit::test]
fn device_at_the_wrong_rate_blocks(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 16_000)]);
    let blocked = f
        .view
        .read_with(cx, |view, _| view.blocked().map(str::to_owned))
        .expect("recording is blocked");
    assert!(blocked.contains("16000 Hz"), "{blocked}");
    press(cx, f.window, &["space"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
}

#[gpui_kit::test]
fn play_in_context_locks_the_take_to_the_film(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d
    record_take(cx, &f);
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window); // the take has loaded: both start
    let (film_s, host_s) = last_play(&f);
    assert!(
        close(film_s, 6.0) && close(host_s, 200.3),
        "{film_s} {host_s}"
    );
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    press(cx, f.window, &["p"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
}

#[gpui_kit::test]
fn an_output_the_take_cannot_use_changes_nothing_and_stops_playback(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    f.view
        .update(cx, |view, _| view.choose_output("Test Phones"));
    press(cx, f.window, &["down", "down", "down"]); // d
    record_take(cx, &f);
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));

    f.media.0.borrow_mut().take_output_error = Some("the take refused".to_owned());
    f.view
        .update(cx, |view, _| view.choose_output("External Headphones"));
    let said = message(cx, &f.view);
    assert!(
        said.contains(
            "Saved External Headphones as the output. Cannot play through External Headphones: the take refused. Playback stopped."
        ),
        "{said}"
    );
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    let fake = f.media.0.borrow();
    assert_eq!(
        fake.film_output.as_deref(),
        Some("Test Phones"),
        "the film stays"
    );
    assert_eq!(
        fake.take.as_ref().and_then(|take| take.output.as_deref()),
        Some("Test Phones"),
        "and the take with it"
    );
    drop(fake);
    assert_eq!(
        f.view
            .read_with(cx, |view, _| view.film_output.clone())
            .as_deref(),
        Some("Test Phones")
    );
    assert_eq!(
        f.view
            .read_with(cx, |view, _| view.output_note())
            .as_deref(),
        Some("External Headphones could not be used: the take refused"),
        "connected, but not in use"
    );
}

#[gpui_kit::test]
fn takes_play_through_the_films_output(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    let take_output = |f: &Fixture| {
        let fake = f.media.0.borrow();
        fake.take.as_ref().map(|take| take.output.clone())
    };
    f.view
        .update(cx, |view, _| view.choose_output("Test Phones"));
    assert!(
        message(cx, &f.view).contains("The film and your takes play through Test Phones."),
        "{}",
        message(cx, &f.view)
    );
    press(cx, f.window, &["down", "down", "down"]); // d
    record_take(cx, &f);
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(
        take_output(&f),
        Some(Some("Test Phones".to_owned())),
        "the take plays where the film does, not through the system output"
    );

    // Another output while the take plays: the take moves with the film.
    f.view
        .update(cx, |view, _| view.choose_output("External Headphones"));
    assert_eq!(
        f.media.0.borrow().film_output.as_deref(),
        Some("External Headphones")
    );
    assert_eq!(
        take_output(&f),
        Some(Some("External Headphones".to_owned()))
    );
    press(cx, f.window, &["p"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));

    // The output is unplugged: P says so and plays nothing elsewhere.
    f.media
        .0
        .borrow_mut()
        .outputs
        .retain(|o| o.name != "External Headphones");
    let opened = f.media.0.borrow().opened_takes.len();
    f.media.set_now(300.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    let said = message(cx, &f.view);
    assert_eq!(
        said,
        "External Headphones is not connected: connect it or pick another output (A)."
    );
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(
        f.media.0.borrow().opened_takes.len(),
        opened,
        "no take opened"
    );
}

/// Commits a take of `line` from outside the window, as an agent's
/// `takewise` could.
fn commit_outside(dir: &Path, line: &str) {
    let snapshot = Project::open(dir)
        .expect("project")
        .snapshot(line)
        .expect("snapshot");
    let input = RecordedInput {
        sample_rate: 48_000,
        device: DEVICE.to_owned(),
        channel: 1,
        line_offset_s: -2.0,
        snapshot,
        dropouts: None,
        film_shift_ms: None,
        passage: None,
    };
    let partial = tw::begin_take(dir, line, &input).expect("begin");
    tw::write_take_wav(&partial.wav, &[0; 480], 48_000).expect("wav");
    tw::commit_take(dir, partial).expect("commit");
}

const PHONES: &str = "WH-1000XM5";

#[gpui_kit::test]
fn a_missing_output_refuses_playback_until_it_is_connected(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        tw::set_route(dir, None, Some(PHONES)).expect("output");
        commit_outside(dir, "d");
    });
    let missing = format!("{PHONES} is not connected: connect it or pick another output (A).");
    assert_eq!(message(cx, &f.view), missing, "said when the window opens");
    f.view.read_with(cx, |view, _| {
        assert_eq!(
            view.output_note().map(|n| n.starts_with(PHONES)),
            Some(true)
        );
    });

    // P and Space play nothing on the system output (Shift+P goes the
    // same way, see the passage tests); a take says why it matters.
    press(cx, f.window, &["down", "down", "down"]); // d
    let not_recording = format!(
        "{PHONES} is not connected, so Takewise does not record: the film would play out loud into your microphone. Connect it, or pick another output, or System output, in the audio panel (A)."
    );
    for (key, said) in [
        ("p", &missing),
        ("space", &not_recording),
        ("r", &not_recording),
    ] {
        f.view.update(cx, |view, _| view.message = None);
        press(cx, f.window, &[key]);
        assert_eq!(&message(cx, &f.view), said, "{key}");
        f.view.read_with(cx, |view, _| {
            assert!(!view.recorder.is_playing() && !view.recorder.is_recording());
        });
    }
    assert_eq!(f.media.0.borrow().opened_takes.len(), 0, "no take opened");
    assert_eq!(f.media.0.borrow().film_output, None);

    // Plugged in later: the next device check moves the film there, and P
    // plays through it.
    f.media.0.borrow_mut().outputs.push(output(
        PHONES,
        Transport::Bluetooth,
        OutputSource::Headphones,
    ));
    f.media.set_now(103.0);
    frame(cx, f.window);
    assert_eq!(f.media.0.borrow().film_output.as_deref(), Some(PHONES));
    assert_eq!(
        message(cx, &f.view),
        format!("{PHONES} is connected: the film and your takes play there.")
    );
    assert_eq!(f.view.read_with(cx, |view, _| view.output_note()), None);
    // Back again after another drop: said over a passing status, but
    // never over a take's result (see `a_reconnect_does_not_hide_a_take_warning`).
    unplug(&f, PHONES);
    f.media.set_now(106.0);
    frame(cx, f.window);
    f.view.update(cx, |view, _| {
        view.message = Some("Chose d take 1.".to_owned());
    });
    f.media.0.borrow_mut().outputs.push(output(
        PHONES,
        Transport::Bluetooth,
        OutputSource::Headphones,
    ));
    f.media.set_now(109.0);
    frame(cx, f.window);
    assert_eq!(
        f.media.0.borrow().output_sets.last(),
        Some(&Some(PHONES.to_owned()))
    );
    assert_eq!(
        message(cx, &f.view),
        format!("{PHONES} is connected: the film and your takes play there.")
    );
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(
        f.media
            .0
            .borrow()
            .take
            .as_ref()
            .and_then(|t| t.output.clone()),
        Some(PHONES.to_owned())
    );

    // Gone while it plays: the playback stops at the next check.
    f.media.0.borrow_mut().outputs.retain(|o| o.name != PHONES);
    f.media.set_now(203.0);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(
        message(cx, &f.view),
        format!("The playback stopped. {missing}")
    );
}

/// The window on a project whose output is Test Phones, with a take of
/// line d selected.
fn on_test_phones(cx: &mut TestAppContext) -> Fixture {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        tw::set_route(dir, None, Some("Test Phones")).expect("output");
        commit_outside(dir, "d");
    });
    press(cx, f.window, &["down", "down", "down"]); // d
    assert_eq!(
        f.media.0.borrow().film_output.as_deref(),
        Some("Test Phones")
    );
    f
}

/// The output `name` goes away, and CoreAudio says the devices changed.
fn unplug(f: &Fixture, name: &str) {
    let mut fake = f.media.0.borrow_mut();
    fake.outputs.retain(|o| o.name != name);
    fake.generation += 1;
}

/// The output `name` is connected (a new CoreAudio object), and CoreAudio
/// says the devices changed.
fn plug(f: &Fixture, name: &str) {
    let mut fake = f.media.0.borrow_mut();
    fake.outputs
        .push(output(name, Transport::Usb, OutputSource::Unknown));
    let object = fake.objects.get(name).copied().unwrap_or(1) + 1;
    fake.objects.insert(name.to_owned(), object);
    fake.generation += 1;
}

/// Removes `[output] device`: the system output, by choice.
fn clear_output_on_disk(cx: &mut TestAppContext, f: &Fixture) {
    let path = f.dir.join("project.toml");
    let text = fs::read_to_string(&path).expect("project.toml");
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| !line.starts_with("device = \"Test Phones\""))
        .collect();
    fs::write(&path, kept.join("\n")).expect("project.toml");
    f.view.update(cx, |view, _| view.reload());
    assert_eq!(config(&f.dir).output.device, None);
}

#[gpui_kit::test]
fn a_lost_output_keeps_the_players_until_the_route_changes(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    unplug(&f, "Test Phones");
    f.media.set_now(103.0);
    frame(cx, f.window);
    f.view.read_with(cx, |view, _| {
        assert_eq!(
            view.film_output.as_deref(),
            Some("Test Phones"),
            "still applied"
        );
        assert!(
            view.output_note()
                .is_some_and(|n| n.contains("not connected"))
        );
    });

    // The system output by choice: the players move there, and P plays.
    clear_output_on_disk(cx, &f);
    assert_eq!(f.media.0.borrow().film_output, None);
    assert_eq!(f.media.0.borrow().output_sets.last(), Some(&None));
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(
        f.media.0.borrow().take.as_ref().map(|t| t.output.clone()),
        Some(None)
    );
    press(cx, f.window, &["p"]);

    // The phones come back: nothing moves, the system output was chosen.
    f.media.0.borrow_mut().outputs.push(output(
        "Test Phones",
        Transport::Usb,
        OutputSource::Unknown,
    ));
    f.media.set_now(300.0);
    frame(cx, f.window);
    assert_eq!(f.media.0.borrow().film_output, None);
    assert_eq!(
        f.view.read_with(cx, |view, _| view.film_output.clone()),
        None
    );
}

#[gpui_kit::test]
fn the_route_changes_for_a_take_still_loading(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    f.media.0.borrow_mut().take_loading = true;
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(f.media.0.borrow().take_plays.len(), 0, "still loading");
    unplug(&f, "Test Phones");
    clear_output_on_disk(cx, &f);
    assert_eq!(
        f.media.0.borrow().take.as_ref().map(|t| t.output.clone()),
        Some(None),
        "the loading take moved to the system output with the film"
    );
    f.media.0.borrow_mut().take_loading = false;
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(f.media.0.borrow().take_plays.len(), 1, "and starts there");
}

#[gpui_kit::test]
fn system_output_in_the_panel_is_the_way_out_of_a_missing_output(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    unplug(&f, "Test Phones");
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    assert!(message(cx, &f.view).contains("or System output, in the audio panel (A)"));
    f.view.update(cx, |view, _| view.choose_system_output());
    assert_eq!(config(&f.dir).output.device, None, "cleared by choice");
    assert_eq!(
        message(cx, &f.view),
        "The film and your takes play through the system output (MacBook Speakers)."
    );
    assert_eq!(f.media.0.borrow().film_output, None);
    assert_eq!(f.view.read_with(cx, |view, _| view.output_note()), None);
    record_take(cx, &f);
    assert_eq!(recorded(&f.dir).len(), 2, "the take is recorded now");
}

#[gpui_kit::test]
fn another_missing_output_set_on_disk_stops_a_playback(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    tw::set_route(&f.dir, None, Some("Gone Phones")).expect("output");
    f.view.update(cx, |view, _| view.reload());
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(
        message(cx, &f.view),
        "The playback stopped. Gone Phones is not connected: connect it or pick another output (A)."
    );
}

#[gpui_kit::test]
fn the_route_is_set_again_only_when_its_device_dropped(cx: &mut TestAppContext) {
    // Setting it before every start could rebuild the audio path and clip
    // the warm-up; a start only checks the device is there.
    let f = on_test_phones(cx);
    let sets = |f: &Fixture| f.media.0.borrow().output_sets.len();
    let before = sets(&f);
    f.media.set_now(200.0);
    press(cx, f.window, &["p", "p", "p"]);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(sets(&f), before, "nothing changed: not set again");
    press(cx, f.window, &["p"]);

    // Another device came: nothing to set again.
    plug(&f, "Other Speakers");
    press(cx, f.window, &["p"]);
    assert_eq!(sets(&f), before);
    press(cx, f.window, &["p"]);

    // The phones dropped out and came back between two ticks: another
    // CoreAudio object, so the next start sets the route again.
    unplug(&f, "Test Phones");
    plug(&f, "Test Phones");
    press(cx, f.window, &["p"]);
    assert_eq!(sets(&f), before + 1);
    assert_eq!(
        f.media.0.borrow().output_sets.last(),
        Some(&Some("Test Phones".to_owned()))
    );
    press(cx, f.window, &["p"]);

    // Gone without CoreAudio saying so: the start still finds out.
    f.media
        .0
        .borrow_mut()
        .outputs
        .retain(|o| o.name != "Test Phones");
    press(cx, f.window, &["p"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert!(message(cx, &f.view).starts_with("Test Phones is not connected"));
}

#[gpui_kit::test]
fn a_route_that_failed_to_be_set_is_tried_again(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    f.media.0.borrow_mut().output_error = Some("the device is busy".to_owned());
    unplug(&f, "Test Phones");
    plug(&f, "Test Phones");
    frame(cx, f.window);
    let attempts = f.media.0.borrow().output_attempts;
    let note = "Test Phones could not be used: the device is busy";
    assert_eq!(
        f.view
            .read_with(cx, |view, _| view.output_note())
            .as_deref(),
        Some(note)
    );
    // The next check tries again, and keeps the error while it fails.
    f.media.set_now(103.0);
    frame(cx, f.window);
    assert!(f.media.0.borrow().output_attempts > attempts, "retried");
    assert_eq!(
        f.view
            .read_with(cx, |view, _| view.output_note())
            .as_deref(),
        Some(note)
    );
    f.media.0.borrow_mut().output_error = None;
    f.media.set_now(106.0);
    frame(cx, f.window);
    assert_eq!(f.view.read_with(cx, |view, _| view.output_note()), None);
    assert_eq!(
        f.media.0.borrow().output_sets.last(),
        Some(&Some("Test Phones".to_owned()))
    );
}

#[gpui_kit::test]
fn a_device_change_stops_a_playback_at_once(cx: &mut TestAppContext) {
    // No two-second wait: CoreAudio's change is acted on at the next tick.
    let f = on_test_phones(cx);
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    unplug(&f, "Test Phones");
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert!(message(cx, &f.view).starts_with("The playback stopped. Test Phones is not connected"));
}

#[gpui_kit::test]
fn a_playback_does_not_start_once_its_output_went(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    f.media.0.borrow_mut().take_loading = true;
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    // Gone while the take loads; it has loaded by the next tick.
    unplug(&f, "Test Phones");
    f.media.0.borrow_mut().take_loading = false;
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(f.media.0.borrow().take_plays.len(), 0, "never scheduled");
}

#[gpui_kit::test]
fn a_take_whose_output_went_before_its_film_started_stops(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let plays = f.media.0.borrow().plays.len();
    unplug(&f, "Test Phones");
    f.media.send(100.05, 2400);
    settle(cx, &f);
    f.media.set_now(100.1);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(
        f.media.0.borrow().plays.len(),
        plays,
        "the film was never scheduled"
    );
    let said = message(cx, &f.view);
    assert!(
        said.contains(
            "Recording stopped: Test Phones is not connected, so Takewise does not record"
        ),
        "{said}"
    );
    assert!(
        !said.contains("may have played elsewhere"),
        "the film never played: {said}"
    );
}

/// Takes `name` away without CoreAudio saying so (no device change).
fn unplug_quietly(f: &Fixture, name: &str) {
    f.media.0.borrow_mut().outputs.retain(|o| o.name != name);
}

#[gpui_kit::test]
fn a_start_looks_up_the_device_even_without_a_change(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    f.media.0.borrow_mut().take_loading = true;
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    // Gone, unreported, and the poll not due: the start still checks.
    unplug_quietly(&f, "Test Phones");
    f.media.0.borrow_mut().take_loading = false;
    f.media.set_now(200.1);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(f.media.0.borrow().take_plays.len(), 0, "never scheduled");

    // A take's film, likewise.
    plug(&f, "Test Phones");
    f.media.set_now(300.0);
    press(cx, f.window, &["space"]);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let plays = f.media.0.borrow().plays.len();
    unplug_quietly(&f, "Test Phones");
    f.media.send(300.05, 2400);
    settle(cx, &f);
    f.media.set_now(300.1);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(f.media.0.borrow().plays.len(), plays, "never scheduled");
}

#[gpui_kit::test]
fn a_route_whose_device_cannot_be_confirmed_stays_due(cx: &mut TestAppContext) {
    // Set, but gone again before its CoreAudio object could be read: not
    // known to be there, so nothing is scheduled on it.
    let f = on_test_phones(cx);
    f.media.0.borrow_mut().objects_unreadable = true;
    unplug(&f, "Test Phones");
    plug(&f, "Test Phones");
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert!(message(cx, &f.view).starts_with("Test Phones is not connected"));
    assert_eq!(f.media.0.borrow().take_plays.len(), 0);
    assert!(f.view.read_with(cx, |view, _| view.route_due));
    // Confirmed on the next try: it plays.
    f.media.0.borrow_mut().objects_unreadable = false;
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert!(!f.view.read_with(cx, |view, _| view.route_due));
}

#[gpui_kit::test]
fn a_route_lost_before_the_scheduled_start_calls_the_start_off(cx: &mut TestAppContext) {
    // `begin` schedules d's film at 100.1 to start at 100.4.
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    unplug(&f, "Test Phones");
    f.media.set_now(100.2);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let said = message(cx, &f.view);
    assert!(
        said.contains(
            "Recording stopped: Test Phones is not connected, so Takewise does not record"
        ),
        "{said}"
    );
    assert!(!said.contains("may have played"), "nothing played: {said}");
    assert_eq!(recorded(&f.dir).len(), 2, "the audio is kept");

    // Gone and back before the start: say to start again.
    plug(&f, "Test Phones");
    f.media.set_now(110.0);
    frame(cx, f.window);
    begin(cx, &f, 120.0);
    unplug(&f, "Test Phones");
    plug(&f, "Test Phones");
    f.media.set_now(120.2);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let said = message(cx, &f.view);
    assert!(
        said.contains("Recording stopped: Test Phones dropped out; start the take again."),
        "{said}"
    );

    // A playback scheduled at 200.3: gone, unreported, at 200.1.
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    unplug_quietly(&f, "Test Phones");
    f.media.set_now(200.1);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert!(message(cx, &f.view).starts_with("The playback stopped. Test Phones is not connected"));
}

#[gpui_kit::test]
fn a_take_stopped_before_its_film_started_has_no_output_note(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    unplug_quietly(&f, "Test Phones");
    f.media.set_now(100.2);
    f.view.update(cx, |view, _| view.finish_take());
    let said = message(cx, &f.view);
    assert!(!said.contains("may have played"), "{said}");
}

#[gpui_kit::test]
fn a_route_loss_calls_off_a_start_whose_time_already_came(cx: &mut TestAppContext) {
    // `begin` schedules d's film for 100.4. At 100.42 the start time has
    // come, but the player has not moved: the window's stop still holds,
    // and the film is paused at once.
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 0.0,
        film_anchor_s: 6.0,
        host_anchor_s: 100.4,
    });
    f.media.set_now(100.42);
    let pauses = f.media.0.borrow().pauses.len();
    f.view.update(cx, |view, _| {
        view.recorder
            .hold_start(super::recorder::StartHold::Stop("gone".to_owned()));
        let update = view.recorder.tick(&mut *view.media, &view.dir);
        view.show(update);
    });
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert!(f.media.0.borrow().pauses.len() > pauses, "paused");
    assert!(message(cx, &f.view).contains("Recording stopped: gone"));
}

#[gpui_kit::test]
fn a_stalled_film_gives_no_output_note(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    // The player never moves: stopped where it was scheduled from.
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 0.0,
        film_anchor_s: 6.0,
        host_anchor_s: 100.4,
    });
    f.media.set_now(100.6);
    frame(cx, f.window);
    unplug(&f, "Test Phones");
    f.media.set_now(100.7);
    frame(cx, f.window);
    let said = message(cx, &f.view);
    assert!(!said.contains("may have played"), "it never ran: {said}");
}

#[gpui_kit::test]
fn every_drop_in_a_take_is_set_again_and_warned_once(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    f.media.set_now(100.5);
    frame(cx, f.window);
    let sets = |f: &Fixture| f.media.0.borrow().output_sets.len();
    let before = sets(&f);
    for (n, at) in [(1, 100.6), (2, 100.7)] {
        unplug(&f, "Test Phones");
        plug(&f, "Test Phones");
        f.media.set_now(at);
        frame(cx, f.window);
        assert_eq!(sets(&f), before + n, "drop {n} set again");
    }
    f.media.set_now(100.8);
    press(cx, f.window, &["space"]);
    let said = message(cx, &f.view);
    assert_eq!(
        said.matches("was disconnected during the take").count(),
        1,
        "{said}"
    );
}

#[gpui_kit::test]
fn a_failed_lookup_is_not_a_drop(cx: &mut TestAppContext) {
    // A playback: no rebind, no stop on a lookup that fails for a moment.
    let f = on_test_phones(cx);
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    let sets = f.media.0.borrow().output_sets.len();
    f.media.0.borrow_mut().lookups_fail = true;
    f.media.0.borrow_mut().generation += 1;
    f.media.set_now(200.1);
    frame(cx, f.window);
    f.media.0.borrow_mut().lookups_fail = false;
    f.media.set_now(200.2);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(f.media.0.borrow().output_sets.len(), sets, "not set again");
    press(cx, f.window, &["p"]);

    // A take whose film is about to be scheduled waits, and then starts.
    f.media.0.borrow_mut().lookups_fail = true;
    f.media.set_now(300.0);
    press(cx, f.window, &["space"]);
    f.media.send(300.05, 2400);
    settle(cx, &f);
    let plays = f.media.0.borrow().plays.len();
    f.media.set_now(300.1);
    frame(cx, f.window);
    assert!(
        f.view.read_with(cx, |view, _| view.recorder.is_recording()),
        "it waits"
    );
    assert_eq!(f.media.0.borrow().plays.len(), plays, "not scheduled yet");
    f.media.0.borrow_mut().lookups_fail = false;
    f.media.set_now(300.2);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(f.media.0.borrow().plays.len(), plays + 1, "scheduled now");

    // Failing for longer than the grace: then it counts as gone.
    f.media.set_now(300.7);
    frame(cx, f.window);
    f.media.0.borrow_mut().lookups_fail = true;
    for at in [300.8, 300.9, 301.0, 301.1, 301.2, 301.3, 301.4] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    f.media.set_now(301.5);
    press(cx, f.window, &["space"]);
    // Reads that failed are not a disconnect: said as such.
    assert!(message(cx, &f.view).contains(
        "Test Phones could not be checked during the take: the film may have played elsewhere"
    ));
}

#[gpui_kit::test]
fn nothing_starts_on_an_unconfirmed_route(cx: &mut TestAppContext) {
    // Set to the headphones, but their object cannot be read: P waits.
    let f = on_test_phones(cx);
    f.media.0.borrow_mut().lookups_fail = true;
    f.view
        .update(cx, |view, _| view.choose_output("External Headphones"));
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(
        f.view.read_with(cx, |view, _| view.recorder.is_playing()),
        "waiting"
    );
    assert_eq!(f.media.0.borrow().take_plays.len(), 0, "not scheduled");
    // Confirmed: it starts.
    f.media.0.borrow_mut().lookups_fail = false;
    f.media.set_now(200.1);
    frame(cx, f.window);
    assert_eq!(
        f.media.0.borrow().take_plays.len(),
        1,
        "scheduled once confirmed"
    );
    press(cx, f.window, &["p"]);

    // Never confirmed: it gives up after the grace, and says so.
    f.view
        .update(cx, |view, _| view.choose_output("Test Phones"));
    f.media.0.borrow_mut().lookups_fail = true;
    f.view
        .update(cx, |view, _| view.choose_output("External Headphones"));
    f.media.set_now(300.0);
    press(cx, f.window, &["p"]);
    for n in 1..=7 {
        f.media.set_now(300.0 + 0.1 * f64::from(n));
        frame(cx, f.window);
    }
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(f.media.0.borrow().take_plays.len(), 1);
    let said = message(cx, &f.view);
    assert!(
        said.contains("External Headphones could not be checked"),
        "{said}"
    );

    // A take likewise: its film waits, then the take stops keeping audio.
    let f = on_test_phones(cx);
    f.media.0.borrow_mut().lookups_fail = true;
    f.view
        .update(cx, |view, _| view.choose_output("External Headphones"));
    let plays = f.media.0.borrow().plays.len();
    begin(cx, &f, 100.0);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(f.media.0.borrow().plays.len(), plays, "the film waits");
    for n in 2..=8 {
        f.media.set_now(100.0 + 0.1 * f64::from(n));
        frame(cx, f.window);
    }
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let said = message(cx, &f.view);
    assert!(
        said.contains("could not be checked, so the film did not start"),
        "{said}"
    );
    assert_eq!(recorded(&f.dir).len(), 2, "the audio is kept");
}

#[gpui_kit::test]
fn a_scheduled_start_is_called_off_while_unconfirmed_and_scheduled_again(cx: &mut TestAppContext) {
    // A playback scheduled for 200.3; a read fails at 200.1.
    let f = on_test_phones(cx);
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert_eq!(f.media.0.borrow().take_plays.len(), 1);
    f.media.0.borrow_mut().lookups_fail = true;
    f.media.set_now(200.1);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert!(
        f.view
            .read_with(cx, |view, _| view.recorder.playback_waiting(200.5)),
        "called off"
    );
    f.media.0.borrow_mut().lookups_fail = false;
    f.media.set_now(200.2);
    frame(cx, f.window);
    assert_eq!(f.media.0.borrow().take_plays.len(), 2, "scheduled again");
    press(cx, f.window, &["p"]);

    // A take's film scheduled for 300.4: called off, the take goes on,
    // and the film is scheduled afresh once confirmed.
    begin(cx, &f, 300.0);
    let plays = f.media.0.borrow().plays.len();
    let pauses = f.media.0.borrow().pauses.len();
    f.media.0.borrow_mut().lookups_fail = true;
    f.media.set_now(300.2);
    frame(cx, f.window);
    assert!(f.media.0.borrow().pauses.len() > pauses, "paused");
    f.media.0.borrow_mut().lookups_fail = false;
    f.media.set_now(300.3);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(
        f.media.0.borrow().plays.len(),
        plays + 1,
        "scheduled afresh"
    );
    let (film_s, host_s) = last_play(&f);
    assert!(
        close(film_s, 6.0) && close(host_s, 300.6),
        "{film_s} {host_s}"
    );
    // Its first sample (heard at host 300.0) lies 0.6 s before film 6.0,
    // which is 2.6 s before d's start: so says the partial sidecar a crash
    // would be recovered from, and the committed take.
    let partial = partial_meta(&f.dir, "d").expect("the take is on disk");
    let sidecar: serde_json::Value =
        serde_json::from_slice(&fs::read(&partial).expect("sidecar")).expect("json");
    let offset = sidecar["line_offset_s"].as_f64().expect("line_offset_s");
    assert!(close(offset, -2.6), "recovered at {offset}");
    f.media.set_now(300.8);
    press(cx, f.window, &["space"]);
    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 2, "kept, placed by the new start");
    let offset = takes[1].2.line_offset_s;
    assert!(close(offset, -2.6), "committed at {offset}");
}

/// The partial sidecar of `line`'s take while it records.
fn partial_meta(dir: &Path, line: &str) -> Option<PathBuf> {
    fs::read_dir(dir.join("takes").join(line))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.to_string_lossy().ends_with(".json.partial"))
}

#[gpui_kit::test]
fn a_playing_route_that_cannot_be_read_is_retried_each_tick(cx: &mut TestAppContext) {
    // A playback that is playing (scheduled for 200.3, read at 200.5),
    // then one device change and reads that fail: read again every tick,
    // well before the two-second poll (next due at 202.0).
    let f = on_test_phones(cx);
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    f.media.set_now(200.5);
    frame(cx, f.window);
    let playing_now = |cx: &mut TestAppContext, at: f64| {
        f.view.read_with(cx, |view, _| {
            view.recorder.is_playing() && !view.recorder.playback_waiting(at)
        })
    };
    assert!(playing_now(cx, 200.5), "started, not waiting");
    let counts = |f: &Fixture| {
        let fake = f.media.0.borrow();
        (
            fake.take_plays.len(),
            fake.pauses.len(),
            fake.output_sets.len(),
        )
    };
    let before = counts(&f);

    // (a) A read succeeds within the grace: it plays on, untouched.
    f.media.0.borrow_mut().lookups_fail = true;
    f.media.0.borrow_mut().generation += 1;
    let mut lookups = f.media.0.borrow().lookups;
    for at in [200.6, 200.7, 200.8] {
        f.media.set_now(at);
        frame(cx, f.window);
        assert!(f.media.0.borrow().lookups > lookups, "read again at {at}");
        lookups = f.media.0.borrow().lookups;
        assert!(playing_now(cx, at), "still playing at {at}");
    }
    f.media.0.borrow_mut().lookups_fail = false;
    f.media.set_now(200.9);
    frame(cx, f.window);
    assert!(playing_now(cx, 200.9));
    assert_eq!(
        counts(&f),
        before,
        "no pause, no new schedule, no new output"
    );

    // (b) Reads that keep failing: it stops once the grace is over.
    f.media.0.borrow_mut().lookups_fail = true;
    f.media.0.borrow_mut().generation += 1;
    for n in 0..=4 {
        let at = 201.0 + 0.1 * f64::from(n);
        f.media.set_now(at);
        frame(cx, f.window);
        assert!(f.media.0.borrow().lookups > lookups, "read again at {at}");
        lookups = f.media.0.borrow().lookups;
        assert!(playing_now(cx, at), "within the grace at {at}");
    }
    f.media.set_now(201.5);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(
        message(cx, &f.view),
        "The playback stopped. Test Phones could not be checked; try again."
    );
}

#[gpui_kit::test]
fn a_take_confirms_its_device_and_clears_the_missing_note(cx: &mut TestAppContext) {
    // The headphones are set while their object cannot be read; a take
    // confirms them before its film starts, and the panel's note goes.
    let f = on_test_phones(cx);
    f.media.0.borrow_mut().lookups_fail = true;
    f.view
        .update(cx, |view, _| view.choose_output("External Headphones"));
    f.view.update(cx, |view, _| {
        view.output_missing = Some("External Headphones".to_owned());
    });
    begin(cx, &f, 100.0);
    f.media.0.borrow_mut().lookups_fail = false;
    f.media.set_now(100.2);
    frame(cx, f.window);
    f.view.read_with(cx, |view, _| {
        assert!(
            matches!(view.applied_device, Some((_, Some(_)))),
            "confirmed"
        );
        assert_eq!(view.output_note(), None);
    });
}

#[gpui_kit::test]
fn an_unread_route_that_fails_past_the_grace_is_set_again(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    f.media.0.borrow_mut().lookups_fail = true;
    f.view
        .update(cx, |view, _| view.choose_output("External Headphones"));
    for n in 1..=7 {
        f.media.set_now(100.0 + 0.1 * f64::from(n));
        frame(cx, f.window);
    }
    assert!(f.view.read_with(cx, |view, _| view.route_due), "due again");
}

#[gpui_kit::test]
fn a_refusal_gives_way_to_the_connected_notice_once_confirmed(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        tw::set_route(dir, None, Some(PHONES)).expect("output");
        commit_outside(dir, "d");
    });
    press(cx, f.window, &["down", "down", "down"]); // d
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    let refusal = format!("{PHONES} is not connected: connect it or pick another output (A).");
    assert_eq!(message(cx, &f.view), refusal);
    // Back, but not confirmed yet: still the refusal.
    f.media.0.borrow_mut().lookups_fail = true;
    plug(&f, PHONES);
    f.media.set_now(200.1);
    frame(cx, f.window);
    assert_eq!(message(cx, &f.view), refusal, "not confirmed yet");
    // Confirmed on a later tick, without another device change.
    f.media.0.borrow_mut().lookups_fail = false;
    f.media.set_now(200.2);
    frame(cx, f.window);
    assert_eq!(
        message(cx, &f.view),
        format!("{PHONES} is connected: the film and your takes play there.")
    );
}

#[gpui_kit::test]
fn a_probe_decides_but_places_nothing(cx: &mut TestAppContext) {
    // The film's player first reports it started at 100.41 (a glimpse at
    // 100.42, when the phones are gone), then at 100.43 once settled. The
    // decision at 100.42 sees it moving, so the take goes on and warns;
    // the take is placed by the settled read (from 100.45 on), not the
    // glimpse: its first sample (host 100.0) at film 6.0 - 0.43 = 5.57.
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 1.0,
        film_anchor_s: 6.0,
        host_anchor_s: 100.41,
    });
    unplug_quietly(&f, "Test Phones");
    f.media.set_now(100.42);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 1.0,
        film_anchor_s: 6.0,
        host_anchor_s: 100.43,
    });
    f.media.set_now(100.5);
    frame(cx, f.window);
    f.media.set_now(100.6);
    press(cx, f.window, &["space"]);
    assert!(message(cx, &f.view).contains("Test Phones was disconnected during the take"));
    let takes = recorded(&f.dir);
    let offset = takes.last().expect("the take").2.line_offset_s;
    assert!(close(offset, 5.57 - 8.0), "placed at {offset}");
}

/// The project's film path.
fn film_of(f: &Fixture) -> PathBuf {
    Project::open(&f.dir).expect("project").film_path()
}

#[gpui_kit::test]
fn a_new_render_asks_for_the_window_to_be_opened_again(cx: &mut TestAppContext) {
    // `takewise film` points the open project at another render: the
    // player keeps the film it opened, and the window says so until the
    // film is opened again, which opens the new one.
    let f = open(cx, &[(DEVICE, 48_000)]);
    let replaced = |cx: &mut TestAppContext| {
        f.view
            .read_with(cx, |view, _| view.film_replaced_note().unwrap_or_default())
    };
    assert_eq!(replaced(cx), "");
    let render = f.dir.join("film-v2.mp4");
    fs::write(&render, b"").expect("film");
    let info = FilmInfo {
        duration_s: 21.0,
        size: 0,
        modified: "2026-11-26T00:00:00Z".to_owned(),
    };
    tw::set_film(&f.dir, Some(&render), &info, jiff::Timestamp::now()).expect("set film");
    f.view.update(cx, |view, _| view.reload());
    assert!(
        replaced(cx).contains("another render of the film"),
        "{}",
        replaced(cx)
    );
    let reopened = f.view.update(cx, |view, _| view.restore_film());
    assert_eq!(reopened, Ok(()));
    assert_eq!(f.media.0.borrow().films.last(), Some(&render));
    assert_eq!(replaced(cx), "");
}

/// The headphones go off and on again between two ticks: the devices
/// changed, and a player on them failed, as macOS fails it.
fn off_and_on(f: &Fixture, player: &str) {
    unplug(f, "Test Phones");
    plug(f, "Test Phones");
    let mut fake = f.media.0.borrow_mut();
    let failure = Some("The operation could not be completed".to_owned());
    match player {
        "film" => fake.film_failed = failure,
        _ => fake.take_failed = failure,
    }
}

/// Why P, Shift+P, Space or a playback could not start on the film.
fn film_status(cx: &mut TestAppContext, f: &Fixture) -> String {
    f.view
        .read_with(cx, |view, _| view.film_status().unwrap_or_default())
}

/// What the film's recovery says, beside the window's message.
fn film_note(cx: &mut TestAppContext, f: &Fixture) -> String {
    f.view.read_with(cx, |view, _| {
        let now = view.media.host_now_s();
        view.film_note(now).unwrap_or_default()
    })
}

fn films(f: &Fixture) -> usize {
    f.media.0.borrow().films.len()
}

#[gpui_kit::test]
fn a_film_that_failed_mid_play_is_reopened_and_plays_again(cx: &mut TestAppContext) {
    // As macOS does when the headphones go off: the playing item fails
    // for good; the playback stops, the film is reopened, and P works.
    let f = on_test_phones(cx);
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    f.media.set_now(200.5);
    frame(cx, f.window);
    let opened = films(&f);
    off_and_on(&f, "film");
    f.media.set_now(200.6);
    frame(cx, f.window);
    f.media.set_now(200.65);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    let said = message(cx, &f.view);
    assert!(
        said.contains(
            "The playback stopped: the film stopped (The operation could not be completed)"
        ),
        "{said}"
    );
    assert!(film_note(cx, &f).contains("The film stopped"));
    assert_eq!(films(&f), opened + 1, "reopened");
    assert_eq!(f.media.0.borrow().films.last(), Some(&film_of(&f)));
    // Loaded: its notice goes.
    f.media.set_now(200.7);
    frame(cx, f.window);
    assert_eq!(film_note(cx, &f), "");
    let plays = f.media.0.borrow().take_plays.len();
    f.media.set_now(201.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(
        f.media.0.borrow().take_plays.len(),
        plays + 1,
        "played again"
    );

    // A take whose player failed: stopped; the next P opens a new one.
    f.media.set_now(201.5);
    frame(cx, f.window);
    off_and_on(&f, "take");
    f.media.set_now(201.6);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert!(message(cx, &f.view).contains("the take stopped"));
    f.media.set_now(202.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
}

#[gpui_kit::test]
fn a_failed_film_is_reopened_once_its_output_is_back(cx: &mut TestAppContext) {
    // The headphones go off mid-play: the film fails, and is not reopened
    // while they are gone (a new player would fail the same way); on
    // again, it is reopened at the next tick.
    let f = on_test_phones(cx);
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    f.media.set_now(200.5);
    frame(cx, f.window);
    let opened = films(&f);
    unplug(&f, "Test Phones");
    f.media.0.borrow_mut().film_failed = Some("failed".to_owned());
    for at in [200.6, 203.0, 206.0] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(films(&f), opened, "not while it is gone");
    plug(&f, "Test Phones");
    f.media.set_now(206.1);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "reopened once back");
    f.media.set_now(207.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
}

#[gpui_kit::test]
fn a_film_on_phones_connected_after_opening_is_reopened(cx: &mut TestAppContext) {
    // Opened with the phones off (the window waits for them), then on:
    // P plays on them; off and on again, the film is reopened on them.
    let f = on_test_phones(cx);
    unplug(&f, "Test Phones");
    f.media.set_now(150.0);
    frame(cx, f.window);
    plug(&f, "Test Phones");
    f.media.set_now(151.0);
    frame(cx, f.window);
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    f.media.set_now(200.5);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    let opened = films(&f);
    off_and_on(&f, "film");
    f.media.set_now(200.6);
    frame(cx, f.window);
    f.media.set_now(200.65);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "reopened on them");
    f.media.set_now(201.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
}

#[gpui_kit::test]
fn a_film_on_phones_chosen_after_opening_is_reopened(cx: &mut TestAppContext) {
    // Opened on the system output; the phones are chosen afterwards (in
    // project.toml): off and on during a playback, the film is reopened.
    let f = open(cx, &[(DEVICE, 48_000)]);
    record_take(cx, &f);
    tw::set_route(&f.dir, None, Some("Test Phones")).expect("output");
    f.view.update(cx, |view, _| view.reload());
    f.media.set_now(150.0);
    frame(cx, f.window);
    assert_eq!(
        f.media.0.borrow().film_output.as_deref(),
        Some("Test Phones")
    );
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    f.media.set_now(200.5);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    let opened = films(&f);
    off_and_on(&f, "film");
    f.media.set_now(200.6);
    frame(cx, f.window);
    f.media.set_now(200.65);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "reopened on them");
    assert!(film_note(cx, &f).contains("The film stopped (The operation could not be completed)"));
}

#[gpui_kit::test]
fn waiting_to_reopen_lists_no_devices(cx: &mut TestAppContext) {
    // While the output is missing, recovery reads the route as the window
    // keeps it: no more listings of the devices by name than the route
    // check's own poll makes without it, and nothing set; "connected" is
    // said by the route check, as ever.
    let f = on_test_phones(cx);
    unplug(&f, "Test Phones");
    let ticks = |cx: &mut TestAppContext, from: f64| {
        let listings = f.media.0.borrow().listings;
        for k in 1..=120 {
            f.media.set_now(from + 0.1 * f64::from(k));
            frame(cx, f.window);
        }
        f.media.0.borrow().listings - listings
    };
    f.media.set_now(100.0);
    frame(cx, f.window);
    let alone = ticks(cx, 100.0);
    assert!(alone < 10, "{alone} listings without a failed film");
    f.media.0.borrow_mut().film_failed = Some("failed".to_owned());
    f.media.set_now(112.05);
    frame(cx, f.window);
    let sets = f.media.0.borrow().output_sets.len();
    let waiting = ticks(cx, 112.05);
    assert!(waiting <= alone + 1, "{waiting} listings, {alone} without");
    assert_eq!(f.media.0.borrow().output_sets.len(), sets, "nothing set");
    plug(&f, "Test Phones");
    f.media.set_now(125.0);
    frame(cx, f.window);
    assert!(
        message(cx, &f.view).contains("Test Phones is connected"),
        "{}",
        message(cx, &f.view)
    );
}

#[gpui_kit::test]
fn a_failed_film_waits_for_a_confirmed_output(cx: &mut TestAppContext) {
    // Back and listed, but the route is not usable yet: setting it fails,
    // then its object cannot be read. The film waits for both; P and
    // Space say why and start nothing.
    let f = on_test_phones(cx);
    let opened = films(&f);
    unplug(&f, "Test Phones");
    f.media.0.borrow_mut().film_failed = Some("failed".to_owned());
    f.media.set_now(100.0);
    frame(cx, f.window);
    plug(&f, "Test Phones");
    f.media.0.borrow_mut().output_error = Some("the device is busy".to_owned());
    for at in [100.1, 103.0, 106.0] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert_eq!(films(&f), opened, "not on a route that failed");
    f.media.0.borrow_mut().output_error = None;
    f.media.0.borrow_mut().lookups_fail = true;
    for at in [109.0, 112.0] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert_eq!(films(&f), opened, "not on an unconfirmed route");
    press(cx, f.window, &["space"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(
        film_status(cx, &f),
        "The film is not ready; try again once it is back."
    );
    f.media.0.borrow_mut().lookups_fail = false;
    f.media.set_now(115.0);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "reopened once confirmed");
    assert_eq!(f.media.0.borrow().films.last(), Some(&film_of(&f)));
}

#[gpui_kit::test]
fn a_failed_film_waits_for_a_system_output(cx: &mut TestAppContext) {
    // On the system output, with none there (the last one went away):
    // nothing to open the film on until one is.
    let f = open(cx, &[(DEVICE, 48_000)]);
    let opened = films(&f);
    {
        let mut fake = f.media.0.borrow_mut();
        fake.no_default_output = true;
        fake.generation += 1;
        fake.film_failed = Some("failed".to_owned());
    }
    for at in [100.0, 103.0, 106.0] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert_eq!(films(&f), opened, "no system output");
    {
        let mut fake = f.media.0.borrow_mut();
        fake.no_default_output = false;
        fake.generation += 1;
    }
    f.media.set_now(106.1);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "reopened");
}

#[gpui_kit::test]
fn a_reopen_that_failed_backs_off_and_retries_on_a_change(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    record_take(cx, &f);
    let result = message(cx, &f.view);
    assert!(result.contains("Recorded"), "{result}");
    // Failed, and reopening fails too: said in its own notice, beside the
    // take's result, and tried again after 1, 2, 4… seconds.
    off_and_on(&f, "film");
    f.media.0.borrow_mut().film_open_error = Some("no".to_owned());
    f.media.set_now(200.0);
    frame(cx, f.window);
    f.media.set_now(200.05);
    frame(cx, f.window);
    assert!(film_note(cx, &f).contains("Could not open the film again (no)"));
    assert!(
        film_note(cx, &f).contains("in 1 s"),
        "{}",
        film_note(cx, &f)
    );
    assert_eq!(message(cx, &f.view), result, "the take's result stays");
    f.media.set_now(200.5);
    frame(cx, f.window);
    assert!(film_note(cx, &f).contains("in 1 s"), "not before");
    f.media.set_now(201.1);
    frame(cx, f.window);
    assert!(
        film_note(cx, &f).contains("in 2 s"),
        "{}",
        film_note(cx, &f)
    );
    // A change of the devices tries it at once.
    unplug(&f, "Test Phones");
    plug(&f, "Test Phones");
    f.media.set_now(201.2);
    frame(cx, f.window);
    assert!(
        film_note(cx, &f).contains("in 4 s"),
        "{}",
        film_note(cx, &f)
    );
    // Space says why, and tries nothing sooner.
    press(cx, f.window, &["space"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(
        film_status(cx, &f),
        "The film is not ready; try again once it is back."
    );
    assert!(
        message(cx, &f.view).contains("Recorded"),
        "the take's result stays"
    );
    // The file is back (no device change, no key): the next try, by the
    // clock alone, opens it, and the notice and status go once it has
    // loaded.
    f.media.0.borrow_mut().film_open_error = None;
    f.media.set_now(204.0);
    frame(cx, f.window);
    assert!(
        f.media.0.borrow().film_failed.is_some(),
        "not before the pause"
    );
    f.media.set_now(205.3);
    frame(cx, f.window);
    assert!(f.media.0.borrow().film_failed.is_none(), "reopened");
    f.media.set_now(205.4);
    frame(cx, f.window);
    assert_eq!(film_note(cx, &f), "");
    assert_eq!(film_status(cx, &f), "");
    // Space records, and the film is scheduled.
    let plays = f.media.0.borrow().plays.len();
    begin(cx, &f, 210.0);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert!(
        f.media.0.borrow().plays.len() > plays,
        "the film is scheduled"
    );
    f.media.set_now(210.5);
    press(cx, f.window, &["space"]);
    assert_eq!(recorded(&f.dir).len(), 3);
}

#[gpui_kit::test]
fn a_film_that_fails_during_a_take_stops_it(cx: &mut TestAppContext) {
    // The headphones go off mid-take: the film's clock stops, so the take
    // would never reach its end. It stops at once, keeps its audio, says
    // why, and the film is reopened once the output is back.
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    f.media.set_now(100.5);
    frame(cx, f.window);
    let opened = films(&f);
    unplug(&f, "Test Phones");
    f.media.0.borrow_mut().film_failed = Some("failed".to_owned());
    f.media.set_now(100.6);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(recorded(&f.dir).len(), 2, "the take is kept");
    let said = message(cx, &f.view);
    assert!(said.contains("Recorded d take 2"), "{said}");
    assert!(
        said.contains("The take stopped early: the film stopped (failed)."),
        "{said}"
    );
    assert_eq!(films(&f), opened, "not while it is gone");
    plug(&f, "Test Phones");
    f.media.set_now(101.0);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "reopened after it");
    f.media.set_now(101.1);
    frame(cx, f.window);
    assert_eq!(film_note(cx, &f), "");
    assert_eq!(message(cx, &f.view), said, "the take's result stays");
}

#[gpui_kit::test]
fn a_film_that_fails_before_any_audio_claims_no_take(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    f.media.0.borrow_mut().film_failed = Some("failed".to_owned());
    f.media.set_now(100.1);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(recorded(&f.dir).len(), 1, "no take");
    let said = message(cx, &f.view);
    assert!(said.contains("The take stopped early"), "{said}");
    assert!(
        !said.contains("Recorded") && !said.contains("kept"),
        "{said}"
    );
}

#[gpui_kit::test]
fn a_take_stopped_by_its_film_says_so_when_saved_later(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    fs::create_dir_all(f.dir.join("takes")).expect("takes");
    fs::write(f.dir.join("takes/a"), b"").expect("blocker");
    begin(cx, &f, 100.0);
    f.media.0.borrow_mut().film_failed = Some("failed".to_owned());
    f.media.set_now(100.5);
    frame(cx, f.window);
    let said = message(cx, &f.view);
    assert!(said.contains("could not be saved"), "{said}");
    assert!(said.contains("The take stopped early"), "{said}");
    fs::remove_file(f.dir.join("takes/a")).expect("unblock");
    f.media.set_now(110.0);
    frame(cx, f.window);
    assert_eq!(recorded(&f.dir).len(), 1, "saved on retry");
    let said = message(cx, &f.view);
    assert!(said.contains("Saved the earlier take of a"), "{said}");
    assert!(said.contains("The take stopped early"), "{said}");
}

#[gpui_kit::test]
fn a_reopened_film_that_never_loads_counts_as_failed(cx: &mut TestAppContext) {
    // Reopened, the new player neither loads nor fails. After ten seconds
    // it counts as failed and is opened again; the next one is given
    // longer.
    let f = on_test_phones(cx);
    off_and_on(&f, "film");
    f.media.0.borrow_mut().film_loading = true;
    f.media.set_now(100.0);
    frame(cx, f.window);
    // Reopened once the route is confirmed afresh.
    f.media.set_now(100.05);
    frame(cx, f.window);
    let opened = films(&f);
    f.media.set_now(105.0);
    frame(cx, f.window);
    assert_eq!(films(&f), opened, "a slow film may still load");
    f.media.set_now(110.1);
    frame(cx, f.window);
    let note = film_note(cx, &f);
    assert!(note.contains("could not play (it did not load)"), "{note}");
    f.media.set_now(111.2);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "reopened");
    f.media.set_now(125.0);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "given twenty seconds");
    f.media.0.borrow_mut().film_loading = false;
    f.media.set_now(125.1);
    frame(cx, f.window);
    assert_eq!(film_note(cx, &f), "");
    press(cx, f.window, &["space"]);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
}

#[gpui_kit::test]
fn space_on_a_slow_reopen_lets_it_load(cx: &mut TestAppContext) {
    // A reopened film still loading after four seconds: Space says so and
    // leaves it to load; once it is ready, that status goes.
    let f = on_test_phones(cx);
    off_and_on(&f, "film");
    f.media.0.borrow_mut().film_loading = true;
    f.media.set_now(100.0);
    frame(cx, f.window);
    // Reopened once the route is confirmed afresh.
    f.media.set_now(100.05);
    frame(cx, f.window);
    let opened = films(&f);
    f.media.set_now(104.0);
    press(cx, f.window, &["space"]);
    assert_eq!(
        film_status(cx, &f),
        "The film is not ready; try again once it is back."
    );
    f.media.set_now(104.1);
    frame(cx, f.window);
    assert_eq!(films(&f), opened, "not thrown away");
    f.media.0.borrow_mut().film_loading = false;
    f.media.set_now(106.0);
    frame(cx, f.window);
    assert_eq!(film_status(cx, &f), "", "the status goes");
    assert_eq!(film_note(cx, &f), "");
    press(cx, f.window, &["space"]);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
}

#[gpui_kit::test]
fn a_route_change_restarts_a_stalled_reopen(cx: &mut TestAppContext) {
    // A reopened film stalls loading; System output is chosen (no CoreAudio
    // event, no film failure): it is opened again at once on that route.
    let f = on_test_phones(cx);
    off_and_on(&f, "film");
    f.media.0.borrow_mut().film_loading = true;
    f.media.set_now(100.0);
    frame(cx, f.window);
    // Reopened once the route is confirmed afresh.
    f.media.set_now(100.05);
    frame(cx, f.window);
    let opened = films(&f);
    clear_output_on_disk(cx, &f);
    for at in [101.0, 101.1, 101.2] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert_eq!(f.media.0.borrow().film_output, None);
    assert_eq!(films(&f), opened + 1, "opened again on the new route");
    assert!(!film_note(cx, &f).contains("check the film file"));
}

/// Fails the film on the system output, and lets it be reopened (still
/// loading) once the route is read again.
fn reopen_on_system_output(cx: &mut TestAppContext, f: &Fixture, at: f64) {
    {
        let mut fake = f.media.0.borrow_mut();
        fake.film_failed = Some("failed".to_owned());
        fake.film_loading = true;
    }
    f.media.set_now(at);
    frame(cx, f.window);
    f.media.set_now(at + 0.05);
    frame(cx, f.window);
}

/// The system's default output becomes another device, without CoreAudio
/// saying the devices changed.
fn switch_default_silently(f: &Fixture) {
    f.media.0.borrow_mut().default_output = "Other Speakers".to_owned();
}

#[gpui_kit::test]
fn another_device_alone_does_not_restart_a_loading_film(cx: &mut TestAppContext) {
    // A reopened film loads; a microphone is plugged in (the devices
    // change, the output does not): it is left to load.
    let f = on_test_phones(cx);
    off_and_on(&f, "film");
    f.media.0.borrow_mut().film_loading = true;
    f.media.set_now(100.0);
    frame(cx, f.window);
    f.media.set_now(100.05);
    frame(cx, f.window);
    let opened = films(&f);
    plug(&f, "USB Mic");
    for at in [101.0, 102.0, 105.0] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert_eq!(films(&f), opened, "left to load");
}

#[gpui_kit::test]
fn a_loading_film_is_kept_while_its_route_cannot_be_read(cx: &mut TestAppContext) {
    // Reads of the phones fail (the devices changed, so the route is read)
    // while a reopened film loads: the player is kept, the notice says it
    // waits for its output; it then loads.
    let f = on_test_phones(cx);
    off_and_on(&f, "film");
    f.media.0.borrow_mut().film_loading = true;
    f.media.set_now(100.0);
    frame(cx, f.window);
    f.media.set_now(100.05);
    frame(cx, f.window);
    let opened = films(&f);
    f.media.0.borrow_mut().lookups_fail = true;
    plug(&f, "USB Mic");
    for at in [100.5, 100.6, 101.5] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert_eq!(films(&f), opened, "kept");
    let note = film_note(cx, &f);
    assert!(note.contains("once its output is back"), "{note}");
    f.media.0.borrow_mut().lookups_fail = false;
    f.media.0.borrow_mut().film_loading = false;
    for at in [102.0, 102.1] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert_eq!(films(&f), opened, "still the same player");
    assert_eq!(film_note(cx, &f), "");
}

#[gpui_kit::test]
fn a_silent_switch_of_the_default_restarts_a_loading_film(cx: &mut TestAppContext) {
    // On the system output, the default becomes another device with no
    // CoreAudio event: the film loading on the one before is opened again
    // on it, not counted as an attempt (it keeps its ten seconds). Then
    // no system output at all: the notice says it waits for one.
    let f = open(cx, &[(DEVICE, 48_000)]);
    reopen_on_system_output(cx, &f, 100.0);
    let opened = films(&f);
    switch_default_silently(&f);
    f.media.set_now(101.0);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "opened again on the new default");
    f.media.set_now(110.5);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "ten seconds, not over yet");
    f.media.set_now(111.2);
    frame(cx, f.window);
    let note = film_note(cx, &f);
    assert!(note.contains("did not load"), "ten, not twenty: {note}");
    {
        let mut fake = f.media.0.borrow_mut();
        fake.no_default_output = true;
        fake.generation += 1;
    }
    f.media.set_now(111.3);
    frame(cx, f.window);
    let note = film_note(cx, &f);
    assert!(note.contains("once its output is back"), "{note}");
}

#[gpui_kit::test]
fn a_queued_playback_never_starts_on_a_film_set_aside(cx: &mut TestAppContext) {
    // P while a reopened film loads; the output is replaced meanwhile: the
    // film is set aside and the queued playback stops (said in the film's
    // status). The old player turning ready starts nothing.
    let f = open(cx, &[(DEVICE, 48_000)]);
    record_take(cx, &f);
    reopen_on_system_output(cx, &f, 200.0);
    f.media.set_now(200.1);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    let plays = f.media.0.borrow().take_plays.len();
    f.media.0.borrow_mut().film_open_error = Some("busy".to_owned());
    switch_default_silently(&f);
    f.media.set_now(200.2);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    let status = film_status(cx, &f);
    assert!(status.contains("play it again"), "{status}");
    // The old player loads after all: still set aside, nothing plays on
    // it, not even its warm-up.
    let film_plays = f.media.0.borrow().plays.len();
    f.media.0.borrow_mut().film_loading = false;
    for at in [200.3, 200.4, 200.8] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert_eq!(
        f.media.0.borrow().take_plays.len(),
        plays,
        "nothing started"
    );
    assert_eq!(f.media.0.borrow().plays.len(), film_plays, "not warmed up");
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
}

#[gpui_kit::test]
fn a_film_failing_as_its_device_goes_is_not_reopened_on_it(cx: &mut TestAppContext) {
    // The phones go as the film fails, before CoreAudio says so: the route
    // is read afresh, finds them gone, and the film waits for them.
    let f = on_test_phones(cx);
    let opened = films(&f);
    unplug_quietly(&f, "Test Phones");
    f.media.0.borrow_mut().film_failed = Some("failed".to_owned());
    for at in [100.0, 100.05, 100.1, 101.0] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert_eq!(films(&f), opened, "not on the vanished phones");
    plug(&f, "Test Phones");
    f.media.set_now(102.0);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "reopened once back");
}

#[gpui_kit::test]
fn the_films_notice_follows_what_keeps_it_from_opening(cx: &mut TestAppContext) {
    // Space while the phones cannot be read: the status says to wait, the
    // notice that it waits for its output. Readable again, but opening
    // fails: the notice says so without another key; the status stays.
    let f = on_test_phones(cx);
    {
        let mut fake = f.media.0.borrow_mut();
        fake.lookups_fail = true;
        fake.film_failed = Some("failed".to_owned());
    }
    f.media.set_now(100.0);
    frame(cx, f.window);
    plug(&f, "USB Mic");
    f.media.set_now(100.05);
    frame(cx, f.window);
    press(cx, f.window, &["space"]);
    assert_eq!(
        film_status(cx, &f),
        "The film is not ready; try again once it is back."
    );
    let note = film_note(cx, &f);
    assert!(note.contains("once its output is back"), "{note}");
    {
        let mut fake = f.media.0.borrow_mut();
        fake.film_open_error = Some("no".to_owned());
        fake.lookups_fail = false;
    }
    for at in [100.1, 100.2] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert_eq!(
        film_status(cx, &f),
        "The film is not ready; try again once it is back."
    );
    let note = film_note(cx, &f);
    assert!(
        note.contains("Could not open the film again (no)"),
        "{note}"
    );
}

#[gpui_kit::test]
fn a_retry_reads_the_device_before_it_opens(cx: &mut TestAppContext) {
    // A film that failed twice waits out its pause; meanwhile the phones
    // go and CoreAudio does not say so yet. At the retry, one read finds
    // them gone: nothing is opened on them.
    let f = on_test_phones(cx);
    f.media.0.borrow_mut().film_failed = Some("Cannot decode".to_owned());
    for at in [100.0, 100.05] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    f.media.0.borrow_mut().film_failed = Some("Cannot decode".to_owned());
    for at in [100.1, 100.15] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    let opened = films(&f);
    unplug_quietly(&f, "Test Phones");
    for at in [101.2, 101.3, 104.0] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert_eq!(films(&f), opened, "not on the vanished phones");
    let note = film_note(cx, &f);
    assert!(note.contains("once its output is back"), "{note}");
    plug(&f, "Test Phones");
    f.media.set_now(105.0);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "opened once they are back");
}

#[gpui_kit::test]
fn a_queued_playback_waits_for_a_system_output(cx: &mut TestAppContext) {
    // P while a reopened film loads on the system output; the last output
    // goes (no CoreAudio event) as the film turns ready: the playback does
    // not start with nowhere to play.
    let f = open(cx, &[(DEVICE, 48_000)]);
    record_take(cx, &f);
    reopen_on_system_output(cx, &f, 200.0);
    f.media.set_now(200.1);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    let plays = f.media.0.borrow().take_plays.len();
    {
        let mut fake = f.media.0.borrow_mut();
        fake.no_default_output = true;
        fake.film_loading = false;
    }
    for at in [200.2, 200.3, 200.4] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert_eq!(f.media.0.borrow().take_plays.len(), plays, "not started");
}

#[gpui_kit::test]
fn a_default_switch_as_the_film_turns_ready_still_restarts_it(cx: &mut TestAppContext) {
    // The reopened film turns ready in the very tick the default becomes
    // another device: it is opened again on that one.
    let f = open(cx, &[(DEVICE, 48_000)]);
    reopen_on_system_output(cx, &f, 100.0);
    let opened = films(&f);
    switch_default_silently(&f);
    f.media.0.borrow_mut().film_loading = false;
    f.media.set_now(100.5);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "opened again on the new default");
}

#[gpui_kit::test]
fn a_failure_is_not_said_to_wait_for_the_output_while_it_is_read_again(cx: &mut TestAppContext) {
    // In the frame the film fails, its route is only being read again: the
    // notice does not say it waits for its output.
    let f = on_test_phones(cx);
    f.media.0.borrow_mut().film_failed = Some("failed".to_owned());
    f.media.set_now(100.0);
    frame(cx, f.window);
    let note = film_note(cx, &f);
    assert!(!note.contains("once its output is back"), "{note}");
    assert!(note.contains("The film stopped (failed)"), "{note}");
}

#[gpui_kit::test]
fn r_blocked_after_a_warned_take_says_the_warning_once(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    warned_take(cx, &f, 100.0);
    f.view.update(cx, |view, _| {
        view.script_problem = Some("The script has a problem.".to_owned());
    });
    f.media.set_now(101.0);
    press(cx, f.window, &["r"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let said = message(cx, &f.view);
    assert_eq!(
        said.matches("Test Phones was disconnected").count(),
        1,
        "once: {said}"
    );
}

#[gpui_kit::test]
fn a_reopened_film_ready_without_a_system_output_waits_for_one(cx: &mut TestAppContext) {
    // The reopened film turns ready while there is no system output; the
    // output that then appears is another device: the film is opened again
    // on it, not taken as recovered on the one before.
    let f = open(cx, &[(DEVICE, 48_000)]);
    reopen_on_system_output(cx, &f, 100.0);
    let opened = films(&f);
    {
        let mut fake = f.media.0.borrow_mut();
        fake.no_default_output = true;
        fake.film_loading = false;
    }
    f.media.set_now(100.5);
    frame(cx, f.window);
    assert_eq!(films(&f), opened, "kept while there is none");
    f.media.0.borrow_mut().no_default_output = false;
    switch_default_silently(&f);
    f.media.set_now(101.0);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "opened again on the new default");
}

#[gpui_kit::test]
fn a_playback_with_no_system_output_says_so(cx: &mut TestAppContext) {
    // P on the system output with none there: the playback waits, then
    // says there is no output, not that it could not be checked.
    let f = open(cx, &[(DEVICE, 48_000)]);
    record_take(cx, &f);
    f.media.0.borrow_mut().no_default_output = true;
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    for at in [200.1, 200.6, 201.2, 201.3] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(
        message(cx, &f.view),
        "The playback stopped: there is no output to play through."
    );
}

#[gpui_kit::test]
fn a_playback_whose_system_output_cannot_be_read_says_so(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    record_take(cx, &f);
    f.media.0.borrow_mut().lookups_fail = true;
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    for at in [200.1, 200.6, 201.2, 201.3] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(
        message(cx, &f.view),
        "The playback stopped: the output could not be checked."
    );
}

#[gpui_kit::test]
fn a_refused_r_keeps_the_warning_for_the_next(cx: &mut TestAppContext) {
    // R blocked after a warned take: the warning is not used up; the next
    // R that starts still carries it.
    let f = on_test_phones(cx);
    warned_take(cx, &f, 100.0);
    f.view.update(cx, |view, _| {
        view.script_problem = Some("The script has a problem.".to_owned());
    });
    f.media.set_now(101.0);
    press(cx, f.window, &["r"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    f.view.update(cx, |view, _| view.script_problem = None);
    f.media.set_now(102.0);
    press(cx, f.window, &["r"]);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let said = message(cx, &f.view);
    assert_eq!(
        said.matches("Test Phones was disconnected").count(),
        1,
        "carried: {said}"
    );
}

#[gpui_kit::test]
fn a_drop_found_by_the_route_check_updates_the_notice_at_once(cx: &mut TestAppContext) {
    // A film that keeps failing waits out a long pause; the phones go
    // without a CoreAudio event, found when P checks the route: the notice
    // says at once that it waits for them.
    let f = on_test_phones(cx);
    for at in [100.0, 101.2, 103.4, 107.6] {
        f.media.0.borrow_mut().film_failed = Some("Cannot decode".to_owned());
        f.media.set_now(at);
        frame(cx, f.window);
        f.media.set_now(at + 0.1);
        frame(cx, f.window);
    }
    assert!(film_note(cx, &f).contains("check the film file"));
    unplug_quietly(&f, "Test Phones");
    f.media.set_now(108.0);
    press(cx, f.window, &["p"]);
    f.media.set_now(108.1);
    frame(cx, f.window);
    let note = film_note(cx, &f);
    assert!(note.contains("once its output is back"), "{note}");
}

#[gpui_kit::test]
fn the_load_deadline_grows_to_thirty_seconds(cx: &mut TestAppContext) {
    // A film that never loads: given 10, 20, 30 s, and then 30 s again.
    let f = on_test_phones(cx);
    off_and_on(&f, "film");
    f.media.0.borrow_mut().film_loading = true;
    f.media.set_now(100.0);
    frame(cx, f.window);
    // Reopened once the route is confirmed afresh.
    f.media.set_now(100.05);
    frame(cx, f.window);
    let opened = films(&f);
    for at in [110.1, 111.2, 131.3, 133.4, 163.5, 167.6, 197.5] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert_eq!(films(&f), opened + 3);
    f.media.set_now(197.7);
    frame(cx, f.window);
    assert!(film_note(cx, &f).contains("did not load"));
    f.media.set_now(205.8);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 4, "failed after 30 s, not 40");
}

#[gpui_kit::test]
fn a_film_that_fails_in_its_warm_up_backs_off(cx: &mut TestAppContext) {
    // Reopened and ready, it fails as its muted warm-up starts: not the
    // first failure again, so the next try waits.
    let f = on_test_phones(cx);
    off_and_on(&f, "film");
    f.media.set_now(100.0);
    frame(cx, f.window);
    // Reopened once the route is confirmed afresh.
    f.media.set_now(100.05);
    frame(cx, f.window);
    let opened = films(&f);
    f.media.set_now(100.1);
    frame(cx, f.window);
    f.media.0.borrow_mut().film_failed = Some("failed".to_owned());
    f.media.set_now(100.2);
    frame(cx, f.window);
    f.media.set_now(100.25);
    frame(cx, f.window);
    assert!(
        film_note(cx, &f).contains("could not play"),
        "{}",
        film_note(cx, &f)
    );
    f.media.set_now(100.5);
    frame(cx, f.window);
    assert_eq!(films(&f), opened, "not at once");
    f.media.set_now(101.3);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "after a second");
    // Ready, warmed up and settled two seconds after the warm-up: a later
    // failure is a first one again.
    for at in [101.4, 101.5, 101.8, 103.0] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    f.media.0.borrow_mut().film_failed = Some("failed".to_owned());
    f.media.set_now(103.5);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "not settled yet: it waits");
    f.media.set_now(103.55);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "the pause");
    for at in [105.0, 105.1, 105.5, 105.9, 108.0] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    let settled = films(&f);
    f.media.0.borrow_mut().film_failed = Some("failed".to_owned());
    f.media.set_now(110.1);
    frame(cx, f.window);
    f.media.set_now(110.15);
    frame(cx, f.window);
    assert_eq!(films(&f), settled + 1, "at once");
}

#[gpui_kit::test]
fn the_output_going_again_while_reopening_says_it_waits(cx: &mut TestAppContext) {
    // Off and on, reopened; off again while the new player loads: the
    // notice says it waits for the output, not to check the file.
    let f = on_test_phones(cx);
    off_and_on(&f, "film");
    f.media.0.borrow_mut().film_loading = true;
    f.media.set_now(100.0);
    frame(cx, f.window);
    unplug(&f, "Test Phones");
    f.media.0.borrow_mut().film_failed = Some("failed".to_owned());
    for at in [100.3, 105.0, 120.0] {
        f.media.set_now(at);
        frame(cx, f.window);
        let note = film_note(cx, &f);
        assert!(note.contains("once its output is back"), "{at}: {note}");
    }
    plug(&f, "Test Phones");
    f.media.0.borrow_mut().film_loading = false;
    f.media.set_now(121.0);
    frame(cx, f.window);
    f.media.set_now(121.1);
    frame(cx, f.window);
    assert_eq!(film_note(cx, &f), "");
}

#[gpui_kit::test]
fn a_failed_read_of_the_phones_does_not_block_the_system_output(cx: &mut TestAppContext) {
    // Reads of the phones fail while the film has failed; System output is
    // chosen, as the refusal advises: the film is reopened there.
    let f = on_test_phones(cx);
    let opened = films(&f);
    {
        let mut fake = f.media.0.borrow_mut();
        fake.lookups_fail = true;
        fake.film_failed = Some("failed".to_owned());
        fake.generation += 1;
    }
    for k in 0..12 {
        f.media.set_now(100.0 + 0.1 * f64::from(k));
        frame(cx, f.window);
    }
    clear_output_on_disk(cx, &f);
    f.media.0.borrow_mut().lookups_fail = false;
    f.media.set_now(102.0);
    frame(cx, f.window);
    f.media.set_now(104.5);
    frame(cx, f.window);
    assert_eq!(f.media.0.borrow().film_output, None);
    assert_eq!(films(&f), opened + 1, "reopened on the system output");
    press(cx, f.window, &["space"]);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
}

#[gpui_kit::test]
fn space_on_a_film_that_keeps_failing_leaves_the_reason_to_the_notice(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    f.media.0.borrow_mut().film_failed = Some("Cannot decode".to_owned());
    f.media.set_now(100.0);
    frame(cx, f.window);
    f.media.set_now(100.05);
    frame(cx, f.window);
    f.media.0.borrow_mut().film_failed = Some("Cannot decode".to_owned());
    f.media.set_now(100.1);
    frame(cx, f.window);
    f.media.set_now(100.15);
    frame(cx, f.window);
    press(cx, f.window, &["space"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    // The notice gives the reason and the countdown; the status only says
    // to wait for it.
    assert_eq!(
        film_status(cx, &f),
        "The film is not ready; try again once it is back."
    );
    assert!(film_note(cx, &f).contains("could not play (Cannot decode)"));
}

#[gpui_kit::test]
fn a_film_that_keeps_failing_is_said_to_be_unplayable(cx: &mut TestAppContext) {
    // The file, not the output: reopened on a usable output, it fails
    // again. Said with the player's reason, and tried less and less often.
    let f = on_test_phones(cx);
    let opened = films(&f);
    f.media.0.borrow_mut().film_failed = Some("Cannot decode".to_owned());
    f.media.set_now(100.0);
    frame(cx, f.window);
    f.media.set_now(100.05);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "tried once the route is read again");
    f.media.0.borrow_mut().film_failed = Some("Cannot decode".to_owned());
    f.media.set_now(100.1);
    frame(cx, f.window);
    f.media.set_now(100.15);
    frame(cx, f.window);
    let note = film_note(cx, &f);
    assert!(
        note.contains("The film could not play (Cannot decode)"),
        "{note}"
    );
    assert!(note.contains("check the film file"), "{note}");
    f.media.set_now(100.9);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "not before a second");
    f.media.set_now(101.2);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 2, "tried again");
    press(cx, f.window, &["space"]);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
}

#[gpui_kit::test]
fn space_on_a_loading_film_does_not_blame_the_output(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    f.media.0.borrow_mut().film_loading = true;
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(
        film_status(cx, &f),
        "The film is not ready (still loading); try again in a moment."
    );
}

#[gpui_kit::test]
fn r_on_a_loading_film_keeps_the_warning_and_drops_only_the_status(cx: &mut TestAppContext) {
    // R right after a take with a warning, the film still loading: the
    // warning is carried, the film's status said beside it; once the film
    // is ready, only the status goes.
    let f = on_test_phones(cx);
    warned_take(cx, &f, 100.0);
    f.media.0.borrow_mut().film_loading = true;
    f.media.set_now(101.0);
    press(cx, f.window, &["r"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let warning = message(cx, &f.view);
    assert_eq!(
        warning.matches("Test Phones was disconnected").count(),
        1,
        "once: {warning}"
    );
    assert!(film_status(cx, &f).contains("still loading"));
    f.media.0.borrow_mut().film_loading = false;
    f.media.set_now(101.1);
    frame(cx, f.window);
    assert_eq!(film_status(cx, &f), "");
    assert_eq!(message(cx, &f.view), warning);
}

#[gpui_kit::test]
fn p_on_a_film_that_does_not_load_blames_the_film(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    f.media.0.borrow_mut().film_loading = true;
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    for at in [201.0, 202.0, 203.5] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(
        film_status(cx, &f),
        "The film is not ready (still loading); play it again in a moment."
    );
    // Loaded: the status goes, and P plays.
    f.media.0.borrow_mut().film_loading = false;
    f.media.set_now(204.0);
    frame(cx, f.window);
    assert_eq!(film_status(cx, &f), "");
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
}

#[gpui_kit::test]
fn a_reconnect_does_not_hide_a_take_warning(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    f.media.set_now(100.5);
    frame(cx, f.window);
    unplug(&f, "Test Phones");
    f.media.set_now(100.6);
    frame(cx, f.window);
    f.media.set_now(100.7);
    press(cx, f.window, &["space"]);
    let lost = "Test Phones was disconnected during the take";
    assert!(message(cx, &f.view).contains(lost));
    // Back: the output is set again, and the warning stays.
    plug(&f, "Test Phones");
    f.media.set_now(103.0);
    frame(cx, f.window);
    assert_eq!(
        f.media.0.borrow().film_output.as_deref(),
        Some("Test Phones")
    );
    assert!(
        message(cx, &f.view).contains(lost),
        "{}",
        message(cx, &f.view)
    );
}

#[gpui_kit::test]
fn a_film_that_moved_within_its_first_moments_counts_as_played(cx: &mut TestAppContext) {
    // Started at 100.4, read at 100.42: before the usual read-back delay,
    // but the player is asked at once, so the take warns and goes on.
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    unplug(&f, "Test Phones");
    f.media.set_now(100.42);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    // A stop right then, before any tick saw the loss, warns as well.
    let f2 = on_test_phones(cx);
    begin(cx, &f2, 100.0);
    unplug_quietly(&f2, "Test Phones");
    f2.media.set_now(100.42);
    f2.view.update(cx, |view, _| view.finish_take());
    assert!(message(cx, &f2.view).contains("Test Phones was disconnected during the take"));
    f.media.set_now(100.6);
    press(cx, f.window, &["space"]);
    assert!(message(cx, &f.view).contains("Test Phones was disconnected during the take"));
}

#[gpui_kit::test]
fn a_system_output_unnamed_at_the_start_is_still_watched(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    f.media.0.borrow_mut().default_name_fails = true;
    begin(cx, &f, 100.0); // line a: the film runs from 102.4
    f.media.0.borrow_mut().default_name_fails = false;
    f.media.set_now(103.0);
    frame(cx, f.window);
    f.media.0.borrow_mut().default_output = "External Headphones".to_owned();
    f.media.set_now(103.2);
    frame(cx, f.window);
    f.media.set_now(103.4);
    press(cx, f.window, &["space"]);
    // Named once it could be read (the first check), and watched all along.
    let said = message(cx, &f.view);
    assert!(
        said.contains("The system output changed from MacBook Speakers to External Headphones"),
        "{said}"
    );

    // Never named: watched by its object all the same.
    let f = open(cx, &[(DEVICE, 48_000)]);
    f.media.0.borrow_mut().default_name_fails = true;
    begin(cx, &f, 100.0);
    f.media.set_now(103.0);
    frame(cx, f.window);
    f.media.0.borrow_mut().default_output = "External Headphones".to_owned();
    f.media.set_now(103.2);
    frame(cx, f.window);
    f.media.set_now(103.4);
    press(cx, f.window, &["space"]);
    let said = message(cx, &f.view);
    assert!(
        said.contains("The system output changed during the take"),
        "{said}"
    );
}

#[gpui_kit::test]
fn a_route_stop_needs_no_input_buffer(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    unplug_quietly(&f, "Test Phones");
    f.media.set_now(100.1);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert!(message(cx, &f.view).contains("Test Phones is not connected"));
}

#[gpui_kit::test]
fn a_confirmed_device_that_cannot_be_read_is_not_called_disconnected(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0); // d's film scheduled for 100.4
    f.media.0.borrow_mut().lookups_fail = true;
    for n in 2..=9 {
        f.media.set_now(100.0 + 0.1 * f64::from(n));
        frame(cx, f.window);
    }
    // Waiting for it to be checked, it gave up: not "not connected".
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let said = message(cx, &f.view);
    assert!(said.contains("could not be checked"), "{said}");
    assert!(!said.contains("not connected"), "{said}");
}

#[gpui_kit::test]
fn a_film_that_moved_at_the_start_gets_the_warning_not_a_stop(cx: &mut TestAppContext) {
    // Scheduled for 100.4; no frame until 100.5, when the phones are gone:
    // the film is read moving first, so the take goes on and warns.
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    unplug(&f, "Test Phones");
    f.media.set_now(100.5);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    f.media.set_now(100.6);
    press(cx, f.window, &["space"]);
    assert!(message(cx, &f.view).contains("Test Phones was disconnected during the take"));
}

#[gpui_kit::test]
fn a_healthy_route_is_not_judged_by_a_listing(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    f.media.0.borrow_mut().listing_fails = true;
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    // A device change and the poll: the route is checked by its object.
    plug(&f, "Other Speakers");
    for at in [200.1, 202.5, 205.0] {
        f.media.set_now(at);
        frame(cx, f.window);
    }
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
}

#[gpui_kit::test]
fn failed_reads_far_apart_are_not_a_drop(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    let sets = f.media.0.borrow().output_sets.len();
    for at in [200.5, 201.2] {
        // One failed read on a device change, then fine again.
        f.media.0.borrow_mut().lookups_fail = true;
        f.media.0.borrow_mut().generation += 1;
        f.media.set_now(at);
        frame(cx, f.window);
        f.media.0.borrow_mut().lookups_fail = false;
        f.media.set_now(at + 0.05);
        frame(cx, f.window);
    }
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(f.media.0.borrow().output_sets.len(), sets);
}

#[gpui_kit::test]
fn a_start_does_not_wait_for_ever(cx: &mut TestAppContext) {
    // Whatever keeps telling it to wait, a take's film gives up.
    let f = on_test_phones(cx);
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    f.media.send(100.05, 2400);
    settle(cx, &f);
    for n in 1..=14 {
        f.media.set_now(100.0 + 0.1 * f64::from(n));
        f.view.update(cx, |view, _| {
            view.recorder.hold_start(super::recorder::StartHold::Wait);
            let update = view.recorder.tick(&mut *view.media, &view.dir);
            view.show(update);
        });
    }
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert!(message(cx, &f.view).contains("the output could not be checked"));
}

#[gpui_kit::test]
fn a_change_of_system_output_in_the_pre_roll_is_named_once(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    begin(cx, &f, 100.0); // line a: the film starts at 102.4
    f.media.0.borrow_mut().default_output = "External Headphones".to_owned();
    let listings = f.media.0.borrow().listings;
    for n in 1..=10 {
        f.media.set_now(100.1 + 0.2 * f64::from(n));
        frame(cx, f.window);
    }
    assert!(
        f.media.0.borrow().listings <= listings + 1,
        "its name read once"
    );
    f.media.set_now(102.6);
    frame(cx, f.window);
    f.media.set_now(102.7);
    press(cx, f.window, &["space"]);
    assert!(message(cx, &f.view).contains("changed from MacBook Speakers to External Headphones"));
}

#[gpui_kit::test]
fn a_take_reads_its_device_cheaply_every_tick(cx: &mut TestAppContext) {
    // Gone mid-take: no device listing per frame while it stays gone.
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    f.media.set_now(100.5);
    frame(cx, f.window);
    unplug(&f, "Test Phones");
    f.media.set_now(100.6);
    frame(cx, f.window);
    let (listings, lookups) = {
        let fake = f.media.0.borrow();
        (fake.listings, fake.lookups)
    };
    for n in 1..=5 {
        f.media.set_now(100.6 + 0.1 * f64::from(n));
        frame(cx, f.window);
    }
    let fake = f.media.0.borrow();
    assert_eq!(fake.listings, listings, "no listing while it is gone");
    assert!(
        fake.lookups - lookups <= 15,
        "{} lookups in 5 ticks",
        fake.lookups - lookups
    );
    drop(fake);

    // On the system output: no name or UID read per frame.
    let f = open(cx, &[(DEVICE, 48_000)]);
    begin(cx, &f, 100.0);
    let listings = f.media.0.borrow().listings;
    for n in 1..=5 {
        f.media.set_now(100.1 + 0.1 * f64::from(n));
        frame(cx, f.window);
    }
    assert_eq!(f.media.0.borrow().listings, listings);
}

#[gpui_kit::test]
fn a_playing_film_is_set_again_only_when_its_device_dropped(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    let sets = |f: &Fixture| f.media.0.borrow().output_sets.len();
    let before = sets(&f);
    // Another device, or another default output: no rebuild mid-play.
    plug(&f, "Other Speakers");
    frame(cx, f.window);
    f.media.0.borrow_mut().default_output = "External Headphones".to_owned();
    f.media.0.borrow_mut().generation += 1;
    frame(cx, f.window);
    assert_eq!(sets(&f), before);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    // The phones dropped and came back between two ticks: set again, and
    // the playback goes on.
    unplug(&f, "Test Phones");
    plug(&f, "Test Phones");
    frame(cx, f.window);
    assert_eq!(sets(&f), before + 1);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
}

#[gpui_kit::test]
fn a_take_notes_a_drop_between_ticks_and_sets_the_route_again_at_once(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    f.media.set_now(100.5); // the film plays from 100.4
    frame(cx, f.window);
    let sets = |f: &Fixture| f.media.0.borrow().output_sets.len();
    let before = sets(&f);
    // Gone and back between two ticks: noted, and set again at once, so
    // the film does not play out loud for the rest of the take.
    unplug(&f, "Test Phones");
    plug(&f, "Test Phones");
    f.media.set_now(100.6);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(sets(&f), before + 1, "during the take");
    f.media.set_now(100.7);
    press(cx, f.window, &["space"]);
    assert!(message(cx, &f.view).contains("Test Phones was disconnected during the take"));
    // Set already: the next start needs nothing.
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    assert_eq!(sets(&f), before + 1);

    // Gone during a take and back only later: set again once it is back.
    press(cx, f.window, &["p"]);
    begin(cx, &f, 300.0);
    f.media.set_now(300.5);
    frame(cx, f.window);
    unplug(&f, "Test Phones");
    f.media.set_now(300.6);
    frame(cx, f.window);
    assert_eq!(sets(&f), before + 1, "nothing to set while it is gone");
    plug(&f, "Test Phones");
    f.media.set_now(300.7);
    frame(cx, f.window);
    assert_eq!(sets(&f), before + 2, "set as soon as it is back");
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
}

#[gpui_kit::test]
fn a_take_checks_its_output_as_it_stops_and_keeps_the_note_until_saved(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    press(cx, f.window, &["up"]); // c
    // A file where c's takes belong: the first save fails.
    fs::write(f.dir.join("takes/c"), b"").expect("blocker");
    begin(cx, &f, 100.0);
    f.media.set_now(101.0); // c's film plays from 100.9
    frame(cx, f.window);
    // Gone, and stopped before any tick saw it.
    unplug_quietly(&f, "Test Phones");
    f.media.set_now(101.5);
    f.view.update(cx, |view, _| view.finish_take());
    let lost = "Test Phones was disconnected during the take";
    assert!(
        message(cx, &f.view).contains(lost),
        "{}",
        message(cx, &f.view)
    );
    // Saved on a later try: said again with it.
    fs::remove_file(f.dir.join("takes/c")).expect("unblock");
    f.view.update(cx, |view, _| view.message = None);
    f.media.set_now(110.0);
    frame(cx, f.window);
    let said = message(cx, &f.view);
    assert!(
        said.contains("Saved the earlier take of c as take 1."),
        "{said}"
    );
    assert!(said.contains(lost), "{said}");
}

#[gpui_kit::test]
fn a_system_output_take_notes_a_change_at_its_automatic_end(cx: &mut TestAppContext) {
    // Line a: its film plays from 102.4 and the take ends by itself at
    // film 2.5 (its slot and the tail), host 104.9.
    let f = open(cx, &[(DEVICE, 48_000)]);
    begin(cx, &f, 100.0);
    f.media.set_now(103.0);
    frame(cx, f.window);
    // Changed without a report, and the next tick is the take's end.
    f.media.0.borrow_mut().default_output = "External Headphones".to_owned();
    f.media.set_now(105.5);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let said = message(cx, &f.view);
    assert!(
        said.contains("The system output changed from MacBook Speakers to External Headphones"),
        "{said}"
    );
}

#[gpui_kit::test]
fn a_system_output_take_notes_a_change_made_during_its_pre_roll(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    begin(cx, &f, 100.0);
    // Line a's film starts at 102.4: changed before, seen before it plays.
    f.media.0.borrow_mut().default_output = "External Headphones".to_owned();
    f.media.0.borrow_mut().generation += 1;
    f.media.set_now(101.0);
    frame(cx, f.window);
    f.media.set_now(103.0);
    frame(cx, f.window);
    f.media.set_now(103.5);
    press(cx, f.window, &["space"]);
    let said = message(cx, &f.view);
    assert!(
        said.contains("The system output changed from MacBook Speakers to External Headphones"),
        "{said}"
    );
}

#[gpui_kit::test]
fn a_system_output_unread_at_the_start_gives_no_false_note(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    f.media.0.borrow_mut().objects_unreadable = true;
    begin(cx, &f, 100.0);
    f.media.0.borrow_mut().objects_unreadable = false;
    f.media.set_now(103.0);
    frame(cx, f.window);
    f.media.set_now(103.5);
    press(cx, f.window, &["space"]);
    let said = message(cx, &f.view);
    assert!(!said.contains("system output"), "{said}");
}

/// A take of line d whose phones dropped and came back while its film ran,
/// stopped with Space: its result warns.
fn warned_take(cx: &mut TestAppContext, f: &Fixture, at: f64) {
    begin(cx, f, at);
    f.media.set_now(at + 0.5);
    frame(cx, f.window);
    unplug(f, "Test Phones");
    plug(f, "Test Phones");
    f.media.set_now(at + 0.6);
    frame(cx, f.window);
    f.media.set_now(at + 0.7);
    press(cx, f.window, &["space"]);
    assert!(message(cx, &f.view).contains("Test Phones was disconnected during the take"));
}

#[gpui_kit::test]
fn a_take_warning_is_carried_only_into_a_retake(cx: &mut TestAppContext) {
    let lost = "Test Phones was disconnected during the take";
    // Any other action first: not carried.
    for other in [&["p", "p"][..], &["a"], &["-"], &["k"], &["c", "escape"]] {
        let f = on_test_phones(cx);
        warned_take(cx, &f, 100.0);
        f.media.set_now(200.0);
        press(cx, f.window, other);
        press(cx, f.window, &["r"]);
        assert!(
            !message(cx, &f.view).contains(lost),
            "{other:?}: {}",
            message(cx, &f.view)
        );
    }

    // A menu command first (File › Open Project…): not carried either.
    let f = on_test_phones(cx);
    warned_take(cx, &f, 100.0);
    cx.dispatch_action(f.window, super::start::OpenProject);
    cx.simulate_path_prompt_response(|_| None);
    press(cx, f.window, &["r"]);
    assert!(
        !message(cx, &f.view).contains(lost),
        "{}",
        message(cx, &f.view)
    );

    // R refused, the output gone again: the warning and the refusal.
    let f = on_test_phones(cx);
    warned_take(cx, &f, 100.0);
    unplug(&f, "Test Phones");
    f.media.set_now(101.0);
    press(cx, f.window, &["r"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let said = message(cx, &f.view);
    assert!(said.starts_with(&format!("Take 2 of d: {lost}")), "{said}");
    assert!(
        said.contains("Test Phones is not connected, so Takewise does not record"),
        "{said}"
    );
}

#[gpui_kit::test]
fn a_take_warning_stays_through_a_retake(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    f.media.set_now(100.5);
    frame(cx, f.window);
    unplug(&f, "Test Phones");
    plug(&f, "Test Phones");
    f.media.set_now(100.6);
    frame(cx, f.window);
    // R: the take ends and the next starts at once.
    f.media.set_now(100.7);
    press(cx, f.window, &["r"]);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let lost = "Test Phones was disconnected during the take";
    assert!(
        message(cx, &f.view).starts_with(&format!("Take 2 of d: {lost}")),
        "{}",
        message(cx, &f.view)
    );
    // Until the next take's result, which has none.
    f.media.send(100.75, 2400);
    settle(cx, &f);
    f.media.set_now(101.5);
    frame(cx, f.window);
    press(cx, f.window, &["space"]);
    assert!(
        !message(cx, &f.view).contains(lost),
        "{}",
        message(cx, &f.view)
    );
}

#[gpui_kit::test]
fn a_take_on_the_system_output_notes_another_default(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d
    begin(cx, &f, 100.0);
    f.media.0.borrow_mut().default_output = "External Headphones".to_owned();
    f.media.0.borrow_mut().generation += 1;
    frame(cx, f.window);
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    let said = message(cx, &f.view);
    assert!(
        said.contains("The system output changed from MacBook Speakers to External Headphones during the take: listen before you choose it."),
        "{said}"
    );

    // Unchanged: nothing said.
    f.media.set_now(200.0);
    begin(cx, &f, 200.0);
    f.media.set_now(200.5);
    press(cx, f.window, &["space"]);
    assert!(
        !message(cx, &f.view).contains("system output"),
        "{}",
        message(cx, &f.view)
    );
}

#[gpui_kit::test]
fn an_output_that_does_not_list_its_configuration_is_still_there(cx: &mut TestAppContext) {
    // Bluetooth headphones switching profile can drop out of the list
    // while the device is found by name.
    let f = on_test_phones(cx);
    unplug(&f, "Test Phones");
    f.media
        .0
        .borrow_mut()
        .hidden_outputs
        .push("Test Phones".to_owned());
    f.media.set_now(103.0);
    frame(cx, f.window);
    assert_eq!(f.view.read_with(cx, |view, _| view.output_note()), None);
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
}

#[gpui_kit::test]
fn a_window_that_is_not_drawn_still_ends_a_playback_and_watches_its_output(
    cx: &mut TestAppContext,
) {
    // Minimized or hidden, macOS draws no frames: the timer ticks instead.
    let f = on_test_phones(cx);
    let no_frames = |cx: &mut TestAppContext| {
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(100));
        cx.run_until_parked();
    };
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    f.media.set_now(400.0);
    no_frames(cx);
    assert!(
        !f.view.read_with(cx, |view, _| view.recorder.is_playing()),
        "ended without a frame"
    );

    f.media.set_now(500.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    unplug(&f, "Test Phones");
    f.media.set_now(500.2);
    no_frames(cx);
    assert!(
        !f.view.read_with(cx, |view, _| view.recorder.is_playing()),
        "stopped without a frame"
    );
    assert!(message(cx, &f.view).starts_with("The playback stopped. Test Phones is not connected"));
}

#[gpui_kit::test]
fn a_take_watches_the_output_it_started_on(cx: &mut TestAppContext) {
    let lost = "Test Phones was disconnected during the take";
    // [output] device cleared mid-take, then the phones go: still lost.
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    f.media.set_now(100.5); // the film plays
    frame(cx, f.window);
    clear_output_on_disk(cx, &f);
    unplug(&f, "Test Phones");
    frame(cx, f.window);
    // Back before the take ends: it was lost all the same.
    plug(&f, "Test Phones");
    frame(cx, f.window);
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    assert_eq!(recorded(&f.dir).len(), 2);
    assert!(
        message(cx, &f.view).contains(lost),
        "{}",
        message(cx, &f.view)
    );
    // Said with the take, and not taken back by what follows.
    f.media.set_now(103.0);
    frame(cx, f.window);
    assert!(
        message(cx, &f.view).contains(lost),
        "{}",
        message(cx, &f.view)
    );

    // Another output, missing, chosen in the file mid-take: the take's
    // own output is still there, so no warning.
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    tw::set_route(&f.dir, None, Some("Gone Phones")).expect("output");
    f.view.update(cx, |view, _| view.reload());
    plug(&f, "Other Speakers");
    f.media.set_now(102.5);
    frame(cx, f.window);
    f.media.set_now(102.6);
    press(cx, f.window, &["space"]);
    assert_eq!(recorded(&f.dir).len(), 2);
    assert!(
        !message(cx, &f.view).contains("disconnected"),
        "{}",
        message(cx, &f.view)
    );
}

#[gpui_kit::test]
fn an_output_lost_during_a_take_keeps_the_take_and_says_so(cx: &mut TestAppContext) {
    let f = on_test_phones(cx);
    begin(cx, &f, 100.0);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    f.media.set_now(100.5); // the film is seen playing
    frame(cx, f.window);
    unplug(&f, "Test Phones");
    f.media.set_now(102.5);
    frame(cx, f.window);
    assert!(
        f.view.read_with(cx, |view, _| view.recorder.is_recording()),
        "it records on"
    );
    f.media.set_now(102.6);
    press(cx, f.window, &["space"]);
    assert_eq!(recorded(&f.dir).len(), 2, "the take is kept");
    let said = message(cx, &f.view);
    assert!(
        said.contains(
            "Test Phones was disconnected during the take, so the film may have played elsewhere"
        ),
        "{said}"
    );
}

#[gpui_kit::test]
fn an_output_changed_on_disk_is_applied(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    assert_eq!(f.media.0.borrow().film_output, None, "the system output");
    tw::set_route(&f.dir, None, Some("External Headphones")).expect("output");
    f.view.update(cx, |view, _| view.reload());
    assert_eq!(
        f.media.0.borrow().film_output.as_deref(),
        Some("External Headphones")
    );
    // Back to none: the system output again.
    let path = f.dir.join("project.toml");
    let text = fs::read_to_string(&path).expect("project.toml");
    let text = text.replace("device = \"External Headphones\"\n", "");
    fs::write(&path, text).expect("project.toml");
    f.view.update(cx, |view, _| view.reload());
    assert_eq!(config(&f.dir).output.device, None);
    assert_eq!(f.media.0.borrow().film_output, None);
}

#[gpui_kit::test]
fn recorded_input_round_trips(cx: &mut TestAppContext) {
    // A take committed outside the window shows up after a reload.
    let f = open(cx, &[(DEVICE, 48_000)]);
    let snapshot = Project::open(&f.dir)
        .expect("project")
        .snapshot("a")
        .expect("snapshot");
    let input = RecordedInput {
        sample_rate: 48_000,
        device: DEVICE.to_owned(),
        channel: 1,
        line_offset_s: -2.0,
        snapshot,
        dropouts: None,
        film_shift_ms: None,
        passage: None,
    };
    let partial = tw::begin_take(&f.dir, "a", &input).expect("begin");
    tw::write_take_wav(&partial.wav, &[0; 480], 48_000).expect("wav");
    tw::commit_take(&f.dir, partial).expect("commit");
    f.view.update(cx, |view, _| view.reload());
    f.view.read_with(cx, |view, _| {
        assert_eq!(view.line_state().map(|line| line.takes.len()), Some(1));
        assert_eq!(view.take, Some(1));
    });
}

#[gpui_kit::test]
fn a_live_take_is_streamed_to_disk_and_never_adopted(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    f.media.send(100.2, 48_000);
    f.media.send(101.2, 4800);
    settle(cx, &f);
    f.media.set_now(101.3);
    frame(cx, f.window); // places the take and begins it on disk
    wait_for(cx, &f.view, |view| {
        view.recorder
            .progress()
            .is_some_and(|p| p.flushed >= 52_800)
    });
    let wav = partial_wav(&f.dir, "a").expect("the partial WAV exists while recording");
    let (samples, _) = tw::read_take_wav(&wav).expect("a valid WAV up to the last flush");
    assert_eq!(samples.len(), 52_800);

    let report = tw::repair(&f.dir).expect("repair");
    assert!(report.adopted.is_empty(), "a live take is never adopted");
    assert!(wav.is_file());

    press(cx, f.window, &["space"]);
    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 1);
    assert_eq!(takes[0].2.samples, 52_800);
    assert!(!takes[0].2.recovered);
    assert_eq!(takes[0].2.dropouts, Some(0));
}

#[gpui_kit::test]
fn a_take_survives_when_its_metadata_cannot_be_written(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    // A file where the line's take directory belongs.
    fs::create_dir_all(f.dir.join("takes")).expect("takes");
    fs::write(f.dir.join("takes/a"), b"").expect("blocker");
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    f.media.send(100.2, 4800);
    settle(cx, &f);
    f.media.set_now(100.3);
    frame(cx, f.window);
    let shown = message(cx, &f.view);
    assert!(shown.contains("kept in memory"), "{shown}");
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    f.media.send(100.3, 4800);

    fs::remove_file(f.dir.join("takes/a")).expect("unblock");
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 1, "written from memory on stop");
    assert_eq!(takes[0].2.samples, 9600);
}

#[gpui_kit::test]
fn an_unsaved_take_is_kept_and_saved_later(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    fs::create_dir_all(f.dir.join("takes")).expect("takes");
    fs::write(f.dir.join("takes/a"), b"").expect("blocker");
    record_take(cx, &f);
    assert_eq!(recorded(&f.dir).len(), 0);
    let shown = message(cx, &f.view);
    assert!(shown.contains("could not be saved"), "{shown}");
    assert!(f.view.read_with(cx, |view, _| view.recorder.has_unsaved()));

    fs::remove_file(f.dir.join("takes/a")).expect("unblock");
    f.media.set_now(110.0);
    frame(cx, f.window);
    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 1, "saved on retry");
    assert_eq!(takes[0].2.samples, 2400);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.has_unsaved()));
}

#[gpui_kit::test]
fn the_window_stays_open_while_a_take_is_only_in_memory(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    // A file where the line's take directory belongs: the take can be
    // neither begun nor saved on disk.
    fs::create_dir_all(f.dir.join("takes")).expect("takes");
    fs::write(f.dir.join("takes/a"), b"").expect("blocker");
    record_take(cx, &f);
    assert_eq!(recorded(&f.dir).len(), 0);

    // Quitting and closing the window are both refused, and the window
    // says why and what to do.
    assert!(cx.update(super::holds_unsaved_audio));
    assert!(!VisualTestContext::from_window(f.window, cx).simulate_close());
    let shown = message(cx, &f.view);
    assert!(
        shown.starts_with("Not closing: 1 take is only in memory, not saved yet ("),
        "{shown}"
    );
    assert!(
        shown.contains("keeps trying to save every 2 s; free disk space or fix the problem"),
        "{shown}"
    );

    // Saving goes on being retried; once it worked, the window closes.
    fs::remove_file(f.dir.join("takes/a")).expect("unblock");
    f.media.set_now(110.0);
    frame(cx, f.window);
    assert_eq!(recorded(&f.dir).len(), 1);
    assert!(!cx.update(super::holds_unsaved_audio));
    assert!(VisualTestContext::from_window(f.window, cx).simulate_close());
}

#[gpui_kit::test]
fn a_take_complete_on_disk_does_not_hold_the_window(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    begin(cx, &f, 100.0); // streamed into its partial files
    super::recorder::commits_fail(true);
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    assert!(f.view.read_with(cx, |view, _| view.recorder.has_unsaved()));
    // Its audio and metadata are complete on disk, so the next start
    // adopts it: closing loses nothing.
    assert!(VisualTestContext::from_window(f.window, cx).simulate_close());
    super::recorder::commits_fail(false);
    // Once the process is gone, its lock with it, a repair adopts it.
    f.view.update(cx, |view, _| {
        view.recorder = super::recorder::Recorder::new();
    });
    let report = tw::repair(&f.dir).expect("repair");
    assert_eq!(report.adopted.len(), 1, "{report:?}");
    assert_eq!(recorded(&f.dir)[0].2.samples, 2400);
}

#[gpui_kit::test]
fn opening_the_window_adopts_a_crashed_take(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        let snapshot = Project::open(dir)
            .expect("project")
            .snapshot("a")
            .expect("snapshot");
        let input = RecordedInput {
            sample_rate: 48_000,
            device: DEVICE.to_owned(),
            channel: 1,
            line_offset_s: -2.0,
            snapshot,
            dropouts: None,
            film_shift_ms: None,
            passage: None,
        };
        let partial = tw::begin_take(dir, "a", &input).expect("begin");
        let mut wav = tw::PartialWav::create(&partial.wav, 48_000).expect("wav");
        wav.write(&vec![5; 4800]).expect("write");
        wav.flush().expect("flush");
        wav.write(&vec![6; 480]).expect("write");
        // The process dies here: no finish, no commit.
    });
    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 1);
    assert!(takes[0].2.recovered);
    assert_eq!(takes[0].2.samples, 5280);
    let shown = message(cx, &f.view);
    assert!(shown.contains("Recovered a take 1"), "{shown}");
    f.view
        .read_with(cx, |view, _| assert_eq!(view.take, Some(1)));
}

#[gpui_kit::test]
fn a_dropout_is_filled_and_counted(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    begin(cx, &f, 100.0); // first buffer: 100.05 .. 100.1
    f.media.send(100.1, 2400); // .. 100.15
    f.media.send(100.2, 2400); // 50 ms lost before it
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    let takes = recorded(&f.dir);
    assert_eq!(takes[0].2.samples, 2400 * 4);
    assert_eq!(takes[0].2.dropouts, Some(1));
    let wav = f.dir.join("takes/a/001.wav");
    let (samples, _) = tw::read_take_wav(&wav).expect("take");
    assert_eq!(&samples[4800..7200], &vec![0; 2400][..]);
    assert_eq!(samples[7200], 100_000);
}

#[gpui_kit::test]
fn an_interruption_is_shown_with_the_saved_take(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    f.media.send(100.2, 4800);
    settle(cx, &f);
    f.media.0.borrow_mut().capture_error = Some("the device was unplugged".to_owned());
    f.media.set_now(100.5);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(recorded(&f.dir).len(), 1);
    let shown = message(cx, &f.view);
    assert!(shown.contains("the device was unplugged"), "{shown}");
    assert!(shown.contains("Recorded a take 1"), "{shown}");
}

#[gpui_kit::test]
fn a_dropout_is_shown_after_the_take(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d
    begin(cx, &f, 100.0); // first sample at film 5.6
    f.media.send(100.1, 2400);
    f.media.send(101.2, 2400); // lost from film 5.7 on
    f.media.set_now(101.5);
    press(cx, f.window, &["space"]);
    let shown = message(cx, &f.view);
    assert!(shown.contains("Recorded d take 1"), "{shown}");
    assert!(shown.contains("Audio dropout at 0:05"), "{shown}");
}

#[gpui_kit::test]
fn mark_during_a_take_keeps_its_film_position(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d: start 8.0
    begin(cx, &f, 100.0); // first sample at film 5.6; film 6.0 at host 100.4
    f.media.set_now(103.4); // the playhead is at 9.0
    press(cx, f.window, &["m"]);
    assert_eq!(start_of(&f.dir, "d"), 9.0);
    f.media.set_now(103.5);
    press(cx, f.window, &["space"]);

    let takes = recorded(&f.dir);
    let event = &takes[0].2;
    assert_eq!(event.snapshot.start, 9.0, "the line as it is at commit");
    assert!(
        close(event.line_offset_s, -3.4),
        "the take still starts at film 5.6: {}",
        event.line_offset_s
    );
}

/// Presses E on the current line, lets `between` change the project (then
/// the window reloads, as its watcher would), types `text` and presses Enter.
fn edit_with_reload(cx: &mut TestAppContext, f: &Fixture, text: &str, between: impl FnOnce(&Path)) {
    press(cx, f.window, &["e"]);
    between(&f.dir);
    f.view.update(cx, |view, _| view.reload());
    press(cx, f.window, &["cmd-a"]);
    act(cx, f.window, |window, cx| window.input(text, cx));
    press(cx, f.window, &["enter"]);
}

#[gpui_kit::test]
fn an_edit_is_saved_against_the_version_it_began_with(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    edit_with_reload(cx, &f, "New words.", |dir| {
        tw::set_line_field(dir, "c", Field::Text("Agent text.".to_owned()), None).expect("edit");
    });
    let script = Project::open(&f.dir).expect("project").script;
    assert_eq!(script.lines[0].text, "First line.", "the draft is refused");
    assert_eq!(script.lines[2].text, "Agent text.");
    let shown = message(cx, &f.view);
    assert!(shown.contains("changed on disk"), "{shown}");
}

#[gpui_kit::test]
fn an_edit_of_a_removed_line_is_refused(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    edit_with_reload(cx, &f, "New words.", |dir| {
        let without_a = SCRIPT.replacen(
            "[[line]]\nid = \"a\"\nstart = 0.0\ntext = \"First line.\"\n\n",
            "",
            1,
        );
        assert_ne!(without_a, SCRIPT);
        fs::write(dir.join("script.toml"), without_a).expect("script");
    });
    let script = Project::open(&f.dir).expect("project").script;
    assert_eq!(script.lines[0].id, "b");
    assert_eq!(
        script.lines[0].text, "Second line.",
        "never written to another line"
    );
    let shown = message(cx, &f.view);
    assert!(shown.contains("no longer in the script"), "{shown}");
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn the_film_clock_follows_the_players_timebase() {
    let mut media = FakeMedia::new(&[(DEVICE, 48_000)]);
    let mut clock = FilmClock::new();
    clock.start(&mut media, 6.0, 0.0).expect("start");
    let (film_s, host_s) = media.0.borrow().plays.last().copied().expect("a play");
    assert!(
        close(film_s, 6.0) && close(host_s, 100.3),
        "{film_s} {host_s}"
    );
    // Before the start: the scheduled mapping, also for earlier samples.
    assert!(close(clock.film_at(99.3).expect("mapped"), 5.0));

    // The player really started 20 ms late.
    media.0.borrow_mut().timebase = Some(Timebase {
        rate: 1.0,
        film_anchor_s: 6.0,
        host_anchor_s: 100.32,
    });
    media.set_now(100.4);
    clock.track(&media);
    assert!(close(clock.film_at(100.4).expect("mapped"), 6.08));
    assert!(
        close(clock.film_at(100.31).expect("mapped"), 5.99),
        "from the start on"
    );
    assert!(
        close(clock.film_at(100.2).expect("mapped"), 5.88),
        "the count-in follows the read-back start"
    );

    // A stall at film 7.0, then playback goes on.
    media.0.borrow_mut().timebase = Some(Timebase {
        rate: 0.0,
        film_anchor_s: 7.0,
        host_anchor_s: 101.32,
    });
    media.set_now(101.5);
    clock.track(&media);
    assert!(close(clock.film_at(101.9).expect("mapped"), 7.0));
    media.0.borrow_mut().timebase = Some(Timebase {
        rate: 1.0,
        film_anchor_s: 7.0,
        host_anchor_s: 102.0,
    });
    media.set_now(102.5);
    clock.track(&media);
    assert!(close(clock.film_at(102.5).expect("mapped"), 7.5));
}

#[gpui_kit::test]
fn the_film_starts_once_capture_delivers(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d: pre-roll from 6.0
    let plays_before = f.media.0.borrow().plays.len();
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    f.media.set_now(100.2);
    frame(cx, f.window);
    assert_eq!(
        f.media.0.borrow().plays.len(),
        plays_before,
        "no film before audio"
    );
    assert!(
        f.view
            .read_with(cx, |view, _| view.recorder.count_in(&*view.media).is_none())
    );

    f.media.send(100.25, 2400);
    settle(cx, &f);
    f.media.set_now(100.3);
    frame(cx, f.window);
    let (film_s, host_s) = last_play(&f);
    assert!(
        close(film_s, 6.0) && close(host_s, 100.6),
        "{film_s} {host_s}"
    );
}

#[gpui_kit::test]
fn the_default_count_in_gives_each_digit_a_second(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| set_preroll(dir, 3.0));
    press(cx, f.window, &["down", "down", "down"]); // d: start 8.0, pre-roll from 5.0
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    f.media.send(100.25, 2400);
    settle(cx, &f);
    f.media.set_now(100.3);
    frame(cx, f.window);
    let (film_s, host_s) = last_play(&f);
    assert!(
        close(film_s, 5.0) && close(host_s, 100.6),
        "{film_s} {host_s}"
    );
    // The line starts at host 103.6; the digit counts the seconds left.
    let count_at = |cx: &mut TestAppContext, now: f64| {
        f.media.set_now(now);
        f.view
            .read_with(cx, |view, _| view.recorder.count_in(&*view.media))
    };
    assert_eq!(count_at(cx, 100.4), None, "before the film starts");
    assert_eq!(count_at(cx, 100.65), Some(3));
    assert_eq!(count_at(cx, 101.55), Some(3));
    assert_eq!(count_at(cx, 101.65), Some(2));
    assert_eq!(count_at(cx, 102.55), Some(2));
    assert_eq!(count_at(cx, 102.65), Some(1));
    assert_eq!(count_at(cx, 103.55), Some(1));
    assert_eq!(count_at(cx, 103.65), None, "the line has started");
}

#[gpui_kit::test]
fn a_longer_pre_roll_still_counts_from_three(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| set_preroll(dir, 5.0));
    press(cx, f.window, &["down", "down", "down"]); // d: start 8.0, pre-roll from 3.0
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    f.media.send(100.25, 2400);
    settle(cx, &f);
    f.media.set_now(100.3);
    frame(cx, f.window);
    // The line starts at host 105.6.
    let count_at = |cx: &mut TestAppContext, now: f64| {
        f.media.set_now(now);
        f.view
            .read_with(cx, |view, _| view.recorder.count_in(&*view.media))
    };
    assert_eq!(count_at(cx, 101.0), None);
    assert_eq!(count_at(cx, 102.55), None);
    assert_eq!(count_at(cx, 102.65), Some(3));
    assert_eq!(count_at(cx, 104.65), Some(1));
}

#[gpui_kit::test]
fn a_take_without_audio_stops_with_a_message(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    f.media.set_now(103.0);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let shown = message(cx, &f.view);
    assert!(shown.contains("No audio"), "{shown}");
    assert_eq!(recorded(&f.dir).len(), 0);
}

#[gpui_kit::test]
fn the_take_follows_the_film_as_it_really_plays(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down"]); // c: 1.5 .. 8.0, stop at film 9.5
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    f.media.send(100.05, 2400);
    settle(cx, &f);
    f.media.set_now(100.1);
    frame(cx, f.window); // film 0 held 0.5 s: plays from host 100.9
    let (film_s, host_s) = last_play(&f);
    assert!(
        close(film_s, 0.0) && close(host_s, 100.9),
        "{film_s} {host_s}"
    );
    // A frame reads the film playing.
    f.media.set_now(101.0);
    frame(cx, f.window);
    // The film stalls at 5.0 (host 105.9) and stays there.
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 0.0,
        film_anchor_s: 5.0,
        host_anchor_s: 105.9,
    });
    f.media.set_now(106.0);
    frame(cx, f.window);
    let playhead = f
        .view
        .read_with(cx, |view, _| view.recorder.playhead_s(&*view.media));
    assert!(close(playhead.expect("playing"), 5.0), "{playhead:?}");
    // By the schedule the tail ended at host 110.4; the stalled film has not.
    f.media.set_now(111.0);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    // Playback goes on from 5.0 at host 111.0: film 9.5 at host 115.5.
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 1.0,
        film_anchor_s: 5.0,
        host_anchor_s: 111.0,
    });
    f.media.set_now(115.4);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    f.media.set_now(115.6);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(recorded(&f.dir).len(), 1);
}

#[gpui_kit::test]
fn a_take_of_the_last_line_stops_after_the_films_end(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    // d is the last line: its slot runs to the film's end (20.0), so slot
    // end + tail (21.5) is never reached; film 6.0 at host 100.4.
    press(cx, f.window, &["down", "down", "down"]);
    begin(cx, &f, 100.0);
    // The film reaches its end at host 114.4, and the player halts there.
    f.media.set_now(114.4);
    frame(cx, f.window);
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 0.0,
        film_anchor_s: 20.0,
        host_anchor_s: 114.4,
    });
    f.media.set_now(114.5);
    frame(cx, f.window);
    f.media.set_now(115.85);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    // tail_s (1.5) of host time after the film's end.
    f.media.set_now(115.95);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(recorded(&f.dir).len(), 1);
}

#[gpui_kit::test]
fn the_halt_at_the_films_end_is_no_stall(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d, the last line
    begin(cx, &f, 100.0); // film 6.0 at host 100.4; film 20.0 at 114.4
    f.media.send(100.1, 758_400); // audio up to host 115.9
    f.media.set_now(114.39);
    frame(cx, f.window);
    // The player halts at the film's end; the next frame, 12 ms after the
    // end, sees it.
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 0.0,
        film_anchor_s: 20.0,
        host_anchor_s: 114.4,
    });
    f.media.set_now(114.412);
    frame(cx, f.window);
    settle(cx, &f);
    f.media.set_now(115.95);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let takes = recorded(&f.dir);
    assert_eq!(takes[0].2.film_shift_ms, None);
    let shown = message(cx, &f.view);
    assert!(!shown.contains("stalled"), "{shown}");
}

#[gpui_kit::test]
fn a_late_start_moves_the_take(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d: start 8.0
    begin(cx, &f, 100.0); // scheduled: film 6.0 at host 100.4
    // The player really starts 40 ms late.
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 1.0,
        film_anchor_s: 6.0,
        host_anchor_s: 100.44,
    });
    f.media.set_now(100.5);
    frame(cx, f.window);
    f.media.set_now(100.6);
    press(cx, f.window, &["space"]);
    let takes = recorded(&f.dir);
    let event = &takes[0].2;
    // The first sample (host 100.0, before the start) is placed with the
    // read-back timebase: film 5.56, not the scheduled 5.6.
    assert!(
        (event.line_offset_s - -2.44).abs() < 1e-6,
        "{}",
        event.line_offset_s
    );
    assert_eq!(event.film_shift_ms, None, "nothing changed after the start");
}

#[gpui_kit::test]
fn a_player_still_stopped_at_the_first_read_back_waits_for_it_to_play(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d: start 8.0
    begin(cx, &f, 100.0); // scheduled: film 6.0 at host 100.4
    // The first read-back finds the player still stopped at film 6.0.
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 0.0,
        film_anchor_s: 6.0,
        host_anchor_s: 100.45,
    });
    f.media.set_now(100.45);
    frame(cx, f.window);
    // Then it plays, 40 ms late.
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 1.0,
        film_anchor_s: 6.0,
        host_anchor_s: 100.44,
    });
    f.media.set_now(100.5);
    frame(cx, f.window);
    f.media.set_now(100.9);
    press(cx, f.window, &["space"]);
    let takes = recorded(&f.dir);
    let event = &takes[0].2;
    assert!(
        (event.line_offset_s - -2.44).abs() < 1e-6,
        "placed 40 ms later: {}",
        event.line_offset_s
    );
    assert_eq!(event.film_shift_ms, None, "no shift from the stopped read");
    let shown = message(cx, &f.view);
    assert!(!shown.contains("stalled"), "{shown}");
}

#[gpui_kit::test]
fn a_player_that_never_plays_flags_the_take(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d: start 8.0
    begin(cx, &f, 100.0); // scheduled: film 6.0 at host 100.4
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 0.0,
        film_anchor_s: 6.0,
        host_anchor_s: 100.45,
    });
    f.media.set_now(100.45);
    frame(cx, f.window);
    f.media.set_now(100.7);
    press(cx, f.window, &["space"]);
    let takes = recorded(&f.dir);
    let event = &takes[0].2;
    assert!(
        close(event.line_offset_s, -2.4),
        "the scheduled mapping stays: {}",
        event.line_offset_s
    );
    let shift = event.film_shift_ms.expect("flagged");
    assert!(
        (shift - 300.0).abs() < 1e-6,
        "the lateness at stop: {shift}"
    );
}

#[gpui_kit::test]
fn a_film_that_does_not_start_stops_the_take_and_keeps_it(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d: start 8.0
    begin(cx, &f, 100.0); // scheduled: film 6.0 at host 100.4
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 0.0,
        film_anchor_s: 6.0,
        host_anchor_s: 100.45,
    });
    f.media.set_now(101.3);
    frame(cx, f.window);
    assert!(
        f.view.read_with(cx, |view, _| view.recorder.is_recording()),
        "still waiting within a second of the scheduled start"
    );
    f.media.set_now(101.41);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 1, "the audio is kept as a take");
    let event = &takes[0].2;
    assert_eq!(event.samples, 2400);
    let shift = event.film_shift_ms.expect("flagged");
    assert!((shift - 1010.0).abs() < 1e-6, "{shift}");
    let shown = message(cx, &f.view);
    assert!(
        shown.contains("the film did not start; check the film file"),
        "{shown}"
    );
}

#[gpui_kit::test]
fn a_film_that_cannot_play_keeps_the_audio_as_a_take(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d: start 8.0
    f.media.0.borrow_mut().play_error = Some("no output device".to_owned());
    begin(cx, &f, 100.0); // the film would have played 6.0 at host 100.4
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 1, "the audio is kept as a take");
    let event = &takes[0].2;
    assert_eq!(event.samples, 2400);
    assert!(
        close(event.line_offset_s, -2.4),
        "placed by the scheduled mapping: {}",
        event.line_offset_s
    );
    assert_eq!(event.film_shift_ms, None, "stopped before the film was due");
    let shown = message(cx, &f.view);
    assert!(
        shown.contains("the film cannot play: no output device"),
        "{shown}"
    );
    assert!(shown.contains("Recorded d take 1"), "{shown}");
}

#[gpui_kit::test]
fn stopping_before_the_film_was_scheduled_keeps_the_audio(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d: start 8.0, pre-roll from 6.0
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    // The first buffer arrives, but the window stops (Space, or closing)
    // before a frame scheduled the film.
    f.media.send(100.05, 2400);
    settle(cx, &f);
    f.media.set_now(100.06);
    f.view.update(cx, |view, _| view.finish_take());
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 1, "the audio is kept as a take");
    let event = &takes[0].2;
    assert_eq!(event.samples, 2400);
    // Placed as the film would have been scheduled when the first buffer
    // arrived: film 6.0 at host 100.35, so the first sample (host 100.0)
    // is film 5.65, 2.35 s before the line.
    assert!(
        close(event.line_offset_s, -2.35),
        "placed by the estimated mapping: {}",
        event.line_offset_s
    );
    let shown = message(cx, &f.view);
    assert!(
        shown.contains("Stopped before the film started; placement estimated."),
        "{shown}"
    );
    assert!(!shown.contains("nothing was recorded"), "{shown}");
    assert!(shown.contains("Recorded d take 1"), "{shown}");
}

#[gpui_kit::test]
fn a_stall_during_the_take_is_recorded(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d
    begin(cx, &f, 100.0); // first sample at film 5.6; film 6.0 at host 100.4
    f.media.send(100.1, 91_200); // audio up to host 102.0
    f.media.set_now(100.5);
    frame(cx, f.window);
    // The film stalls at 7.0 for 100 ms.
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 0.0,
        film_anchor_s: 7.0,
        host_anchor_s: 101.4,
    });
    f.media.set_now(101.4);
    frame(cx, f.window);
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 1.0,
        film_anchor_s: 7.0,
        host_anchor_s: 101.5,
    });
    f.media.set_now(101.5);
    frame(cx, f.window);
    settle(cx, &f);
    f.media.set_now(102.0);
    press(cx, f.window, &["space"]);
    let takes = recorded(&f.dir);
    let event = &takes[0].2;
    assert!(close(event.line_offset_s, -2.4), "{}", event.line_offset_s);
    let shift = event.film_shift_ms.expect("a shift is recorded");
    assert!((shift - 100.0).abs() < 1e-6, "{shift}");
    let shown = message(cx, &f.view);
    assert!(
        shown.contains("film stalled during the take; placement may be off by 100 ms."),
        "{shown}"
    );
}

#[gpui_kit::test]
fn a_text_edit_during_a_take_keeps_the_text_it_was_read_from(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d: start 8.0
    begin(cx, &f, 100.0);
    // An agent changes the text and the start while the narrator reads.
    tw::set_line_field(&f.dir, "d", Field::Text("Agent text.".to_owned()), None).expect("edit");
    tw::set_line_field(&f.dir, "d", Field::Start(9.0), None).expect("move");
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);

    let takes = recorded(&f.dir);
    let snapshot = &takes[0].2.snapshot;
    assert_eq!(
        snapshot.text, "Fourth line.",
        "the text read during the take"
    );
    assert_eq!(snapshot.start, 9.0, "the slot as it is at commit");
    f.view.update(cx, |view, _| view.reload());
    f.view.read_with(cx, |view, _| {
        let take = &view.line_state().expect("line").takes[0];
        assert!(take.recorded_against_old_text, "{take:?}");
    });
}

#[gpui_kit::test]
fn the_prompter_keeps_the_takes_text_until_it_stops(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d
    begin(cx, &f, 100.0);
    tw::set_line_field(&f.dir, "d", Field::Text("Agent text.".to_owned()), None).expect("edit");
    f.view.update(cx, |view, _| view.reload());
    f.media.set_now(100.3);
    frame(cx, f.window);
    let shown = |cx: &mut TestAppContext| {
        f.view
            .read_with(cx, |view, _| view.prompter_text().map(str::to_owned))
    };
    assert_eq!(
        shown(cx).as_deref(),
        Some("Fourth line."),
        "while recording"
    );
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    assert_eq!(shown(cx).as_deref(), Some("Agent text."), "after the take");
}

/// Records line `c` while `change` rewrites the script (and the window
/// reloads, as its watcher would); returns what the prompter showed during
/// the take and after it, and the line selected after it.
fn take_while_the_script_changes(
    cx: &mut TestAppContext,
    f: &Fixture,
    change: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    press(cx, f.window, &["down", "down"]); // c: "Third line."
    begin(cx, f, 100.0);
    fs::write(f.dir.join("script.toml"), change).expect("script");
    f.view.update(cx, |view, _| view.reload());
    f.media.set_now(100.3);
    frame(cx, f.window);
    let during = f
        .view
        .read_with(cx, |view, _| view.prompter_text().map(str::to_owned));
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    f.view.read_with(cx, |view, _| {
        (
            during,
            view.prompter_text().map(str::to_owned),
            view.line_id().map(str::to_owned),
        )
    })
}

#[gpui_kit::test]
fn a_line_removed_during_its_take_keeps_the_prompter_and_saves_the_take(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    let without_c = SCRIPT.replace(
        "[[line]]\nid = \"c\"\nstart = 1.5\ntext = \"Third line.\"\n\n",
        "",
    );
    let (during, after, selected) = take_while_the_script_changes(cx, &f, &without_c);
    assert_eq!(during.as_deref(), Some("Third line."), "during the take");
    assert_eq!(selected.as_deref(), Some("d"), "the nearest line");
    assert_eq!(after.as_deref(), Some("Fourth line."));
    let takes = recorded(&f.dir);
    assert_eq!((takes[0].0.as_str(), takes[0].1), ("c", 1), "saved");
    let shown = message(cx, &f.view);
    assert!(
        shown.contains("the line you recorded was removed or renamed; the take was saved"),
        "{shown}"
    );
}

#[gpui_kit::test]
fn a_line_renamed_during_its_take_keeps_the_prompter(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    let renamed = SCRIPT
        .replace("id = \"c\"", "id = \"c2\"")
        .replace("Third line.", "Third line, renamed.");
    let (during, after, selected) = take_while_the_script_changes(cx, &f, &renamed);
    assert_eq!(during.as_deref(), Some("Third line."), "during the take");
    assert_eq!(selected.as_deref(), Some("c2"));
    assert_eq!(after.as_deref(), Some("Third line, renamed."));
    let shown = message(cx, &f.view);
    assert!(
        shown.contains("the line you recorded was removed or renamed; the take was saved"),
        "{shown}"
    );
}

#[gpui_kit::test]
fn the_note_and_next_line_keep_the_takes_until_it_stops(cx: &mut TestAppContext) {
    let with_note = SCRIPT.replace(
        "text = \"Third line.\"\n",
        "text = \"Third line.\"\nnote = \"Slowly.\"\n",
    );
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        fs::write(dir.join("script.toml"), &with_note).expect("script");
    });
    // An agent removes the recorded line c meanwhile and rewrites d.
    let changed = SCRIPT
        .replace(
            "[[line]]\nid = \"c\"\nstart = 1.5\ntext = \"Third line.\"\n\n",
            "",
        )
        .replace(
            "text = \"Fourth line.\"",
            "text = \"Fourth, rewritten.\"\nnote = \"Loud.\"",
        );
    press(cx, f.window, &["down", "down"]); // c
    begin(cx, &f, 100.0);
    fs::write(f.dir.join("script.toml"), changed).expect("script");
    f.view.update(cx, |view, _| view.reload());
    f.media.set_now(100.3);
    frame(cx, f.window);
    let cues = |cx: &mut TestAppContext| {
        f.view.read_with(cx, |view, _| {
            let (note, next) = view.prompter_cues();
            (note.map(str::to_owned), next.map(str::to_owned))
        })
    };
    assert_eq!(
        cues(cx),
        (Some("Slowly.".to_owned()), Some("Fourth line.".to_owned())),
        "while recording"
    );
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    assert_eq!(
        cues(cx),
        (Some("Loud.".to_owned()), None),
        "after the take: line d is selected"
    );
}

#[gpui_kit::test]
fn a_take_whose_finish_failed_stays_locked_until_saved(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    begin(cx, &f, 100.0); // begun on disk, streaming into the partial WAV
    f.view
        .read_with(cx, |view, _| view.recorder.fail_next_finish());
    super::recorder::commits_fail(true);
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    let shown = message(cx, &f.view);
    assert!(shown.contains("could not be saved"), "{shown}");

    let report = tw::repair(&f.dir).expect("repair");
    assert!(report.adopted.is_empty(), "{:?}", report.adopted);

    super::recorder::commits_fail(false);
    f.media.set_now(110.0);
    frame(cx, f.window);
    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 1, "saved on retry");
    assert_eq!((takes[0].1, takes[0].2.samples), (1, 2400));
    assert!(!takes[0].2.recovered);
}

/// Records a take whose commit fails after `renames` of its two renames,
/// checks that a concurrent repair leaves it alone, and that the retry
/// completes it with exactly one recorded event.
fn a_partly_committed_take_is_finished_by_its_window(cx: &mut TestAppContext, renames: u8) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    begin(cx, &f, 100.0);
    tw::takes::fail_commits_after_renames(Some(renames));
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    tw::takes::fail_commits_after_renames(None);
    let shown = message(cx, &f.view);
    assert!(shown.contains("could not be saved"), "{shown}");

    let report = tw::repair(&f.dir).expect("repair");
    assert!(report.adopted.is_empty(), "{:?}", report.adopted);
    assert!(report.unadopted.is_empty(), "{:?}", report.unadopted);
    assert_eq!(recorded(&f.dir).len(), 0);

    f.media.set_now(110.0);
    frame(cx, f.window);
    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 1, "saved on retry");
    assert_eq!((takes[0].1, takes[0].2.samples), (1, 2400));
    assert!(!takes[0].2.recovered);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.has_unsaved()));
    f.media.set_now(120.0);
    frame(cx, f.window);
    assert_eq!(recorded(&f.dir).len(), 1, "exactly one recorded event");
}

#[gpui_kit::test]
fn a_take_whose_commit_failed_after_renaming_its_metadata_is_finished(cx: &mut TestAppContext) {
    a_partly_committed_take_is_finished_by_its_window(cx, 1);
}

#[gpui_kit::test]
fn a_take_whose_commit_failed_after_renaming_its_audio_is_finished(cx: &mut TestAppContext) {
    a_partly_committed_take_is_finished_by_its_window(cx, 2);
}

/// Records a take whose commit fails after `renames` of its two renames,
/// then removes the renamed files: the retry cannot finish that commit, so
/// it saves the take again from memory, once.
fn a_take_whose_renamed_files_vanished_is_saved_from_memory(cx: &mut TestAppContext, renames: u8) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    begin(cx, &f, 100.0);
    tw::takes::fail_commits_after_renames(Some(renames));
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    tw::takes::fail_commits_after_renames(None);
    fs::remove_file(f.dir.join("takes/a/001.json")).expect("metadata");
    if renames == 2 {
        fs::remove_file(f.dir.join("takes/a/001.wav")).expect("audio");
    }

    f.media.set_now(110.0);
    frame(cx, f.window);
    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 1, "saved on retry");
    assert_eq!((takes[0].1, takes[0].2.samples), (1, 2400));
    let (samples, _) = tw::read_take_wav(&f.dir.join("takes/a/001.wav")).expect("audio");
    assert_eq!(samples.len(), 2400);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.has_unsaved()));
    let shown = message(cx, &f.view);
    assert!(shown.contains("saved again from memory"), "{shown}");
    f.media.set_now(120.0);
    frame(cx, f.window);
    assert_eq!(recorded(&f.dir).len(), 1, "exactly one recorded event");
}

#[gpui_kit::test]
fn a_take_whose_renamed_metadata_vanished_is_saved_from_memory(cx: &mut TestAppContext) {
    a_take_whose_renamed_files_vanished_is_saved_from_memory(cx, 1);
}

#[gpui_kit::test]
fn a_take_whose_renamed_files_both_vanished_is_saved_from_memory(cx: &mut TestAppContext) {
    a_take_whose_renamed_files_vanished_is_saved_from_memory(cx, 2);
}

#[gpui_kit::test]
fn a_take_whose_written_audio_vanished_is_rewritten_from_memory(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    begin(cx, &f, 100.0); // streamed into the partial WAV, finished on stop
    super::recorder::commits_fail(true);
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    let shown = message(cx, &f.view);
    assert!(shown.contains("could not be saved"), "{shown}");
    let wav = partial_wav(&f.dir, "a").expect("the partial WAV");
    fs::remove_file(&wav).expect("the audio vanishes");

    super::recorder::commits_fail(false);
    f.media.set_now(110.0);
    frame(cx, f.window);
    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 1, "saved on retry");
    assert_eq!((takes[0].1, takes[0].2.samples), (1, 2400));
    let (samples, _) = tw::read_take_wav(&f.dir.join("takes/a/001.wav")).expect("audio");
    assert_eq!(samples.len(), 2400);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.has_unsaved()));
}

#[gpui_kit::test]
fn a_take_begun_late_is_rewritten_under_its_lock(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    // A file where the line's take directory belongs: the take cannot be
    // begun on disk and is kept in memory.
    fs::create_dir_all(f.dir.join("takes")).expect("takes");
    fs::write(f.dir.join("takes/a"), b"").expect("blocker");
    begin(cx, &f, 100.0);
    fs::remove_file(f.dir.join("takes/a")).expect("unblock");
    super::recorder::commits_fail(true);
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    let shown = message(cx, &f.view);
    assert!(shown.contains("could not be saved"), "{shown}");

    let report = tw::repair(&f.dir).expect("repair");
    assert!(report.adopted.is_empty(), "{:?}", report.adopted);

    super::recorder::commits_fail(false);
    f.media.set_now(110.0);
    frame(cx, f.window);
    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 1, "saved on retry");
    assert_eq!((takes[0].1, takes[0].2.samples), (1, 2400));
}

/// A transcriber that waits for the test to let it finish, then hears the
/// first line.
struct Gated {
    gate: Mutex<Receiver<()>>,
    heard: Mutex<Vec<PathBuf>>,
}

impl tw::Transcriber for Gated {
    fn transcribe(&self, wav: &Path) -> tw::Transcript {
        self.heard.lock().expect("heard").push(wav.to_path_buf());
        let _ = self.gate.lock().expect("gate").recv();
        tw::Transcript::Words(Vec::new())
    }
    fn model(&self) -> String {
        "ggml-gated.bin".to_owned()
    }
}

#[gpui_kit::test]
fn verdict_appears_after_commit(cx: &mut TestAppContext) {
    let (release, gate) = channel();
    let gated = Arc::new(Gated {
        gate: Mutex::new(gate),
        heard: Mutex::new(Vec::new()),
    });
    let f = open_checked(cx, &[(DEVICE, 48_000)], |_| {}, gated.clone());
    record_take(cx, &f);
    let take = tw::TakeRef {
        line: "a".to_owned(),
        take: 1,
    };
    // Committed and queued: the list says "checking…" until the check is in.
    f.view.read_with(cx, |view, _| {
        assert!(view.checks.is_pending(&take));
        let state = view.line_state().expect("line a");
        let (text, _) = super::takes::label(&state.takes[0], true);
        assert_eq!(text, "checking…");
        assert_eq!(state.takes[0].effective, None);
        // Proposed, but no hollow star until its check is in.
        assert_eq!(state.proposed, Some(1));
        assert_eq!(super::takes::star(state, &state.takes[0], true), "");
    });
    release.send(()).expect("the check waits");
    for _ in 0..2000 {
        frame(cx, f.window);
        if f.view
            .read_with(cx, |view, _| !view.checks.is_pending(&take))
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    f.view.read_with(cx, |view, _| {
        assert!(!view.checks.is_pending(&take), "the check finished");
        let state = view.line_state().expect("line a");
        let checked = &state.takes[0];
        // The fake capture is one steady level, so no speech stands out.
        assert_eq!(checked.effective, Some(tw::Verdict::Retake));
        assert_eq!(checked.reason.as_deref(), Some("no speech detected"));
        assert_eq!(super::takes::label(checked, false).0, "retake");
        // Still proposed for agents, but a retake gets no hollow star.
        assert_eq!(state.proposed, Some(1));
        assert_eq!(super::takes::star(state, checked, false), "");
    });
    assert_eq!(
        *gated.heard.lock().expect("heard"),
        vec![f.dir.join("takes/a/001.wav")]
    );
    let checked: Vec<tw::Checked> = tw::read(&f.dir)
        .expect("ledger")
        .events
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Checked(checked) => Some(checked),
            _ => None,
        })
        .collect();
    assert_eq!(checked.len(), 1);
    assert_eq!(
        checked[0].checker,
        format!(
            "takewise {} whisper ggml-gated.bin pre=hp60+norm",
            env!("CARGO_PKG_VERSION")
        )
    );
}

/// Renders frames until `done` holds for the transcriber's calls.
fn wait_heard(cx: &mut TestAppContext, f: &Fixture, gated: &Gated, done: impl Fn(usize) -> bool) {
    for _ in 0..2000 {
        frame(cx, f.window);
        if done(gated.heard.lock().expect("heard").len()) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("timed out waiting for the checks");
}

#[gpui_kit::test]
fn checks_wait_while_a_take_records(cx: &mut TestAppContext) {
    let (release, gate) = channel();
    let gated = Arc::new(Gated {
        gate: Mutex::new(gate),
        heard: Mutex::new(Vec::new()),
    });
    let f = open_checked(cx, &[(DEVICE, 48_000)], |_| {}, gated.clone());
    record_take(cx, &f); // a take 1: its check starts and waits
    begin(cx, &f, 200.0);
    f.media.set_now(200.5);
    press(cx, f.window, &["space"]); // a take 2: queued
    wait_heard(cx, &f, &gated, |n| n == 1);

    // A take starts while take 1 is checked: that check finishes, the
    // next one waits for the take to end.
    press(cx, f.window, &["down"]);
    begin(cx, &f, 300.0);
    release.send(()).expect("the check waits");
    let first = tw::TakeRef {
        line: "a".to_owned(),
        take: 1,
    };
    for _ in 0..2000 {
        f.media.set_now(300.2);
        frame(cx, f.window);
        if f.view
            .read_with(cx, |view, _| !view.checks.is_pending(&first))
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        !f.view
            .read_with(cx, |view, _| view.checks.is_pending(&first)),
        "the running check finished"
    );
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(gated.heard.lock().expect("heard").len(), 1, "take 2 waits");
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));

    f.media.set_now(300.5);
    press(cx, f.window, &["space"]); // b take 1 committed: checks resume
    wait_heard(cx, &f, &gated, |n| n >= 2);
    assert_eq!(
        gated.heard.lock().expect("heard")[1],
        f.dir.join("takes/a/002.wav")
    );
    drop(release);
}

#[gpui_kit::test]
fn a_reload_queues_a_take_whose_check_went_stale(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |_| {});
    record_take(cx, &f);
    let take = tw::TakeRef {
        line: "a".to_owned(),
        take: 1,
    };
    for _ in 0..2000 {
        frame(cx, f.window);
        if f.view
            .read_with(cx, |view, _| !view.checks.is_pending(&take))
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    // An edit made outside the window makes the check stale; the reload
    // queues the take again.
    tw::set_line_field(
        &f.dir,
        "a",
        Field::Text("First line, again.".to_owned()),
        None,
    )
    .expect("edit");
    f.view.update(cx, |view, _| view.reload());
    f.view
        .read_with(cx, |view, _| assert!(view.checks.is_pending(&take)));
}

/// Sets line `a`'s text outside the window and reloads, as the watcher
/// would.
fn set_text_a(cx: &mut TestAppContext, f: &Fixture, text: &str) {
    tw::set_line_field(&f.dir, "a", Field::Text(text.to_owned()), None).expect("edit");
    f.view.update(cx, |view, _| view.reload());
}

fn pending_a(cx: &mut TestAppContext, f: &Fixture) -> Vec<u32> {
    f.view.read_with(cx, |view, _| view.checks.pending_of("a"))
}

#[gpui_kit::test]
fn the_check_queue_follows_what_was_checked(cx: &mut TestAppContext) {
    let (release, gate) = channel();
    let gated = Arc::new(Gated {
        gate: Mutex::new(gate),
        heard: Mutex::new(Vec::new()),
    });
    let f = open_checked(cx, &[(DEVICE, 48_000)], |_| {}, gated.clone());
    record_take(cx, &f); // a take 1: checked against A, waits in the transcriber
    wait_heard(cx, &f, &gated, |n| n == 1);
    begin(cx, &f, 200.0);
    f.media.set_now(200.5);
    press(cx, f.window, &["space"]); // a take 2: queued
    assert_eq!(pending_a(cx, &f), vec![1, 2]);

    // A → B while both are queued or running: neither is queued again.
    set_text_a(cx, &f, "Changed line.");
    assert_eq!(pending_a(cx, &f), vec![1, 2], "never queued twice");

    // Take 1 finishes against A, take 2 starts against B; take 1 is stale
    // and queued again.
    release.send(()).expect("the check waits");
    wait_heard(cx, &f, &gated, |n| n == 2);
    frame(cx, f.window);
    let mut pending = pending_a(cx, &f);
    pending.sort_unstable();
    assert_eq!(pending, vec![1, 2]);

    // B → A while take 2 runs against B: it finishes stale and, since A
    // was never checked for it, is queued again.
    set_text_a(cx, &f, "First line.");
    release.send(()).expect("the check waits");
    wait_heard(cx, &f, &gated, |n| n == 3);
    frame(cx, f.window);
    let take2 = tw::TakeRef {
        line: "a".to_owned(),
        take: 2,
    };
    assert!(
        f.view
            .read_with(cx, |view, _| view.checks.is_pending(&take2)),
        "take 2 is checked against A"
    );
    drop(release);
    for _ in 0..2000 {
        frame(cx, f.window);
        if pending_a(cx, &f).is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    f.view.read_with(cx, |view, _| {
        let state = view.line_state().expect("line a");
        assert!(state.takes.iter().all(|t| !t.stale_check), "{state:?}");
    });
}

/// A transcriber that runs `tool` as a child process and keeps a temporary
/// directory while it runs, as whisper-cli is run.
struct Spawning {
    tool: &'static str,
    args: &'static [&'static str],
    /// The worker's tag, which the temporary directory is named with; the
    /// tool's last argument is a file in it, as the real tools' are.
    tag: String,
    child: Mutex<Option<(u32, PathBuf)>>,
}

impl Spawning {
    fn new(tool: &'static str, args: &'static [&'static str]) -> Arc<Self> {
        Arc::new(Self {
            tool,
            args,
            tag: worker_tag(),
            child: Mutex::new(None),
        })
    }

    fn checker(self: &Arc<Self>) -> Checker {
        Checker {
            transcriber: Arc::clone(self) as SharedTranscriber,
            tools: vec![PathBuf::from(self.tool)],
            tag: self.tag.clone(),
        }
    }

    /// The running child's pid and temporary directory, once it started.
    fn child(&self) -> Option<(u32, PathBuf)> {
        self.child.lock().expect("child").clone()
    }
}

impl tw::Transcriber for Spawning {
    fn transcribe(&self, _: &Path) -> tw::Transcript {
        let tmp = tempfile::Builder::new()
            .prefix(&self.tag)
            .tempdir()
            .expect("temp dir");
        fs::write(tmp.path().join("take.wav"), b"").expect("temp file");
        let mut child = std::process::Command::new(self.tool)
            .args(self.args)
            .arg(tmp.path().join("take.wav"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("the tool starts");
        // Held open, so a tool reading it waits.
        let _stdin = child.stdin.take();
        *self.child.lock().expect("child") = Some((child.id(), tmp.path().to_path_buf()));
        let status = child.wait().expect("wait");
        drop(tmp);
        if status.success() {
            tw::Transcript::Words(Vec::new())
        } else {
            tw::Transcript::Failed(format!("{} failed ({status})", self.tool))
        }
    }
    fn model(&self) -> String {
        "ggml-spawning.bin".to_owned()
    }
}

fn open_spawning(cx: &mut TestAppContext, spawning: &Arc<Spawning>) -> Fixture {
    let Checker {
        transcriber,
        tools,
        tag,
    } = spawning.checker();
    open_tooled(cx, &[(DEVICE, 48_000)], |_| {}, transcriber, tools, tag)
}

/// The process state `ps` reports for `pid` (`T` when stopped), or `None`
/// once it is gone.
fn process_state(pid: u32) -> Option<String> {
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let stat = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !stat.is_empty()).then_some(stat)
}

/// Renders frames until `done` holds.
fn wait_until(cx: &mut TestAppContext, f: &Fixture, done: impl Fn() -> bool) {
    for _ in 0..3000 {
        frame(cx, f.window);
        if done() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("timed out");
}

fn checked_events(dir: &Path) -> usize {
    tw::read(dir)
        .expect("ledger")
        .events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::Checked(_)))
        .count()
}

#[gpui_kit::test]
fn a_running_check_is_paused_while_a_take_records(cx: &mut TestAppContext) {
    // The file argument is the shell's `$0`.
    let spawning = Spawning::new("/bin/sh", &["-c", "sleep 1; :"]);
    let f = open_spawning(cx, &spawning);
    record_take(cx, &f); // a take 1: its check runs the tool
    wait_until(cx, &f, || spawning.child().is_some());
    let (pid, _) = spawning.child().expect("the check runs");

    press(cx, f.window, &["down"]);
    begin(cx, &f, 200.0); // a take starts: the check's child is stopped
    wait_until(cx, &f, || {
        process_state(pid).is_some_and(|state| state.starts_with('T'))
    });
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));

    f.media.set_now(200.5);
    press(cx, f.window, &["space"]); // committed: the child goes on
    wait_until(cx, &f, || {
        process_state(pid).is_none_or(|state| !state.starts_with('T'))
    });
    wait_until(cx, &f, || checked_events(&f.dir) >= 1);
    assert!(process_state(pid).is_none_or(|state| !state.starts_with('T')));
}

#[gpui_kit::test]
fn a_window_pausing_its_checks_leaves_another_windows_running(cx: &mut TestAppContext) {
    // Both windows' tools are `cat`, children of this one process.
    let mine = Spawning::new("/bin/cat", &["-"]);
    let f = open_spawning(cx, &mine);
    record_take(cx, &f); // a take 1: this window checks it
    wait_until(cx, &f, || mine.child().is_some());
    let (my_pid, _) = mine.child().expect("this window's check runs");

    let theirs = Spawning::new("/bin/cat", &["-"]);
    let mut other = Checks::new(f.dir.clone(), theirs.checker());
    let take = tw::TakeRef {
        line: "a".to_owned(),
        take: 1,
    };
    let snapshot = Project::open(&f.dir)
        .expect("project")
        .snapshot("a")
        .expect("line a");
    other.queue(&take, &snapshot);
    wait_until(cx, &f, || theirs.child().is_some());
    let (their_pid, _) = theirs.child().expect("the other window's check runs");

    press(cx, f.window, &["down"]);
    begin(cx, &f, 200.0); // this window records: its own check stops
    wait_until(cx, &f, || {
        process_state(my_pid).is_some_and(|state| state.starts_with('T'))
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    let their_state = process_state(their_pid);

    other.cancel(); // quitting one window kills only its own tool
    assert_eq!(process_state(their_pid), None);
    assert!(process_state(my_pid).is_some(), "this window's check lives");
    f.view.update(cx, |view, _| view.shutdown());
    assert!(
        their_state.is_some_and(|state| !state.starts_with('T')),
        "the other window's check runs on"
    );
}

/// Sets `[check]` in `project.toml`, as an edit by hand would.
fn set_check(dir: &Path, model: &str, language: &str) {
    let path = dir.join("project.toml");
    let text = fs::read_to_string(&path).expect("project.toml");
    let kept = match text.find("[check]") {
        Some(start) => {
            let rest = &text[start + 1..];
            let end = rest.find("\n[").map_or(text.len(), |i| start + 1 + i + 1);
            format!("{}{}", &text[..start], &text[end..])
        }
        None => text,
    };
    let section = format!("[check]\nwhisper_model = {model:?}\nlanguage = {language:?}\n");
    fs::write(&path, format!("{}\n{section}", kept.trim_end())).expect("write project.toml");
}

#[gpui_kit::test]
fn changed_check_settings_start_a_new_worker(cx: &mut TestAppContext) {
    let f = open_checked(cx, &[(DEVICE, 48_000)], |_| {}, no_transcriber());
    let made: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let log = Arc::clone(&made);
    f.view.update(cx, |view, _| {
        view.make_checker = Box::new(move |project: &Project| {
            let config = &project.config.check;
            log.lock()
                .expect("made")
                .push((config.whisper_model.clone(), config.language.clone()));
            Checker {
                transcriber: Arc::new(tw::Unavailable {
                    reason: "a new worker".to_owned(),
                    model: config.whisper_model.clone(),
                }),
                tools: Vec::new(),
                tag: worker_tag(),
            }
        });
        view.reload();
    });
    let made_now = || made.lock().expect("made").clone();
    assert_eq!(made_now(), [], "unchanged settings keep the worker");

    let model = config(&f.dir).check.whisper_model;
    set_check(&f.dir, &model, "de");
    f.view.update(cx, |view, _| view.reload());
    assert_eq!(made_now(), [(model.clone(), "de".to_owned())]);

    set_check(&f.dir, "models/ggml-other.bin", "de");
    f.view.update(cx, |view, _| view.reload());
    f.view.update(cx, |view, _| view.reload());
    assert_eq!(made_now().len(), 2, "one new worker per change");

    record_take(cx, &f);
    wait_until(cx, &f, || checked_events(&f.dir) >= 1);
    let checker = tw::read(&f.dir)
        .expect("ledger")
        .events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Checked(checked) => Some(checked.checker.clone()),
            _ => None,
        })
        .expect("a check");
    assert!(
        checker.contains(" whisper models/ggml-other.bin "),
        "checked by the new worker: {checker}"
    );
}

/// Logs the `[check] whisper_model` of each new check worker.
fn log_new_workers(cx: &mut TestAppContext, f: &Fixture) -> Arc<Mutex<Vec<String>>> {
    let made: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = Arc::clone(&made);
    f.view.update(cx, |view, _| {
        view.make_checker = Box::new(move |project: &Project| {
            let model = project.config.check.whisper_model.clone();
            log.lock().expect("made").push(model.clone());
            Checker {
                transcriber: Arc::new(tw::Unavailable {
                    reason: "a new worker".to_owned(),
                    model,
                }),
                tools: Vec::new(),
                tag: worker_tag(),
            }
        });
    });
    made
}

#[gpui_kit::test]
fn a_model_of_the_same_name_elsewhere_starts_a_new_worker(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    set_check(&f.dir, "a/model.bin", "en");
    f.view.update(cx, |view, _| view.reload());
    let made = log_new_workers(cx, &f);
    set_check(&f.dir, "b/model.bin", "en");
    f.view.update(cx, |view, _| view.reload());
    assert_eq!(*made.lock().expect("made"), ["b/model.bin"]);
}

#[gpui_kit::test]
fn the_model_banner_follows_a_model_change(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    let models = tempfile::tempdir().expect("models");
    f.view.update(cx, |view, _| {
        view.models_dir = models.path().to_path_buf();
        view.offers_model = true;
    });
    let pinned = tw::CheckConfig::default().whisper_model;
    set_check(&f.dir, "ggml-base.bin", "en");
    f.view.update(cx, |view, _| view.reload());
    assert_eq!(banner(cx, &f), None, "no download for another model");

    set_check(&f.dir, &pinned, "en");
    f.view.update(cx, |view, _| view.reload());
    let text = banner(cx, &f).expect("the pinned model is missing");
    assert!(text.contains("Install the speech model"), "{text}");

    set_check(&f.dir, "ggml-base.bin", "en");
    f.view.update(cx, |view, _| view.reload());
    assert_eq!(banner(cx, &f), None, "gone with the model");
}

#[gpui_kit::test]
fn quitting_mid_check_kills_its_child_and_temporary_files(cx: &mut TestAppContext) {
    // Reads its held-open standard input first.
    let spawning = Spawning::new("/bin/cat", &["-"]);
    let f = open_spawning(cx, &spawning);
    record_take(cx, &f);
    wait_until(cx, &f, || spawning.child().is_some());
    let (pid, tmp) = spawning.child().expect("the check runs");
    assert!(tmp.is_dir());

    f.view.update(cx, |view, _| view.shutdown());
    assert_eq!(process_state(pid), None, "the child is gone");
    assert!(!tmp.exists(), "its temporary files are gone");
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(
        checked_events(&f.dir),
        0,
        "a cancelled check records nothing"
    );
}

// The audio panel, meters and volume keys.

fn config(dir: &Path) -> tw::ProjectConfig {
    Project::open(dir).expect("project").config
}

#[gpui_kit::test]
fn missing_device_opens_panel(cx: &mut TestAppContext) {
    let f = open(cx, &[("Other Mic", 48_000)]);
    frame(cx, f.window);
    f.view.read_with(cx, |view, _| {
        assert!(view.panel.open, "the panel opens by itself");
        let issue = view.panel.issue.as_ref().expect("an input issue");
        assert_eq!(
            issue.message(),
            "Test Mic is not connected. Connect it or pick another input."
        );
        assert_eq!(view.inputs.len(), 1, "the panel lists the connected inputs");
    });
    press(cx, f.window, &["space"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert_eq!(recorded(&f.dir).len(), 0, "Space stays blocked");

    f.view.update(cx, |view, _| view.choose_input("Other Mic"));
    frame(cx, f.window);
    f.view.read_with(cx, |view, _| {
        assert_eq!(view.panel.issue, None);
        assert_eq!(view.blocked(), None);
    });
    record_take(cx, &f);
    assert_eq!(recorded(&f.dir)[0].2.device, "Other Mic");
}

#[gpui_kit::test]
fn panel_meters_every_channel_while_open_and_idle(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    frame(cx, f.window);
    assert_eq!(f.media.0.borrow().channel_meter, None, "closed: no meter");
    press(cx, f.window, &["a"]);
    frame(cx, f.window);
    assert_eq!(f.media.0.borrow().channel_meter.as_deref(), Some(DEVICE));
    begin(cx, &f, 100.0);
    assert_eq!(
        f.media.0.borrow().channel_meter,
        None,
        "no channel meter during a take"
    );
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    frame(cx, f.window);
    assert_eq!(f.media.0.borrow().channel_meter.as_deref(), Some(DEVICE));
    press(cx, f.window, &["a"]);
    frame(cx, f.window);
    assert_eq!(f.media.0.borrow().channel_meter, None);
}

#[gpui_kit::test]
fn pickers_disabled_during_take(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000), ("Other Mic", 48_000)]);
    press(cx, f.window, &["a"]);
    begin(cx, &f, 100.0);
    assert!(!f.view.read_with(cx, |view, _| view.pickers_enabled()));
    f.view.update(cx, |view, _| {
        view.choose_input("Other Mic");
        view.choose_channel(2);
        view.choose_output("Test Phones");
        view.switch_to_48k();
    });
    frame(cx, f.window);
    let config = config(&f.dir);
    assert_eq!(config.input.device.as_deref(), Some(DEVICE));
    assert_eq!(config.input.channel, 1);
    assert_eq!(config.output.device, None);
    assert_eq!(f.media.0.borrow().film_output, None);
    assert!(
        message(cx, &f.view).contains("between takes"),
        "{}",
        message(cx, &f.view)
    );
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    assert!(f.view.read_with(cx, |view, _| view.pickers_enabled()));
    assert_eq!(recorded(&f.dir)[0].2.device, DEVICE);
}

#[gpui_kit::test]
fn route_change_applies_between_takes(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000), ("Other Mic", 48_000)]);
    let settings = f.dir.join("settings.toml");
    let path = settings.clone();
    f.view.update(cx, |view, _| view.defaults_path = Some(path));
    frame(cx, f.window);
    assert_eq!(
        f.media.0.borrow().capture_input,
        Some((DEVICE.to_owned(), 1)),
        "the meter listens to the configured input"
    );
    f.view.update(cx, |view, _| {
        view.choose_input("Other Mic");
        view.choose_channel(2);
        view.choose_output("Test Phones");
    });
    frame(cx, f.window);
    let config = config(&f.dir);
    assert_eq!(config.input.device.as_deref(), Some("Other Mic"));
    assert_eq!(config.input.channel, 2);
    assert_eq!(config.output.device.as_deref(), Some("Test Phones"));
    assert_eq!(
        f.media.0.borrow().film_output.as_deref(),
        Some("Test Phones"),
        "the film plays through the new output"
    );
    assert_eq!(
        f.media.0.borrow().capture_input,
        Some(("Other Mic".to_owned(), 2)),
        "the meter follows the new input and channel"
    );
    let defaults = tw::route::load_defaults_from(&settings)
        .expect("settings")
        .expect("saved defaults");
    assert_eq!(defaults.input.as_deref(), Some("Other Mic"));
    assert_eq!(defaults.channel, 2);
    assert_eq!(defaults.output.as_deref(), Some("Test Phones"));

    record_take(cx, &f);
    let (_, _, event) = &recorded(&f.dir)[0];
    assert_eq!((event.device.as_str(), event.channel), ("Other Mic", 2));
}

#[gpui_kit::test]
fn configured_output_is_used_on_open(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        tw::set_route(dir, None, Some("Test Phones")).expect("route");
    });
    assert_eq!(
        f.media.0.borrow().film_output.as_deref(),
        Some("Test Phones")
    );
    let missing = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        tw::set_route(dir, None, Some("Gone Phones")).expect("route");
    });
    assert_eq!(
        message(cx, &missing.view),
        "Gone Phones is not connected: connect it or pick another output (A)."
    );
    assert_eq!(missing.media.0.borrow().film_output, None);
}

#[gpui_kit::test]
fn rate_switch_unblocks_or_says_why_not(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 44_100)]);
    f.view.read_with(cx, |view, _| {
        assert!(matches!(
            view.panel.issue,
            Some(super::panel::InputIssue::WrongRate { rate: 44_100, .. })
        ));
        assert!(view.blocked().is_some());
    });
    let refusal = "\"Test Mic\" could not switch to 48000 Hz: the device does not support that rate (CoreAudio status '!dat')";
    f.media.0.borrow_mut().rate_error = Some(refusal.to_owned());
    f.view.update(cx, |view, _| view.switch_to_48k());
    let text = message(cx, &f.view);
    assert!(text.starts_with(refusal), "{text}");
    assert!(text.contains("Recording stays off"), "{text}");
    assert!(f.view.read_with(cx, |view, _| view.blocked().is_some()));

    f.media.0.borrow_mut().rate_error = None;
    f.view.update(cx, |view, _| view.switch_to_48k());
    f.view.read_with(cx, |view, _| {
        assert_eq!(view.blocked(), None);
        assert_eq!(view.panel.issue, None);
    });
}

#[gpui_kit::test]
fn volume_keys_match_characters(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    let volumes = |f: &Fixture, cx: &mut TestAppContext| {
        f.view
            .read_with(cx, |view, _| (view.volumes.film, view.volumes.monitor))
    };
    assert_eq!(volumes(&f, cx), (-12.0, -6.0));
    press(cx, f.window, &["-"]);
    assert_eq!(volumes(&f, cx), (-13.0, -6.0));
    assert_eq!(f.media.0.borrow().film_gain_db, Some(-13.0));
    // QWERTY types + with shift and =; QWERTZ has a + key.
    press(cx, f.window, &["shift-=->+"]);
    assert_eq!(volumes(&f, cx).0, -12.0);
    press(cx, f.window, &["+"]);
    assert_eq!(volumes(&f, cx).0, -11.0);
    press(cx, f.window, &["=", "shift-="]);
    assert_eq!(volumes(&f, cx).0, -11.0, "only a typed + raises it");
    // The monitor level is hidden while monitoring cannot run: the keys
    // say so and change nothing.
    press(cx, f.window, &[","]);
    assert_eq!(volumes(&f, cx).1, -6.0);
    assert_eq!(
        message(cx, &f.view),
        "The , and . keys set the monitor level, which waits for monitoring (not available yet: monitoring needs a test on your setup)."
    );
    f.media.0.borrow_mut().monitor_unavailable = None;
    press(cx, f.window, &[",", ",", "."]);
    assert_eq!(volumes(&f, cx), (-11.0, -7.0));
    press(cx, f.window, &["."; 20]);
    assert_eq!(volumes(&f, cx).1, 6.0, "the monitor stops at +6 dB");
    press(cx, f.window, &["+"; 20]);
    assert_eq!(volumes(&f, cx).0, 0.0, "the film stops at 0 dB");

    // Written once the keys rest.
    f.media.set_now(101.0);
    frame(cx, f.window);
    let saved = config(&f.dir);
    assert_eq!(saved.record.film_gain_db, 0.0);
    assert_eq!(saved.monitor.volume_db, 6.0);

    // During a take the film's level changes at once and is written after.
    begin(cx, &f, 200.0);
    press(cx, f.window, &["-", "-"]);
    assert_eq!(f.media.0.borrow().film_gain_db, Some(-2.0));
    f.media.set_now(201.0);
    frame(cx, f.window);
    assert_eq!(
        config(&f.dir).record.film_gain_db,
        0.0,
        "not during the take"
    );
    press(cx, f.window, &["space"]);
    f.media.set_now(202.0);
    frame(cx, f.window);
    assert_eq!(config(&f.dir).record.film_gain_db, -2.0);
    assert_eq!(
        volumes(&f, cx).0,
        -2.0,
        "the reload after the take keeps it"
    );
}

/// The value `level`'s slider shows.
fn slider_db(cx: &mut TestAppContext, f: &Fixture, level: Level) -> f32 {
    f.view.read_with(cx, |view, cx| {
        view.levels.slider(level).read(cx).value().end()
    })
}

/// Drags `level`'s slider to `db`, as the slider reports a drag.
fn drag_slider(cx: &mut TestAppContext, f: &Fixture, level: Level, db: f32) {
    let slider = f
        .view
        .read_with(cx, |view, _| view.levels.slider(level).clone());
    slider.update(cx, |_, cx| {
        cx.emit(SliderEvent::Change(SliderValue::from(db)));
    });
    slider.update(cx, |_, cx| {
        cx.emit(SliderEvent::Release(SliderValue::from(db)));
    });
}

#[gpui_kit::test]
fn keys_move_the_level_sliders(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    f.media.0.borrow_mut().monitor_unavailable = None;
    assert_eq!(slider_db(cx, &f, Level::Film), -12.0);
    assert_eq!(slider_db(cx, &f, Level::Voice), 0.0);
    press(cx, f.window, &["-", "-", ","]);
    assert_eq!(slider_db(cx, &f, Level::Film), -14.0);
    assert_eq!(slider_db(cx, &f, Level::Monitor), -7.0);

    drag_slider(cx, &f, Level::Film, -30.0);
    f.view
        .read_with(cx, |view, _| assert_eq!(view.volumes.film, -30.0));
    assert_eq!(f.media.0.borrow().film_gain_db, Some(-30.0));
    f.media.set_now(101.0);
    frame(cx, f.window);
    assert_eq!(config(&f.dir).record.film_gain_db, -30.0);
}

#[gpui_kit::test]
fn a_reloaded_level_moves_its_slider(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    let edit = tw::Volumes {
        film_gain_db: Some(-20.0),
        ..tw::Volumes::default()
    };
    tw::set_volumes(&f.dir, edit).expect("agent edit");
    f.view.update(cx, |view, _| view.reload());
    frame(cx, f.window);
    assert_eq!(slider_db(cx, &f, Level::Film), -20.0);
    press(cx, f.window, &["+"]);
    assert_eq!(slider_db(cx, &f, Level::Film), -19.0);
    press(cx, f.window, &["-", "-"]);
    assert_eq!(slider_db(cx, &f, Level::Film), -21.0);
    f.view
        .read_with(cx, |view, _| assert_eq!(view.volumes.film, -21.0));
}

#[gpui_kit::test]
fn the_monitor_level_shows_only_while_monitoring_can_run(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    let shown = |cx: &mut TestAppContext| f.view.read_with(cx, |view, _| view.shown_levels());
    assert_eq!(shown(cx), [Level::Film, Level::Voice]);
    f.media.0.borrow_mut().monitor_unavailable = None;
    assert_eq!(shown(cx), [Level::Film, Level::Voice, Level::Monitor]);
}

#[gpui_kit::test]
fn the_voice_level_plays_takes_back_quieter(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    let gains = |f: &Fixture| {
        let fake = f.media.0.borrow();
        fake.take.as_ref().map(|take| take.gains_db.clone())
    };
    press(cx, f.window, &["down", "down", "down"]); // d
    record_take(cx, &f);
    drag_slider(cx, &f, Level::Voice, -9.0);
    assert_eq!(gains(&f), None, "no take is open to set");

    // Set before the take starts, with the film.
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(gains(&f), Some(vec![-9.0]));
    assert_eq!(f.media.0.borrow().take_plays.len(), 1);
    assert_eq!(
        f.media.0.borrow().film_gain_db,
        Some(-12.0),
        "the film keeps its level"
    );

    // Changed while it plays: at once, and saved while it still plays.
    drag_slider(cx, &f, Level::Voice, -20.0);
    assert_eq!(gains(&f), Some(vec![-9.0, -20.0]));
    f.media.set_now(201.0);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    let saved = config(&f.dir);
    assert_eq!(saved.record.voice_gain_db, -20.0);
    assert_eq!(saved.record.film_gain_db, -12.0);

    // The next playback starts at the new level; above 0 dB is refused.
    press(cx, f.window, &["p", "p"]);
    frame(cx, f.window);
    assert_eq!(gains(&f), Some(vec![-20.0]));
    drag_slider(cx, &f, Level::Voice, 6.0);
    f.view
        .read_with(cx, |view, _| assert_eq!(view.volumes.voice, 0.0));
}

#[gpui_kit::test]
fn the_menu_bar_works_from_a_recording_window(cx: &mut TestAppContext) {
    use super::{OpenHelp, OpenKeys, OpenReleaseNotes, ReportIssue};
    let f = open(cx, &[(DEVICE, 48_000)]);
    cx.update(super::menu_actions);
    let names: Vec<_> = super::menus().into_iter().map(|menu| menu.name).collect();
    assert_eq!(names, ["Takewise", "File", "Window", "Help"]);
    act(cx, f.window, |window, cx| {
        for menu in super::menus() {
            for item in menu.items {
                if let gpui_kit::MenuItem::Action { name, action, .. } = item {
                    // Handled by the window or by the app.
                    let here = window.is_action_available(&*action, cx);
                    assert!(
                        here || cx.is_action_available(&*action),
                        "{name} does nothing"
                    );
                }
            }
        }
    });
    cx.dispatch_action(f.window, OpenHelp);
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://github.com/niklas-heer/takewise#readme")
    );
    cx.dispatch_action(f.window, OpenKeys);
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://github.com/niklas-heer/takewise#keys")
    );
    cx.dispatch_action(f.window, OpenReleaseNotes);
    assert_eq!(
        cx.opened_url(),
        Some(format!(
            "https://github.com/niklas-heer/takewise/releases/tag/v{}",
            env!("CARGO_PKG_VERSION")
        ))
    );
    cx.dispatch_action(f.window, ReportIssue);
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://github.com/niklas-heer/takewise/issues/new")
    );
}

// The test window of gpui-pre 0.3.12 (the GPUI gpui-kit 0.10.0 pins)
// implements minimize() and zoom() as `unimplemented!()`, which panics
// with "not implemented": these pass only when the action reaches the
// window's own minimize or zoom, which the app-wide handlers of 0.2.1's
// first cut never did. A GPUI whose test window records the calls instead
// fails them, to be rewritten against what it records.
#[gpui_kit::test]
#[should_panic(expected = "not implemented")]
fn window_minimize_reaches_the_window(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["cmd-m"]);
}

#[gpui_kit::test]
#[should_panic(expected = "not implemented")]
fn window_zoom_reaches_the_window(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    cx.dispatch_action(f.window, super::Zoom);
}

#[gpui_kit::test]
fn command_w_closes_the_window_and_command_q_is_quit(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    cx.update(|cx| {
        let keymap = cx.key_bindings();
        let keys = |action: &dyn gpui_kit::Action| -> Vec<String> {
            keymap
                .borrow()
                .bindings_for_action(action)
                .map(|binding| {
                    let stroke = &binding.keystrokes()[0];
                    let cmd = if stroke.modifiers().platform {
                        "cmd-"
                    } else {
                        ""
                    };
                    format!("{cmd}{}", stroke.key())
                })
                .collect()
        };
        assert_eq!(keys(&super::Quit), ["cmd-q"], "⌘Q quits, ⌘W does not");
        assert_eq!(keys(&super::CloseWindow), ["cmd-w"]);
    });
    press(cx, f.window, &["-"]);
    press(cx, f.window, &["cmd-w"]);
    assert_eq!(cx.windows().len(), 0, "closed");
    assert_eq!(config(&f.dir).record.film_gain_db, -13.0, "shut down first");
}

#[gpui_kit::test]
fn command_w_and_command_q_keep_a_window_with_audio_only_in_memory(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    cx.update(super::menu_actions);
    // A file where line a's takes belong: its take cannot be saved.
    fs::create_dir_all(f.dir.join("takes")).expect("takes");
    fs::write(f.dir.join("takes/a"), b"").expect("blocker");
    record_take(cx, &f);
    assert_eq!(recorded(&f.dir).len(), 0);
    for key in ["cmd-w", "cmd-q"] {
        f.view.update(cx, |view, _| view.message = None);
        press(cx, f.window, &[key]);
        assert_eq!(cx.windows().len(), 1, "{key}: it stays");
        assert!(
            message(cx, &f.view).starts_with("Not closing: 1 take is only in memory"),
            "{key}"
        );
    }
}

#[gpui_kit::test]
fn a_level_changed_on_disk_is_kept_and_heard(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["down", "down", "down"]); // d
    record_take(cx, &f);
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));

    // The film here, the voice by an agent before the film was saved.
    press(cx, f.window, &["-"]);
    let edit = tw::Volumes {
        voice_gain_db: Some(-7.0),
        film_gain_db: Some(-30.0),
        ..tw::Volumes::default()
    };
    tw::set_volumes(&f.dir, edit).expect("agent edit");
    f.view.update(cx, |view, _| view.reload());
    f.view.read_with(cx, |view, _| {
        assert_eq!(view.volumes.film, -13.0, "the unsaved change stays");
        assert_eq!(view.volumes.voice, -7.0, "the file's voice level is taken");
    });
    let gains = f.media.0.borrow().take.as_ref().map(|t| t.gains_db.clone());
    assert_eq!(
        gains.and_then(|g| g.last().copied()),
        Some(-7.0),
        "and heard"
    );

    f.media.set_now(201.0);
    frame(cx, f.window);
    let saved = config(&f.dir);
    assert_eq!(saved.record.film_gain_db, -13.0);
    assert_eq!(saved.record.voice_gain_db, -7.0, "not overwritten");

    // A film level from the file is heard too, and the next playback
    // starts at the voice level from the file.
    let edit = tw::Volumes {
        film_gain_db: Some(-20.0),
        voice_gain_db: Some(-9.0),
        ..tw::Volumes::default()
    };
    tw::set_volumes(&f.dir, edit).expect("agent edit");
    f.view.update(cx, |view, _| view.reload());
    assert_eq!(f.media.0.borrow().film_gain_db, Some(-20.0));
    press(cx, f.window, &["p", "p"]);
    frame(cx, f.window);
    let gains = f.media.0.borrow().take.as_ref().map(|t| t.gains_db.clone());
    assert_eq!(gains, Some(vec![-9.0]));
    assert_eq!(f.media.0.borrow().film_gain_db, Some(-20.0));
}

#[gpui_kit::test]
fn a_level_that_cannot_be_saved_is_tried_again(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    let path = f.dir.join("project.toml");
    let good = fs::read_to_string(&path).expect("project.toml");
    fs::write(&path, format!("{good}\n[[[ broken")).expect("break it");
    press(cx, f.window, &["-"]);
    f.media.set_now(101.0);
    frame(cx, f.window);
    assert!(
        message(cx, &f.view).starts_with("Could not save the levels; trying again"),
        "{}",
        message(cx, &f.view)
    );
    fs::write(&path, &good).expect("mend it");
    f.media.set_now(102.0);
    frame(cx, f.window);
    assert_eq!(config(&f.dir).record.film_gain_db, -12.0, "waits to retry");
    f.media.set_now(103.1);
    frame(cx, f.window);
    assert_eq!(config(&f.dir).record.film_gain_db, -13.0, "not lost");
    assert_eq!(message(cx, &f.view), "Saved the levels.");

    // Closing writes what has not rested yet.
    press(cx, f.window, &["-"]);
    f.view.update(cx, |view, _| view.shutdown());
    assert_eq!(config(&f.dir).record.film_gain_db, -14.0);
}

#[gpui_kit::test]
fn levels_out_of_range_in_the_file_are_clamped(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        let edit = tw::Volumes {
            film_gain_db: Some(12.0),
            voice_gain_db: Some(5.0),
            monitor_db: Some(-99.0),
        };
        tw::set_volumes(dir, edit).expect("levels");
        commit_outside(dir, "d");
    });
    f.view.read_with(cx, |view, _| {
        assert_eq!(
            (view.volumes.film, view.volumes.voice, view.volumes.monitor),
            (0.0, 0.0, -60.0)
        );
    });
    press(cx, f.window, &["down", "down", "down"]); // d
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    assert_eq!(f.media.0.borrow().film_gain_db, Some(0.0));
    let gains = f.media.0.borrow().take.as_ref().map(|t| t.gains_db.clone());
    assert_eq!(gains, Some(vec![0.0]));
}

#[gpui_kit::test]
fn open_project_in_a_recording_window_opens_the_folder_picker(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    cx.update(super::menu_actions);
    cx.dispatch_action(f.window, super::start::OpenProject);
    assert!(cx.did_prompt_for_paths(), "the folder picker");
    assert_eq!(cx.windows().len(), 1, "no start window");

    // The project open here comes to the front; nothing else opens.
    let dir = f.dir.clone();
    cx.simulate_path_prompt_response(move |_| Some(vec![dir]));
    cx.run_until_parked();
    assert_eq!(cx.windows().len(), 1);
    assert!(!message(cx, &f.view).starts_with("Cannot open"));

    // A folder that is no project yet: a start window asks for the film.
    cx.dispatch_action(f.window, super::start::OpenProject);
    let empty = tempfile::tempdir().expect("dir");
    let folder = empty.path().to_owned();
    cx.simulate_path_prompt_response(move |_| Some(vec![folder]));
    cx.run_until_parked();
    assert_eq!(cx.windows().len(), 2, "a start window");
    assert!(cx.did_prompt_for_paths(), "the film picker");
}

/// −0.4 dBFS: near clipping, not clipped.
const LOUD: i32 = 8_000_000;

#[gpui_kit::test]
fn near_clip_latches_until_k(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    frame(cx, f.window); // the idle meter runs
    let near = |f: &Fixture, cx: &mut TestAppContext| {
        f.view.read_with(cx, |view, _| view.peak_meter.near_clip())
    };
    f.media.send(100.0, 480);
    frame(cx, f.window);
    assert!(!near(&f, cx));
    f.media.send_value(100.01, 480, LOUD);
    f.media.send(100.02, 480); // a quiet buffer in the same frame
    frame(cx, f.window);
    assert!(near(&f, cx), "a loud buffer turns it red");
    f.media.set_now(105.0);
    f.media.send(105.0, 480);
    frame(cx, f.window);
    assert!(near(&f, cx), "it stays red");
    press(cx, f.window, &["k"]);
    assert!(!near(&f, cx), "K clears it");

    f.media.send_value(105.1, 480, LOUD);
    frame(cx, f.window);
    assert!(near(&f, cx));
    begin(cx, &f, 110.0);
    assert!(!near(&f, cx), "the next take clears it");
}

#[gpui_kit::test]
fn clipped_take_row(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    // Take 1 reaches full scale; take 2 stays near clipping only.
    for (value, at) in [(takewise_core::takes::FULL_SCALE, 100.0), (LOUD, 200.0)] {
        begin(cx, &f, at);
        f.media.send_value(at + 0.1, 2400, value);
        settle(cx, &f);
        f.media.set_now(at + 0.5);
        press(cx, f.window, &["space"]);
    }
    let clipped =
        |f: &Fixture, cx: &mut TestAppContext| f.view.read_with(cx, |view, _| view.clipped_takes());
    assert_eq!(clipped(&f, cx), [1], "marked from the recorded peak");
    let both_checked = |f: &Fixture, cx: &mut TestAppContext| {
        f.view.read_with(cx, |view, _| {
            view.line_state()
                .is_some_and(|line| line.takes.iter().all(|t| t.clipped.is_some()))
        })
    };
    for _ in 0..3000 {
        frame(cx, f.window);
        if both_checked(&f, cx) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(clipped(&f, cx), [1], "and from the check");
    f.view.read_with(cx, |view, _| {
        let takes = &view.line_state().expect("line").takes;
        assert_eq!(takes[0].clipped, Some(true));
        assert_eq!(takes[1].clipped, Some(false));
    });
}

#[gpui_kit::test]
fn h_says_why_monitoring_is_off(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    press(cx, f.window, &["h"]);
    let text = message(cx, &f.view);
    assert!(
        text.contains("not available yet: monitoring needs a test on your setup")
            && text.contains("direct monitoring"),
        "{text}"
    );
    begin(cx, &f, 100.0);
    f.view.update(cx, |view, _| view.message = None);
    press(cx, f.window, &["h"]);
    assert_eq!(message(cx, &f.view), "", "H does nothing during a take");
}

#[gpui_kit::test]
fn monitor_keys_hidden_while_monitoring_is_unavailable(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    let hints = f.view.read_with(cx, |view, _| view.key_hints());
    assert!(!hints.contains("monitor"), "{hints}");
    assert!(hints.contains("−/+ film"), "{hints}");
    f.media.0.borrow_mut().monitor_unavailable = None;
    let hints = f.view.read_with(cx, |view, _| view.key_hints());
    assert!(hints.contains(",/. monitor"), "{hints}");
}

#[gpui_kit::test]
fn stale_calibration_shows_in_the_panel(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        tw::set_latency_offset(dir, 25).expect("offset");
    });
    let stale = f
        .view
        .read_with(cx, |view, _| view.panel.calibration.clone())
        .expect("stale");
    assert!(stale.contains("calibrate --write"), "{stale}");
    let fresh = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        let info = tw::CalibrationInfo {
            input: DEVICE.to_owned(),
            output: "MacBook Speakers".to_owned(),
            buffer_frames: Some(512),
            date: "2026-11-23".to_owned(),
            from: None,
        };
        tw::set_calibration(dir, 25, &info).expect("calibration");
    });
    assert_eq!(
        fresh
            .view
            .read_with(cx, |view, _| view.panel.calibration.clone()),
        None
    );
}

/// Stores a 25 ms offset measured on `output` with the fake's buffer size.
fn calibrate_on(dir: &Path, output: &str) {
    let info = tw::CalibrationInfo {
        input: DEVICE.to_owned(),
        output: output.to_owned(),
        buffer_frames: Some(512),
        date: "2026-11-23".to_owned(),
        from: None,
    };
    tw::set_calibration(dir, 25, &info).expect("calibration");
}

/// Records a take of line d and returns where its first sample landed.
fn line_offset_of_a_take(cx: &mut TestAppContext, f: &Fixture) -> f64 {
    press(cx, f.window, &["down", "down", "down"]);
    begin(cx, f, 100.0);
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    let takes = recorded(&f.dir);
    takes.last().expect("a take").2.line_offset_s
}

#[gpui_kit::test]
fn offset_applies_only_on_the_calibrated_route(cx: &mut TestAppContext) {
    // Calibrated on the devices in use: the 25 ms offset moves the take.
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        calibrate_on(dir, "MacBook Speakers");
    });
    let offset = line_offset_of_a_take(cx, &f);
    assert!((offset - -2.375).abs() < 1e-6, "{offset}");
    assert_eq!(
        f.view
            .read_with(cx, |view, _| view.panel.calibration.clone()),
        None
    );

    // Calibrated on other headphones: no offset, and the window says so.
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        calibrate_on(dir, "Test Phones");
    });
    let offset = line_offset_of_a_take(cx, &f);
    assert!(close(offset, -2.4), "{offset}");
    let note = f
        .view
        .read_with(cx, |view, _| view.panel.calibration.clone())
        .expect("a note");
    assert!(
        note.contains("calibration does not match these devices"),
        "{note}"
    );
    let offer = f
        .view
        .read_with(cx, |view, _| view.setup_offer.clone())
        .expect("an offer");
    assert!(offer.contains("does not match these devices"), "{offer}");

    // Calibrated on the chosen output, which is gone: no take is recorded
    // on the speakers instead, and the offset does not apply to them.
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        tw::set_route(dir, None, Some("Gone Phones")).expect("route");
        calibrate_on(dir, "Gone Phones");
    });
    press(cx, f.window, &["space"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_recording()));
    assert!(message(cx, &f.view).starts_with("Gone Phones is not connected"));
    assert!(
        f.view
            .read_with(cx, |view, _| view.panel.calibration.clone())
            .is_some_and(|note| note.contains("does not match these devices"))
    );
}

#[gpui_kit::test]
fn long_line_scrolls_forward_and_a_retake_starts_at_the_top(cx: &mut TestAppContext) {
    let words = "every answer becomes a page worth opening again ".repeat(9);
    let script = format!(
        "[[line]]\nid = \"long\"\nstart = 0.0\nend = 10.0\ntext = \"{}\"\n",
        words.trim()
    );
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        fs::write(dir.join("script.toml"), &script).expect("script");
    });
    begin(cx, &f, 100.0); // film 0 at host 102.4
    let mut seen = Vec::new();
    for step in 0..=40 {
        let now = 102.4 + f64::from(step) * 0.25;
        f.media.set_now(now);
        f.media.send(now, 480);
        frame(cx, f.window);
        frame(cx, f.window);
        seen.push(f.view.read_with(cx, |view, _| view.scroll.at(now)));
    }
    assert!(
        seen.windows(2).all(|w| w[1] >= w[0] - 1e-6),
        "never scrolls back during a take: {seen:?}"
    );
    let last = *seen.last().expect("values");
    assert!(last >= 1.0, "a long line scrolls: {seen:?}");
    assert!(seen[0].abs() < f32::EPSILON, "the first rows hold still");

    press(cx, f.window, &["r"]);
    let now = f.media.0.borrow().now;
    assert!(
        f.view.read_with(cx, |view, _| view.scroll.at(now)).abs() < f32::EPSILON,
        "a retake starts from the top"
    );
}

/// Five seconds of a voice-like tone (150 Hz with harmonics) peaking at
/// `peak_dbfs`, as capture buffers of 100 ms.
fn voice_buffers(peak_dbfs: f64) -> Vec<Vec<i32>> {
    let full = f64::from((1 << 23) - 1);
    let wave = |t: f64| {
        (1..=8)
            .map(|k| (std::f64::consts::TAU * 150.0 * f64::from(k) * t).sin() / f64::from(k))
            .sum::<f64>()
    };
    let n = 5 * 48_000;
    let raw: Vec<f64> = (0..n).map(|i| wave(f64::from(i) / 48_000.0)).collect();
    let peak = raw.iter().fold(0.0_f64, |m, v| m.max(v.abs()));
    let gain = 10f64.powf(peak_dbfs / 20.0) * full / peak;
    #[allow(clippy::cast_possible_truncation)]
    let samples: Vec<i32> = raw.iter().map(|v| (v * gain).round() as i32).collect();
    samples.chunks(4_800).map(<[i32]>::to_vec).collect()
}

/// Five seconds of quiet noise at about −70 dBFS, as buffers of 100 ms.
fn room_buffers() -> Vec<Vec<i32>> {
    let mut state: u32 = 0x9e37_79b9;
    let amplitude = f64::from((1 << 23) - 1) * 10f64.powf(-70.0 / 20.0) * 3f64.sqrt();
    #[allow(clippy::cast_possible_truncation)]
    let samples: Vec<i32> = (0..5 * 48_000)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let unit = f64::from(state >> 8) / f64::from(1u32 << 24) * 2.0 - 1.0;
            (unit * amplitude).round() as i32
        })
        .collect();
    samples.chunks(4_800).map(<[i32]>::to_vec).collect()
}

/// Space in the setup check, then the buffers as the capture delivers them
/// over five seconds, then the frame that analyzes them.
fn listen(cx: &mut TestAppContext, f: &Fixture, buffers: &[Vec<i32>]) {
    press(cx, f.window, &["space"]);
    let start = f.media.0.borrow().now;
    for (i, samples) in buffers.iter().enumerate() {
        let sink = f.media.0.borrow().sink.clone().expect("the meter captures");
        sink.send(takewise_media::CaptureBuffer {
            host_time_s: start + 0.1 * i as f64,
            samples: samples.clone(),
        })
        .expect("the window receives");
    }
    f.media.set_now(start + 5.05);
    frame(cx, f.window);
}

fn setup_step(cx: &mut TestAppContext, f: &Fixture) -> Option<super::setup::Step> {
    f.view
        .read_with(cx, |view, _| view.setup.as_ref().map(|s| s.step))
}

/// Opens the setup check and reads the sentence at a level that fits.
fn through_voice(cx: &mut TestAppContext, f: &Fixture) {
    frame(cx, f.window); // the meter's capture starts
    press(cx, f.window, &["c", "enter"]);
    listen(cx, f, &voice_buffers(-10.0));
    press(cx, f.window, &["enter"]);
    assert_eq!(setup_step(cx, f), Some(super::setup::Step::Room));
}

#[gpui_kit::test]
fn setup_offered_on_first_open(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    frame(cx, f.window);
    let offer = f.view.read_with(cx, |view, _| view.setup_offer.clone());
    assert!(
        offer
            .as_deref()
            .is_some_and(|o| o.contains("no setup check")),
        "{offer:?}"
    );
    // Offered, not forced: Space still records.
    assert_eq!(setup_step(cx, &f), None);
    press(cx, f.window, &["c"]);
    assert_eq!(setup_step(cx, &f), Some(super::setup::Step::Devices));
    // The speakers are flagged.
    assert!(
        f.view
            .read_with(cx, |view, _| view.output_warning())
            .is_some_and(|w| w.contains("wired headphones"))
    );
    press(cx, f.window, &["escape"]);
    assert_eq!(setup_step(cx, &f), None);
}

/// The output warning for each of the fake outputs.
fn warning_on(cx: &mut TestAppContext, f: &Fixture, output: &str) -> Option<&'static str> {
    f.view.update(cx, |view, _| view.choose_output(output));
    frame(cx, f.window);
    f.view.read_with(cx, |view, _| view.output_warning())
}

#[gpui_kit::test]
fn output_warning_tells_speakers_from_the_headphone_jack(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    frame(cx, f.window);
    assert_eq!(
        warning_on(cx, &f, "MacBook Speakers"),
        Some("Film sound from speakers will leak into your takes; use wired headphones.")
    );
    assert_eq!(warning_on(cx, &f, "External Headphones"), None);
    assert_eq!(
        warning_on(cx, &f, "Built-in Output"),
        Some(
            "If this is the Mac's speakers, film sound will leak into your takes; use wired headphones."
        )
    );
    assert_eq!(warning_on(cx, &f, "Test Phones"), None);
}

#[gpui_kit::test]
fn setup_not_offered_when_current(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        let info = tw::SetupInfo {
            date: "2026-11-23".to_owned(),
            input: DEVICE.to_owned(),
            channel: 1,
            output: "MacBook Speakers".to_owned(),
            room_floor_dbfs: Some(-70.0),
            speech_to_noise_db: Some(48.0),
            from: None,
        };
        tw::set_setup(dir, &info).expect("setup");
        save_room_tone_at(dir, jiff::Timestamp::now());
        calibrate_on(dir, "MacBook Speakers");
    });
    frame(cx, f.window);
    assert_eq!(
        f.view.read_with(cx, |view, _| view.setup_offer.clone()),
        None
    );
}

/// Keeps a room tone recorded at `at` on the test devices.
fn save_room_tone_at(dir: &Path, at: jiff::Timestamp) {
    let input = tw::room::RoomInput {
        input: DEVICE,
        channel: 1,
        output: "MacBook Speakers",
        floor_dbfs: -70.0,
        gated: false,
    };
    tw::room::save_room_tone(dir, &vec![0; 4_800], 48_000, at, &input).expect("room tone");
}

#[gpui_kit::test]
fn room_step_offered_without_todays_room_tone(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        let info = tw::SetupInfo {
            date: "2026-11-20".to_owned(),
            input: DEVICE.to_owned(),
            channel: 1,
            output: "MacBook Speakers".to_owned(),
            room_floor_dbfs: Some(-70.0),
            speech_to_noise_db: Some(48.0),
            from: None,
        };
        tw::set_setup(dir, &info).expect("setup");
        let days_ago = jiff::Timestamp::now() - jiff::SignedDuration::from_hours(72);
        save_room_tone_at(dir, days_ago);
    });
    frame(cx, f.window);
    let offer = f
        .view
        .read_with(cx, |view, _| view.setup_offer.clone())
        .expect("an offer");
    assert!(offer.contains("no room tone recorded today"), "{offer}");

    // The setup check records today's room tone; the offer is gone.
    through_voice(cx, &f);
    listen(cx, &f, &room_buffers());
    let offer = f.view.read_with(cx, |view, _| view.setup_offer.clone());
    assert!(
        !offer.is_some_and(|o| o.contains("room tone")),
        "the room tone is from today"
    );
}

#[test]
fn room_tone_today_uses_the_local_date() {
    let at: jiff::Timestamp = "2026-11-22T23:30:00Z".parse().expect("time");
    let tone = tw::room::RoomTone {
        file: "takes/.room/x.wav".to_owned(),
        recorded_at: at,
        input: DEVICE.to_owned(),
        channel: 1,
        output: "Test Phones".to_owned(),
        sample_rate: 48_000,
        samples: 0,
        floor_dbfs: -70.0,
        gated: false,
    };
    let route = tw::Route {
        input: Some(DEVICE.to_owned()),
        channel: 1,
        output: Some("Test Phones".to_owned()),
        buffer_frames: None,
    };
    let today = jiff::civil::date(2026, 11, 23);
    let berlin = jiff::tz::TimeZone::fixed(jiff::tz::offset(1));
    let utc = jiff::tz::TimeZone::UTC;
    let tones = std::slice::from_ref(&tone);
    assert!(super::setup::room_tone_today(tones, &route, today, &berlin));
    assert!(!super::setup::room_tone_today(tones, &route, today, &utc));
    let other = tw::Route {
        channel: 2,
        ..route.clone()
    };
    assert!(!super::setup::room_tone_today(
        tones, &other, today, &berlin
    ));
    let speakers = tw::Route {
        output: Some("MacBook Speakers".to_owned()),
        ..route
    };
    assert!(!super::setup::room_tone_today(
        tones, &speakers, today, &berlin
    ));
}

#[gpui_kit::test]
fn setup_never_opens_during_a_take(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    begin(cx, &f, 100.0);
    press(cx, f.window, &["c"]);
    assert_eq!(setup_step(cx, &f), None);
    assert!(message(cx, &f.view).contains("Finish the take"));
}

#[gpui_kit::test]
fn voice_step_repeats_until_level_fits(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    frame(cx, f.window);
    press(cx, f.window, &["c", "enter"]);
    assert_eq!(setup_step(cx, &f), Some(super::setup::Step::Voice));
    // Enter before reading stays.
    press(cx, f.window, &["enter"]);
    assert_eq!(setup_step(cx, &f), Some(super::setup::Step::Voice));

    // Too quiet: the report asks for about 20 dB more gain.
    listen(cx, &f, &voice_buffers(-30.0));
    let voice = f
        .view
        .read_with(cx, |view, _| {
            view.setup.as_ref().and_then(|s| s.voice.clone())
        })
        .expect("a report");
    assert!(!voice.level_fits(), "{voice:?}");
    assert!((voice.gain_change_db - 20.0).abs() < 0.5, "{voice:?}");
    assert_eq!(setup_step(cx, &f), Some(super::setup::Step::Voice));

    // Read again after turning the gain up: it fits.
    listen(cx, &f, &voice_buffers(-10.0));
    let voice = f
        .view
        .read_with(cx, |view, _| {
            view.setup.as_ref().and_then(|s| s.voice.clone())
        })
        .expect("a report");
    assert!(voice.level_fits(), "{voice:?}");
    press(cx, f.window, &["enter"]);
    assert_eq!(setup_step(cx, &f), Some(super::setup::Step::Room));
    // No take was recorded.
    assert_eq!(recorded(&f.dir), []);
}

#[gpui_kit::test]
fn room_tone_saved_with_devices(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    through_voice(cx, &f);
    listen(cx, &f, &room_buffers());
    let room = f
        .view
        .read_with(cx, |view, _| {
            view.setup.as_ref().and_then(|s| s.room.clone())
        })
        .expect("a room report");
    assert!((room.floor_dbfs + 70.0).abs() < 1.5, "{room:?}");
    let tones = tw::room::room_tones(&f.dir).expect("room tones");
    assert_eq!(tones.len(), 1, "{tones:?}");
    let tone = &tones[0];
    assert_eq!(
        (tone.input.as_str(), tone.channel, tone.output.as_str()),
        (DEVICE, 1, "MacBook Speakers")
    );
    assert!((tone.floor_dbfs - room.floor_dbfs).abs() < 1e-9);
    assert!(!tone.gated);
    assert_eq!(tone.samples, 5 * 48_000);
    assert!(f.dir.join(&tone.file).is_file());
    assert!(tone.file.starts_with("takes/.room/"), "{}", tone.file);
    assert_eq!(recorded(&f.dir), []);
}

#[gpui_kit::test]
fn setup_writes_project_toml(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    through_voice(cx, &f);
    listen(cx, &f, &room_buffers());
    let setup = Project::open(&f.dir)
        .expect("project")
        .config
        .setup
        .expect("[setup] written");
    assert_eq!(setup.input, DEVICE);
    assert_eq!(setup.channel, 1);
    assert_eq!(setup.output, "MacBook Speakers");
    let floor = setup.room_floor_dbfs.expect("a floor");
    assert!((floor + 70.0).abs() < 1.5, "{floor}");
    let snr = setup.speech_to_noise_db.expect("speech to noise");
    assert!(snr > 40.0, "{snr}");
    assert_eq!(setup.date.len(), 10);
    // Only the latency calibration is still offered, also for a fresh
    // project with no offset: Enter goes on to it, Enter again skips it.
    let offer = f
        .view
        .read_with(cx, |view, _| view.setup_offer.clone())
        .expect("an offer");
    assert_eq!(offer, "the latency is not calibrated for these devices");
    press(cx, f.window, &["enter"]);
    assert_eq!(setup_step(cx, &f), Some(super::setup::Step::Latency));
    press(cx, f.window, &["enter"]);
    assert_eq!(setup_step(cx, &f), None);
    let shown = message(cx, &f.view);
    assert!(
        shown.contains("Setup check saved; the latency is not calibrated"),
        "{shown}"
    );
}

/// The beep film in tests: an empty file, since the fake plays nothing.
fn fake_beep_film(dir: &Path) -> Result<PathBuf, String> {
    let film = dir.join("beeps.mov");
    fs::write(&film, b"").map_err(|err| err.to_string())?;
    Ok(film)
}

/// What the input hears at stamp time `stamp`: each audible play's beeps,
/// heard `late_s` after the film plays them, through the fake's latency,
/// over quiet noise.
fn loopback_sample(fake: &Fake, stamp: f64, late_s: f64, noise: &mut u32) -> i32 {
    let full = f64::from((1 << 23) - 1);
    *noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    let unit = f64::from(*noise >> 8) / f64::from(1u32 << 24) * 2.0 - 1.0;
    let mut value = unit * full * 0.001;
    let heard = stamp - LATENCY_S - late_s;
    for &(from, host) in &fake.audible {
        let stop = fake
            .pauses
            .iter()
            .copied()
            .find(|&p| p > host)
            .unwrap_or(f64::INFINITY);
        for beep in crate::calibrate::BEEPS.iter().filter(|&&b| b >= from) {
            let at = host + (beep - from);
            if at < stop && (at..at + 0.02).contains(&heard) {
                value += (std::f64::consts::TAU * 1000.0 * (heard - at)).sin() * 0.3 * full;
            }
        }
    }
    #[allow(clippy::cast_possible_truncation)]
    let sample = value.round() as i32;
    sample
}

/// Runs the calibration the latency step started: 20 ms buffers of what
/// the input hears (`beeps`: the loopback, else silence) until it ends.
fn run_calibration(cx: &mut TestAppContext, f: &Fixture, beeps: bool) {
    let mut noise = 0x9e37_79b9_u32;
    for _ in 0..2000 {
        if !f.view.read_with(cx, |view, _| view.calibrating.is_some()) {
            return;
        }
        let (start, sink) = {
            let fake = f.media.0.borrow();
            (fake.now, fake.sink.clone().expect("the meter captures"))
        };
        let samples: Vec<i32> = {
            let fake = f.media.0.borrow();
            (0..960)
                .map(|i| {
                    let stamp = start + f64::from(i) / 48_000.0;
                    if beeps {
                        loopback_sample(&fake, stamp, 0.03, &mut noise)
                    } else {
                        0
                    }
                })
                .collect()
        };
        sink.send(takewise_media::CaptureBuffer {
            host_time_s: start,
            samples,
        })
        .expect("the window receives");
        f.media.set_now(start + 0.02);
        frame(cx, f.window);
    }
    panic!("the calibration did not end");
}

/// Opens the setup check through the room step to the latency step.
fn to_latency_step(cx: &mut TestAppContext, f: &Fixture) {
    f.view.update(cx, |view, _| view.beep_film = fake_beep_film);
    through_voice(cx, f);
    listen(cx, f, &room_buffers());
    press(cx, f.window, &["enter"]);
    assert_eq!(setup_step(cx, f), Some(super::setup::Step::Latency));
}

#[gpui_kit::test]
fn setup_check_calibrates_the_latency(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    to_latency_step(cx, &f);
    press(cx, f.window, &["space"]);
    assert!(f.view.read_with(cx, |view, _| view.calibrating.is_some()));
    run_calibration(cx, &f, true);

    let latency = setup_of(cx, &f).latency.expect("an outcome");
    let stored = latency.expect("calibrated");
    assert!(stored.contains("latency_offset_ms = -30"), "{stored}");
    assert!(stored.ends_with("Press Enter to finish."), "{stored}");
    let config = Project::open(&f.dir).expect("project").config;
    assert_eq!(config.input.latency_offset_ms, -30);
    let cal = config.calibration.expect("[calibration]");
    assert_eq!(
        (cal.input.as_str(), cal.output.as_str(), cal.buffer_frames),
        (DEVICE, "MacBook Speakers", Some(512))
    );
    // The beep film played, then the project's film came back.
    let films = f.media.0.borrow().films.clone();
    assert_eq!(films.len(), 2, "{films:?}");
    assert!(films[0].ends_with("beeps.mov"), "{films:?}");
    assert_eq!(
        films[1],
        Project::open(&f.dir).expect("project").film_path()
    );
    // Calibrated for these devices: no offer, and Enter finishes.
    assert_eq!(
        f.view.read_with(cx, |view, _| view.setup_offer.clone()),
        None
    );
    press(cx, f.window, &["enter"]);
    assert!(
        message(cx, &f.view).contains("the latency is calibrated"),
        "{}",
        message(cx, &f.view)
    );
    assert_eq!(recorded(&f.dir), [], "no take was recorded");
}

#[gpui_kit::test]
fn the_latency_measurement_waits_for_a_missing_output(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        tw::set_route(dir, None, Some("Test Phones")).expect("output");
    });
    to_latency_step(cx, &f);
    unplug(&f, "Test Phones");
    press(cx, f.window, &["space"]);
    assert!(!f.view.read_with(cx, |view, _| view.calibrating.is_some()));
    let err = setup_of(cx, &f)
        .latency
        .expect("an outcome")
        .expect_err("refused");
    assert_eq!(
        err,
        "Test Phones is not connected: connect it or pick another output (A)."
    );
    assert_eq!(f.media.0.borrow().films.len(), 0, "no beeps played");
}

#[gpui_kit::test]
fn the_film_comes_back_when_the_output_goes_during_the_measurement(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        tw::set_route(dir, None, Some("Test Phones")).expect("output");
    });
    to_latency_step(cx, &f);
    press(cx, f.window, &["space"]);
    assert!(f.view.read_with(cx, |view, _| view.calibrating.is_some()));
    some_calibration(cx, &f, 10);
    let films = f.media.0.borrow().films.len();
    unplug(&f, "Test Phones");
    some_calibration(cx, &f, 150);
    assert!(!f.view.read_with(cx, |view, _| view.calibrating.is_some()));
    // Not reopened on the device that went away: it would fail at once.
    assert_eq!(
        f.media.0.borrow().films.len(),
        films,
        "not while it is gone"
    );
    // The output returns: the film is reopened on it.
    plug(&f, "Test Phones");
    let now = f.media.0.borrow().now;
    f.media.set_now(now + 3.0);
    frame(cx, f.window);
    assert_eq!(
        f.media.0.borrow().output_sets.last(),
        Some(&Some("Test Phones".to_owned()))
    );
    let films = f.media.0.borrow().films.clone();
    assert_eq!(
        films.last(),
        Some(&Project::open(&f.dir).expect("project").film_path()),
        "the project's film is back: {films:?}"
    );
}

#[gpui_kit::test]
fn a_calibration_that_sees_the_output_go_first_does_not_reopen_on_it(cx: &mut TestAppContext) {
    // The calibration's own read finds the phones gone before CoreAudio
    // says so: the film is not reopened on the route that just dropped,
    // only once the phones are back and set again.
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        tw::set_route(dir, None, Some("Test Phones")).expect("output");
    });
    to_latency_step(cx, &f);
    press(cx, f.window, &["space"]);
    some_calibration(cx, &f, 10);
    let opened = films(&f);
    unplug_quietly(&f, "Test Phones");
    some_calibration(cx, &f, 2);
    assert!(!f.view.read_with(cx, |view, _| view.calibrating.is_some()));
    let now = f.media.0.borrow().now;
    for k in 1..=5 {
        f.media.set_now(now + f64::from(k));
        frame(cx, f.window);
    }
    assert_eq!(films(&f), opened, "not on the dropped route");
    plug(&f, "Test Phones");
    f.media.set_now(now + 6.0);
    frame(cx, f.window);
    assert_eq!(films(&f), opened + 1, "reopened once back");
    assert_eq!(f.media.0.borrow().films.last(), Some(&film_of(&f)));
}

#[gpui_kit::test]
fn a_calibration_does_not_inherit_the_films_load_deadline(cx: &mut TestAppContext) {
    // A reopened film still loading when the measurement starts (the
    // beeps load slowly too): the film's ten seconds pass during the
    // beeps, which have their own deadline, and do not cancel them.
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        tw::set_route(dir, None, Some("Test Phones")).expect("output");
    });
    to_latency_step(cx, &f);
    let now = f.media.0.borrow().now;
    off_and_on(&f, "film");
    f.media.0.borrow_mut().film_loading = true;
    f.media.set_now(now + 0.1);
    frame(cx, f.window);
    f.media.set_now(now + 9.0);
    press(cx, f.window, &["space"]);
    assert!(f.view.read_with(cx, |view, _| view.calibrating.is_some()));
    some_calibration(cx, &f, 100);
    assert!(f.media.0.borrow().now > now + 10.5);
    assert!(
        f.view.read_with(cx, |view, _| view.calibrating.is_some()),
        "{:?}",
        setup_of(cx, &f).latency
    );
}

#[gpui_kit::test]
fn a_failed_beep_film_says_why_in_the_latency_step(cx: &mut TestAppContext) {
    // AVFoundation fails the beeps' player with the route unchanged: the
    // calibration stops, and the latency step says why.
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        tw::set_route(dir, None, Some("Test Phones")).expect("output");
    });
    to_latency_step(cx, &f);
    press(cx, f.window, &["space"]);
    some_calibration(cx, &f, 10);
    f.media.0.borrow_mut().film_failed = Some("Cannot decode".to_owned());
    some_calibration(cx, &f, 1);
    assert!(!f.view.read_with(cx, |view, _| view.calibrating.is_some()));
    let err = setup_of(cx, &f)
        .latency
        .expect("an outcome")
        .expect_err("cancelled");
    assert!(err.contains("Cannot decode"), "{err}");
    assert!(err.contains("press Space to measure again"), "{err}");
    assert_eq!(
        f.media.0.borrow().films.last(),
        Some(&film_of(&f)),
        "the project's film is back"
    );
}

#[gpui_kit::test]
fn a_reconnect_during_the_latency_measurement_cancels_it(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        tw::set_route(dir, None, Some("Test Phones")).expect("output");
    });
    to_latency_step(cx, &f);
    press(cx, f.window, &["space"]);
    some_calibration(cx, &f, 10);
    assert!(f.view.read_with(cx, |view, _| view.calibrating.is_some()));
    // A lookup that fails for a moment is no reason to stop.
    f.media.0.borrow_mut().lookups_fail = true;
    some_calibration(cx, &f, 3);
    f.media.0.borrow_mut().lookups_fail = false;
    some_calibration(cx, &f, 1);
    assert!(f.view.read_with(cx, |view, _| view.calibrating.is_some()));
    // Back between two ticks as another device: the beeps may have
    // played elsewhere meanwhile.
    unplug(&f, "Test Phones");
    plug(&f, "Test Phones");
    some_calibration(cx, &f, 1);
    assert!(!f.view.read_with(cx, |view, _| view.calibrating.is_some()));
    let err = setup_of(cx, &f)
        .latency
        .expect("an outcome")
        .expect_err("cancelled");
    assert_eq!(err, super::latency::OUTPUT_CHANGED);
    assert!(message(cx, &f.view).contains("press Space to measure again"));
}

#[gpui_kit::test]
fn the_film_level_leaves_the_beeps_alone(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    to_latency_step(cx, &f);
    press(cx, f.window, &["space"]);
    some_calibration(cx, &f, 10);
    assert_eq!(f.media.0.borrow().film_gain_db, Some(0.0), "beeps at 0 dB");
    drag_slider(cx, &f, Level::Film, -30.0);
    assert_eq!(f.media.0.borrow().film_gain_db, Some(0.0), "still");
    f.view
        .read_with(cx, |view, _| assert_eq!(view.volumes.film, -30.0));
    // Ended: the film is back, at the level set meanwhile.
    press(cx, f.window, &["escape"]);
    assert!(!f.view.read_with(cx, |view, _| view.calibrating.is_some()));
    assert_eq!(f.media.0.borrow().film_gain_db, Some(-30.0));
}

#[gpui_kit::test]
fn a_calibration_without_beeps_stores_nothing(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    to_latency_step(cx, &f);
    press(cx, f.window, &["space"]);
    run_calibration(cx, &f, false);
    let err = setup_of(cx, &f)
        .latency
        .expect("an outcome")
        .expect_err("failed");
    assert!(err.contains("no beeps detected"), "{err}");
    let config = Project::open(&f.dir).expect("project").config;
    assert_eq!(config.calibration, None);
    assert_eq!(f.media.0.borrow().films.len(), 2, "the film is back");
    // Escape mid-run cancels and puts the film back too.
    press(cx, f.window, &["space"]);
    assert!(f.view.read_with(cx, |view, _| view.calibrating.is_some()));
    press(cx, f.window, &["escape"]);
    assert!(!f.view.read_with(cx, |view, _| view.calibrating.is_some()));
    assert_eq!(f.media.0.borrow().films.len(), 4);
}

/// Feeds the running calibration `n` buffers of what the input hears.
fn some_calibration(cx: &mut TestAppContext, f: &Fixture, n: usize) {
    let mut noise = 0x9e37_79b9_u32;
    for _ in 0..n {
        let (start, sink) = {
            let fake = f.media.0.borrow();
            (fake.now, fake.sink.clone().expect("the meter captures"))
        };
        let samples: Vec<i32> = {
            let fake = f.media.0.borrow();
            (0..960)
                .map(|i| loopback_sample(&fake, start + f64::from(i) / 48_000.0, 0.03, &mut noise))
                .collect()
        };
        let _ = sink.send(takewise_media::CaptureBuffer {
            host_time_s: start,
            samples,
        });
        f.media.set_now(start + 0.02);
        frame(cx, f.window);
    }
}

#[gpui_kit::test]
fn changing_the_devices_cancels_a_calibration(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000), ("Other Mic", 48_000)]);
    to_latency_step(cx, &f);
    let changes: [fn(&mut RecordingWindow); 2] = [
        |view| view.choose_output("Test Phones"),
        |view| view.choose_input("Other Mic"),
    ];
    for change in changes {
        press(cx, f.window, &["space"]);
        assert!(f.view.read_with(cx, |view, _| view.calibrating.is_some()));
        some_calibration(cx, &f, 10);
        f.view.update(cx, |view, _| change(view));
        assert!(!f.view.read_with(cx, |view, _| view.calibrating.is_some()));
        let err = setup_of(cx, &f)
            .latency
            .expect("an outcome")
            .expect_err("cancelled");
        assert!(
            err.contains("calibration cancelled: the devices changed"),
            "{err}"
        );
        let said = message(cx, &f.view);
        assert!(
            said.contains("calibration cancelled: the devices changed"),
            "{said}"
        );
        some_calibration(cx, &f, 5);
        let config = Project::open(&f.dir).expect("project").config;
        assert_eq!(config.calibration, None, "nothing measured on mixed routes");
    }
}

/// The setup check's state, read from the window.
fn setup_of(cx: &mut TestAppContext, f: &Fixture) -> super::setup::Setup {
    f.view
        .read_with(cx, |view, _| view.setup.clone())
        .expect("the setup check is open")
}

#[gpui_kit::test]
fn a_silent_voice_step_does_not_go_on(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    frame(cx, f.window);
    press(cx, f.window, &["c", "enter"]);
    let silence: Vec<Vec<i32>> = (0..50).map(|_| vec![0; 4_800]).collect();
    listen(cx, &f, &silence);
    let voice = setup_of(cx, &f).voice.expect("a report");
    assert!(!super::setup::heard_voice(&voice), "{voice:?}");
    press(cx, f.window, &["enter"]);
    let setup = setup_of(cx, &f);
    assert_eq!(setup.step, super::setup::Step::Voice, "it stays");
    let error = setup.error.expect("says why");
    assert!(
        error.contains("No voice heard on channel 1 — check the channel in the audio panel"),
        "{error}"
    );
    // A opens the audio panel from the setup check.
    press(cx, f.window, &["a"]);
    assert!(f.view.read_with(cx, |view, _| view.panel.open));
    // A voice heard afterwards goes on.
    listen(cx, &f, &voice_buffers(-10.0));
    press(cx, f.window, &["enter"]);
    assert_eq!(setup_step(cx, &f), Some(super::setup::Step::Room));
}

#[gpui_kit::test]
fn a_room_tone_that_cannot_be_kept_is_not_saved(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    // A file where the room tone directory goes.
    fs::write(f.dir.join("takes/.room"), b"").expect("block");
    through_voice(cx, &f);
    listen(cx, &f, &room_buffers());
    let error = setup_of(cx, &f).save_error.expect("an error");
    assert!(error.contains("Could not keep the room tone"), "{error}");
    assert_eq!(Project::open(&f.dir).expect("project").config.setup, None);

    press(cx, f.window, &["enter"]);
    let setup = setup_of(cx, &f);
    assert_eq!(setup.step, super::setup::Step::Room, "the step stays");
    assert!(setup.save_error.is_some(), "the error stays visible");
    assert!(!message(cx, &f.view).contains("Setup check saved"));
}

#[gpui_kit::test]
fn a_setup_that_cannot_be_written_is_not_saved(cx: &mut TestAppContext) {
    use std::os::unix::fs::PermissionsExt as _;
    let f = open(cx, &[(DEVICE, 48_000)]);
    fs::create_dir_all(f.dir.join("takes/.room")).expect("room dir");
    through_voice(cx, &f);
    fs::set_permissions(&f.dir, fs::Permissions::from_mode(0o555)).expect("lock");
    listen(cx, &f, &room_buffers());
    press(cx, f.window, &["enter"]);
    fs::set_permissions(&f.dir, fs::Permissions::from_mode(0o755)).expect("unlock");
    let setup = setup_of(cx, &f);
    let error = setup.save_error.expect("an error");
    assert!(error.contains("Could not save the setup check"), "{error}");
    assert_eq!(setup.step, super::setup::Step::Room);
    assert!(!message(cx, &f.view).contains("Setup check saved"));

    // Recording the room again saves it.
    listen(cx, &f, &room_buffers());
    assert_eq!(setup_of(cx, &f).save_error, None);
    assert!(
        Project::open(&f.dir)
            .expect("project")
            .config
            .setup
            .is_some()
    );
}

/// An installer that reports half the model, waits for the test to let it
/// finish, then hears with its transcriber.
struct FakeInstaller {
    gate: Mutex<Receiver<()>>,
    transcriber: SharedTranscriber,
}

impl super::model::Installer for FakeInstaller {
    fn install(
        &self,
        progress: &mut dyn FnMut(tw::models::Progress) -> std::ops::ControlFlow<()>,
    ) -> Result<(), String> {
        let _ = progress(tw::models::Progress {
            bytes: 812_277_637,
            total: 1_624_555_275,
            verifying: false,
        });
        let _ = self.gate.lock().expect("gate").recv();
        Ok(())
    }

    fn checker(&self, _: &Project) -> Checker {
        Checker {
            transcriber: Arc::clone(&self.transcriber),
            tools: Vec::new(),
            tag: worker_tag(),
        }
    }
}

fn offer_model(cx: &mut TestAppContext, f: &Fixture, installer: FakeInstaller) {
    f.view.update(cx, |view, _| {
        view.model = Some(super::model::ModelPrompt::new(
            Arc::new(installer),
            1_624_555_275,
        ));
    });
}

fn banner(cx: &mut TestAppContext, f: &Fixture) -> Option<String> {
    f.view
        .read_with(cx, |view, _| view.model.as_ref().map(|m| m.text()))
}

#[gpui_kit::test]
fn banner_when_model_missing(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    frame(cx, f.window);
    assert_eq!(banner(cx, &f), None, "no banner while the model is there");

    let (_release, gate) = channel();
    offer_model(
        cx,
        &f,
        FakeInstaller {
            gate: Mutex::new(gate),
            transcriber: no_transcriber(),
        },
    );
    frame(cx, f.window);
    let text = banner(cx, &f).expect("a banner");
    assert!(text.contains("Install the speech model (1.5 GB)"), "{text}");
}

#[gpui_kit::test]
fn progress_then_checks_resume(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    // Recorded before the model is there: checked without words.
    record_take(cx, &f);
    wait_until(cx, &f, || checked_events(&f.dir) == 1);
    let take = tw::TakeRef {
        line: "a".to_owned(),
        take: 1,
    };
    let unheard = |dir: &Path| {
        let project = Project::open(dir).expect("project");
        tw::words_not_checked_takes(&project, &tw::read(dir).expect("ledger"))
    };
    assert_eq!(unheard(&f.dir), vec![take.clone()]);

    let (release, gate) = channel();
    let (_, heard_gate) = channel();
    let hears = Arc::new(Gated {
        gate: Mutex::new(heard_gate),
        heard: Mutex::new(Vec::new()),
    });
    offer_model(
        cx,
        &f,
        FakeInstaller {
            gate: Mutex::new(gate),
            transcriber: hears.clone(),
        },
    );
    press(cx, f.window, &["i"]);
    wait_for(cx, &f.view, |view| {
        view.model
            .as_ref()
            .and_then(super::model::ModelPrompt::fraction)
            .is_some_and(|f| f > 0.0)
    });
    let text = banner(cx, &f).expect("the banner shows progress");
    assert!(text.contains("0.8 GB of 1.5 GB (50 %)"), "{text}");
    assert_eq!(hears.heard.lock().expect("heard").len(), 0);

    release.send(()).expect("the install waits");
    wait_until(cx, &f, || checked_events(&f.dir) == 2);
    assert_eq!(banner(cx, &f), None, "the banner is gone once installed");
    assert_eq!(
        *hears.heard.lock().expect("heard"),
        vec![f.dir.join("takes/a/001.wav")],
        "the take is checked again, now with words"
    );
    assert_eq!(unheard(&f.dir), Vec::<tw::TakeRef>::new());
    let shown = message(cx, &f.view);
    assert!(shown.contains("checking the words of 1 take"), "{shown}");
}

#[gpui_kit::test]
fn a_denied_microphone_blocks_recording_with_the_settings_path(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    f.media.0.borrow_mut().mic = tw::MicPermission::Denied;
    press(cx, f.window, &["space"]);
    assert!(!f.media.capturing(), "no take starts");
    let blocked = f
        .view
        .read_with(cx, |view, _| view.blocked().map(str::to_owned))
        .expect("blocked");
    assert!(
        blocked.contains("System Settings › Privacy & Security › Microphone"),
        "{blocked}"
    );

    // Not asked yet: opening the input asks, so recording goes ahead.
    f.media.0.borrow_mut().mic = tw::MicPermission::NotDetermined;
    press(cx, f.window, &["space"]);
    assert!(f.media.capturing(), "the take starts and macOS asks");
}

// The start window.

#[gpui_kit::test]
fn opening_a_project_already_open_focuses_its_window(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    let start = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| super::start::StartWindow::new(Vec::new(), window, cx))
        })
        .expect("open the start window")
    });
    assert_eq!(cx.windows().len(), 2);
    // The same folder by another path.
    let again = f.dir.join("takes").join("..");
    cx.update_window(start.0, |_, window, cx| {
        start
            .1
            .update(cx, |view, cx| view.open_project(&again, window, cx));
    })
    .expect("start window");
    let message = start.1.read_with(cx, |view, _| view.message.clone());
    assert_eq!(message, None, "no error");
    assert_eq!(
        cx.windows(),
        vec![f.window],
        "no second window on the project"
    );
    let recent = super::start::recent_path().expect("isolated settings");
    assert_eq!(
        super::start::load_recent(&recent),
        vec![fs::canonicalize(&f.dir).expect("canonical")],
        "remembered resolved, in the test's own settings"
    );
}

#[gpui_kit::test]
fn a_note_for_an_open_project_shows_in_its_window(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    let note = "Promo.takewise is the project of Promo.mp4".to_owned();
    cx.update(|cx| super::tell_open(&f.dir.join("takes/.."), note.clone(), cx));
    assert_eq!(message(cx, &f.view), note);
}

/// The remembered setup of the test's own settings folder.
fn remembered_store() -> PathBuf {
    tw::remembered::beside(&super::start::isolate_settings())
}

/// Remembers `offset_ms` measured on the test mic and `output` with
/// `buffer` frames on `date`.
fn remember_measurement(output: &str, buffer: u32, offset_ms: i32, date: &str) {
    let info = tw::CalibrationInfo {
        input: DEVICE.to_owned(),
        output: output.to_owned(),
        buffer_frames: Some(buffer),
        date: date.to_owned(),
        from: None,
    };
    assert!(
        tw::remembered::remember_latency(&remembered_store(), offset_ms, &info).expect("remember")
    );
}

/// Uses the test's settings, as the window does when it opens.
fn use_settings(cx: &mut TestAppContext, f: &Fixture) {
    let settings = super::start::isolate_settings();
    f.view
        .update(cx, |view, _| view.use_settings(Some(settings)));
    frame(cx, f.window);
}

#[gpui_kit::test]
fn a_remembered_latency_applies_when_the_window_opens(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    remember_measurement("MacBook Speakers", 512, -29, "2026-11-23");
    use_settings(cx, &f);
    let config = Project::open(&f.dir).expect("project").config;
    assert_eq!(config.input.latency_offset_ms, -29);
    let cal = config.calibration.expect("[calibration]");
    assert!(cal.is_remembered(), "{cal:?}");
    assert_eq!(
        (cal.output.as_str(), cal.buffer_frames, cal.date.as_str()),
        ("MacBook Speakers", Some(512), "2026-11-23")
    );
    let note = f
        .view
        .read_with(cx, |view, _| view.panel.remembered.clone())
        .expect("a note");
    assert_eq!(
        note,
        "Latency from an earlier measurement on these devices (2026-11-23)."
    );
    assert_eq!(
        f.view
            .read_with(cx, |view, _| view.panel.calibration.clone()),
        None
    );
    assert!(
        message(cx, &f.view).contains("the latency measured on these devices on 2026-11-23"),
        "{}",
        message(cx, &f.view)
    );
    // Takes are placed with it.
    let offset = line_offset_of_a_take(cx, &f);
    assert!((offset - -2.429).abs() < 1e-6, "{offset}");
}

#[gpui_kit::test]
fn a_remembered_latency_follows_the_route(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    // Measured with another buffer on these speakers, and with this
    // buffer on the phones.
    remember_measurement("MacBook Speakers", 256, -25, "2026-11-22");
    remember_measurement("Test Phones", 512, -29, "2026-11-23");
    use_settings(cx, &f);
    let config = Project::open(&f.dir).expect("project").config;
    assert_eq!(
        config.calibration, None,
        "another buffer size never applies"
    );
    assert_eq!(
        f.view
            .read_with(cx, |view, _| view.panel.remembered.clone()),
        None
    );

    f.view
        .update(cx, |view, _| view.choose_output("Test Phones"));
    frame(cx, f.window);
    let config = Project::open(&f.dir).expect("project").config;
    assert_eq!(config.input.latency_offset_ms, -29);
    assert!(config.calibration.expect("[calibration]").is_remembered());
    let note = f
        .view
        .read_with(cx, |view, _| view.panel.remembered.clone())
        .expect("a note");
    assert!(!note.contains("Bluetooth"), "{note}");

    // Back on the speakers nothing fits: the window says the calibration
    // does not match, as before.
    f.view
        .update(cx, |view, _| view.choose_output("MacBook Speakers"));
    frame(cx, f.window);
    assert_eq!(
        f.view
            .read_with(cx, |view, _| view.panel.remembered.clone()),
        None
    );
    assert!(
        f.view
            .read_with(cx, |view, _| view.panel.calibration.clone())
            .is_some_and(|note| note.contains("does not match these devices"))
    );
}

#[gpui_kit::test]
fn a_projects_own_calibration_wins_and_is_remembered(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        calibrate_on(dir, "MacBook Speakers");
    });
    remember_measurement("MacBook Speakers", 512, -29, "2026-11-20");
    use_settings(cx, &f);
    let config = Project::open(&f.dir).expect("project").config;
    assert_eq!(config.input.latency_offset_ms, 25);
    assert!(!config.calibration.expect("[calibration]").is_remembered());
    // It is newer than the one remembered, so it replaces it.
    let remembered = tw::remembered::read(&remembered_store()).expect("remembered");
    assert_eq!(remembered.latency.len(), 1, "{remembered:?}");
    assert_eq!(remembered.latency[0].latency_offset_ms, 25);
}

#[gpui_kit::test]
fn a_bluetooth_output_offers_to_measure_again(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    f.media.0.borrow_mut().outputs.push(output(
        "WH-1000XM5",
        Transport::Bluetooth,
        OutputSource::Unknown,
    ));
    remember_measurement("WH-1000XM5", 512, -29, "2026-11-23");
    use_settings(cx, &f);
    f.view
        .update(cx, |view, _| view.choose_output("WH-1000XM5"));
    frame(cx, f.window);
    let note = f
        .view
        .read_with(cx, |view, _| view.panel.remembered.clone())
        .expect("a note");
    assert_eq!(
        note,
        format!(
            "Latency from an earlier measurement on these devices (2026-11-23). {}",
            super::remembered::BLUETOOTH_HINT
        )
    );
    // The setup check goes on to the latency step although it fits.
    to_latency_step(cx, &f);
}

#[gpui_kit::test]
fn l_measures_the_latency_again_on_the_same_route(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    f.view.update(cx, |view, _| view.beep_film = fake_beep_film);
    f.media.0.borrow_mut().outputs.push(output(
        "WH-1000XM5",
        Transport::Bluetooth,
        OutputSource::Unknown,
    ));
    remember_measurement("WH-1000XM5", 512, -29, "2026-11-23");
    use_settings(cx, &f);
    f.view
        .update(cx, |view, _| view.choose_output("WH-1000XM5"));
    frame(cx, f.window);
    // The Bluetooth output is flagged in plain words, and L is named.
    let warning = f
        .view
        .read_with(cx, |view, _| view.output_warning())
        .expect("a warning");
    assert!(warning.contains("latency measurement (L)"), "{warning}");
    assert!(!warning.contains("not supported"), "{warning}");
    let note = f
        .view
        .read_with(cx, |view, _| view.panel.remembered.clone())
        .expect("a note");
    assert!(note.contains("press L to measure it again"), "{note}");

    // L opens the latency step alone; once it stored a measurement, it
    // closes and says what it stored.
    for round in 0..2 {
        press(cx, f.window, &["l"]);
        assert_eq!(
            setup_step(cx, &f),
            Some(super::setup::Step::Latency),
            "round {round}"
        );
        press(cx, f.window, &["space"]);
        assert!(f.view.read_with(cx, |view, _| view.calibrating.is_some()));
        run_calibration(cx, &f, true);
        assert_eq!(setup_step(cx, &f), None, "round {round}");
        let shown = message(cx, &f.view);
        assert!(
            shown.starts_with("Latency measured:")
                && shown.contains("Stored latency_offset_ms = -30 for Test Mic and WH-1000XM5."),
            "{shown}"
        );
        // The fresh measurement is the project's own, so C's full check no
        // longer offers the latency step; L still measures on this route.
        let cal = Project::open(&f.dir)
            .expect("project")
            .config
            .calibration
            .expect("[calibration]");
        assert!(!cal.is_remembered(), "{cal:?}");
        assert_eq!(cal.output, "WH-1000XM5");
        // The headphones can reconnect after this measurement too: the
        // panel keeps saying that L measures again.
        let note = f
            .view
            .read_with(cx, |view, _| view.panel.remembered.clone());
        assert_eq!(
            note.as_deref(),
            Some(super::remembered::BLUETOOTH_HINT),
            "round {round}"
        );
    }
    assert_eq!(recorded(&f.dir), [], "no take was recorded");
    // On a wired output, a measurement of the project's own needs no note.
    f.view
        .update(cx, |view, _| view.choose_output("MacBook Speakers"));
    frame(cx, f.window);
    press(cx, f.window, &["l"]);
    press(cx, f.window, &["space"]);
    run_calibration(cx, &f, true);
    let config = Project::open(&f.dir).expect("project").config;
    assert_eq!(
        config.calibration.expect("[calibration]").output,
        "MacBook Speakers"
    );
    let note = f
        .view
        .read_with(cx, |view, _| view.panel.remembered.clone());
    assert_eq!(note, None);
}

#[gpui_kit::test]
fn l_waits_for_the_take_and_esc_closes_it(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    begin(cx, &f, 100.0);
    press(cx, f.window, &["l"]);
    assert_eq!(setup_step(cx, &f), None);
    assert!(message(cx, &f.view).contains("Finish the take first, then press L."));
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    press(cx, f.window, &["l"]);
    assert_eq!(setup_step(cx, &f), Some(super::setup::Step::Latency));
    press(cx, f.window, &["escape"]);
    assert_eq!(setup_step(cx, &f), None);
}

/// Saves a setup check's result in another project with the test mic and
/// `output` today, and remembers it.
fn remember_todays_setup(output: &str) {
    let tmp = tempfile::tempdir().expect("tmp");
    let dir = tmp.path().join("other.takewise");
    let info = FilmInfo {
        duration_s: 20.0,
        size: 0,
        modified: "2026-11-22T00:00:00Z".to_owned(),
    };
    Project::init(&dir, Path::new("film.mp4"), info).expect("init");
    let room = tw::room::RoomInput {
        input: DEVICE,
        channel: 1,
        output,
        floor_dbfs: -71.0,
        gated: false,
    };
    let tone = tw::room::save_room_tone(&dir, &[1, 2, 3], 48_000, jiff::Timestamp::now(), &room)
        .expect("room tone");
    let today = jiff::ZonedDateTime::now().date();
    let setup = tw::SetupInfo {
        date: today.to_string(),
        input: DEVICE.to_owned(),
        channel: 1,
        output: output.to_owned(),
        room_floor_dbfs: Some(-71.0),
        speech_to_noise_db: Some(50.0),
        from: None,
    };
    tw::set_setup(&dir, &setup).expect("[setup]");
    tw::remembered::remember_setup(&remembered_store(), &dir, &setup, &tone, today)
        .expect("remember");
}

#[gpui_kit::test]
fn todays_setup_check_is_adopted(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    remember_todays_setup("MacBook Speakers");
    use_settings(cx, &f);
    let setup = Project::open(&f.dir)
        .expect("project")
        .config
        .setup
        .expect("[setup] adopted");
    assert!(setup.is_remembered(), "{setup:?}");
    assert_eq!(setup.room_floor_dbfs, Some(-71.0));
    let tones = tw::room::room_tones(&f.dir).expect("room tones");
    assert_eq!(tones.len(), 1, "{tones:?}");
    assert!(f.dir.join(&tones[0].file).is_file());
    // Only the latency is still offered.
    let offer = f
        .view
        .read_with(cx, |view, _| view.setup_offer.clone())
        .expect("an offer");
    assert_eq!(offer, "the latency is not calibrated for these devices");
}

#[gpui_kit::test]
fn nothing_is_used_while_the_input_is_missing(cx: &mut TestAppContext) {
    let f = open(cx, &[("Other Mic", 48_000)]);
    remember_todays_setup("MacBook Speakers");
    remember_measurement("MacBook Speakers", 512, -29, "2026-11-23");
    use_settings(cx, &f);
    let config = Project::open(&f.dir).expect("project").config;
    assert_eq!((config.calibration, config.setup), (None, None));
    // Once it is connected, both are used.
    f.media.0.borrow_mut().inputs.push(AvailableInput {
        name: DEVICE.to_owned(),
        sample_rate: 48_000,
    });
    f.view.update(cx, |view, _| view.check_inputs());
    let config = Project::open(&f.dir).expect("project").config;
    assert!(config.calibration.is_some_and(|c| c.is_remembered()));
    assert!(config.setup.is_some_and(|s| s.is_remembered()));
}

#[gpui_kit::test]
fn a_setup_check_for_other_devices_is_not_adopted(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    remember_todays_setup("Test Phones");
    use_settings(cx, &f);
    assert_eq!(Project::open(&f.dir).expect("project").config.setup, None);
    assert_eq!(tw::room::room_tones(&f.dir).expect("room tones"), []);
    // Switching to those phones adopts it.
    f.view
        .update(cx, |view, _| view.choose_output("Test Phones"));
    frame(cx, f.window);
    let setup = Project::open(&f.dir).expect("project").config.setup;
    assert!(setup.is_some_and(|s| s.is_remembered()));
}

#[gpui_kit::test]
fn the_setup_check_and_its_calibration_are_remembered(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    let settings = super::start::isolate_settings();
    fs::write(&settings, "# untouched\n").expect("settings");
    use_settings(cx, &f);
    to_latency_step(cx, &f);
    press(cx, f.window, &["space"]);
    run_calibration(cx, &f, true);
    let remembered = tw::remembered::read(&remembered_store()).expect("remembered");
    assert_eq!(remembered.setup.len(), 1, "{remembered:?}");
    let known = &remembered.setup[0];
    assert_eq!(
        (
            known.setup.input.as_str(),
            known.setup.channel,
            known.setup.output.as_str()
        ),
        (DEVICE, 1, "MacBook Speakers")
    );
    assert!(known.room_tone.file.starts_with("rooms/"), "{known:?}");
    let folder = settings.parent().expect("settings folder");
    assert!(folder.join(&known.room_tone.file).is_file());
    assert_eq!(
        remembered.latency,
        [tw::remembered::RememberedLatency {
            input: DEVICE.to_owned(),
            output: "MacBook Speakers".to_owned(),
            buffer_frames: 512,
            latency_offset_ms: -30,
            date: crate::calibrate::today(),
        }]
    );
    assert_eq!(
        fs::read_to_string(&settings).expect("settings"),
        "# untouched\n"
    );
}

#[gpui_kit::test]
fn the_line_list_scrolls_to_keep_the_current_line_in_view(cx: &mut TestAppContext) {
    let script = (0..60).fold(String::new(), |script, i| {
        let start = f64::from(i) * 0.3;
        format!(
            "{script}[[line]]\nid = \"l{i:02}\"\nstart = {start:.1}\nend = {:.1}\ntext = \"Line {i}.\"\n\n",
            start + 0.2
        )
    });
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        fs::write(dir.join("script.toml"), &script).expect("script");
    });
    let in_view = |cx: &mut TestAppContext, idx: usize| {
        frame(cx, f.window);
        f.view.read_with(cx, |view, _| {
            let list = view.lines_scroll.bounds();
            let row = view.lines_scroll.bounds_for_item(idx).expect("the row");
            let offset = view.lines_scroll.offset().y;
            row.top() + offset >= list.top() && row.bottom() + offset <= list.bottom()
        })
    };
    assert!(in_view(cx, 0));
    assert!(!in_view(cx, 45), "the list is longer than its space");

    press(cx, f.window, &["down"; 45]);
    assert_eq!(f.view.read_with(cx, |view, _| view.line), 45);
    assert!(in_view(cx, 45), "scrolled down to the current line");
    assert!(!in_view(cx, 0));
    // Just enough: the line became the last one in view.
    assert!(!in_view(cx, 46));

    press(cx, f.window, &["up"; 45]);
    assert!(in_view(cx, 0), "and back up");

    // A reload that moves the current line (an agent removed lines above
    // it) follows it too.
    press(cx, f.window, &["down"; 59]);
    assert!(in_view(cx, 59));
    let kept: String = script
        .split("[[line]]")
        .skip(51)
        .fold(String::new(), |all, line| all + "[[line]]" + line);
    fs::write(f.dir.join("script.toml"), kept).expect("script");
    f.view.update(cx, |view, _| view.reload());
    assert_eq!(f.view.read_with(cx, |view, _| view.line), 9);
    assert!(in_view(cx, 9));
}
