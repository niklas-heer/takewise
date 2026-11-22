//! The window in a headless GPUI test window, driven by key presses, with
//! the media replaced by a fake whose host clock the test sets.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::Sender;

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Entity, TestAppContext, VisualTestContext,
    Window, WindowBounds, WindowOptions, point, px, size,
};
use takewise_core::{
    self as tw, AvailableInput, EventKind, Field, FilmInfo, Project, RecordedInput, Timebase,
};
use takewise_media::{CaptureBuffer, Frame};

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
        })))
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

impl Media for FakeMedia {
    fn host_now_s(&self) -> f64 {
        self.0.borrow().now
    }
    fn film_ready(&self) -> bool {
        true
    }
    fn film_play_at(&mut self, film_s: f64, host_s: f64) -> Result<Timebase, String> {
        self.0.borrow_mut().plays.push((film_s, host_s));
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
    fn film_pause(&mut self) {}
    fn film_seek(&mut self, _: f64) {}
    fn film_set_gain_db(&mut self, _: f64) {}
    fn film_set_muted(&mut self, _: bool) {}
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
        _: u16,
        sink: Sender<CaptureBuffer>,
    ) -> Result<(), String> {
        let mut fake = self.0.borrow_mut();
        if !fake.inputs.iter().any(|input| input.name == device) {
            return Err(format!("no input device named {device:?}"));
        }
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
    fs::write(&config, text).expect("project.toml");
    (tmp, dir)
}

fn open(cx: &mut TestAppContext, inputs: &[(&str, u32)]) -> Fixture {
    open_with(cx, inputs, |_| {})
}

/// Opens the window after `prepare` ran on the new project.
fn open_with(
    cx: &mut TestAppContext,
    inputs: &[(&str, u32)],
    prepare: impl FnOnce(&Path),
) -> Fixture {
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
                RecordingWindow::new(window_dir, loaded, Box::new(window_media), None, window, cx)
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
