//! The window in a headless GPUI test window, driven by key presses, with
//! the media replaced by a fake whose host clock the test sets.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Entity, TestAppContext, VisualTestContext,
    Window, WindowBounds, WindowOptions, point, px, size,
};
use takewise_core::{
    self as tw, AvailableInput, EventKind, Field, FilmInfo, Project, RecordedInput, Timebase,
};
use takewise_media::{CaptureBuffer, DeviceInfo, Frame, OutputSource, Transport};

use super::checks::{Checker, Checks, SharedTranscriber, worker_tag};
use super::clock::FilmClock;
use super::{Media, RecordingWindow, bind_keys, load};

const DEVICE: &str = "Test Mic";
const LATENCY_S: f64 = 0.05;

/// The fake's state, shared between the window and the test.
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
    /// Where the film plays (`None`: the system default).
    film_output: Option<String>,
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
            film_output: None,
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
        true
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
        Timebase {
            rate: if fake.now >= host_s { 1.0 } else { 0.0 },
            film_anchor_s: film_s,
            host_anchor_s: host_s,
        }
    }
    fn film_open(&mut self, path: &Path) -> Result<(), String> {
        self.0.borrow_mut().films.push(path.to_owned());
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
        self.0.borrow().outputs.clone()
    }
    fn default_output(&self) -> Option<String> {
        Some("MacBook Speakers".to_owned())
    }
    fn film_set_output(&mut self, device: &str) -> Result<(), String> {
        let mut fake = self.0.borrow_mut();
        if !fake.outputs.iter().any(|output| output.name == device) {
            return Err(format!("no output device named {device:?}"));
        }
        fake.film_output = Some(device.to_owned());
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
    let text = message(cx, &missing.view);
    assert!(
        text.contains("Gone Phones is not connected") && text.contains("MacBook Speakers"),
        "{text}"
    );
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
            .read_with(cx, |view, _| (view.film_gain_db, view.monitor_db))
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

    // Calibrated on the chosen output, which is gone: the film falls back
    // to the speakers, so the offset is not applied.
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        tw::set_route(dir, None, Some("Gone Phones")).expect("route");
        calibrate_on(dir, "Gone Phones");
    });
    let offset = line_offset_of_a_take(cx, &f);
    assert!(close(offset, -2.4), "{offset}");
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
