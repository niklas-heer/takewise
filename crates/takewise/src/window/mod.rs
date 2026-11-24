//! The recording window (GPUI): the film with the line to read, the line
//! and take lists, and the keys to record, play, choose, mark and edit, a
//! line at a time or a whole passage (with Shift, see `passage`).
//!
//! All writes go through `takewise-core`; the project lock is only held for
//! those short writes, never while recording. The microphone is never
//! monitored: capture feeds the take and the meters only (monitoring waits
//! behind the `Media` seam for its hardware spike). The audio panel (A)
//! picks devices between takes and sets the film and monitor volume. Takes are
//! checked on a worker thread after each commit and whenever a reload finds
//! takes without a current check.

mod checks;
mod clock;
mod latency;
mod lines;
mod media;
mod meter;
mod model;
mod panel;
mod passage;
mod prompter;
mod recorder;
mod remembered;
#[cfg(feature = "screenshots")]
pub mod screenshots;
mod setup;
mod start;
mod takes;
#[cfg(test)]
mod tests;
mod video;
mod watch;
mod writer;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::base::slider::SliderEvent;
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Bounds, Context, Entity, FocusHandle, Global,
    InteractiveElement as _, IntoElement, KeyBinding, Menu, MenuItem, ParentElement as _, Render,
    ScrollHandle, StatefulInteractiveElement as _, Styled as _, Subscription, TitlebarOptions,
    WeakEntity, Window, WindowBounds, WindowOptions, div, px, rgb, size,
};
use std::sync::Arc;

use takewise_core::{
    self as tw, AvailableInput, Field, LineState, Project, ScriptVersion, State, TakeRef,
    Unavailable, Unchecked, WhisperCli, check_input, clamp_mark, plan_take,
};

pub use checks::reap_orphaned_tools;
use checks::{Checker, Checks, worker_tag};
pub use clock::FilmClock;
pub use media::{Media, RealMedia};
use recorder::Recorder;
use watch::Watcher;

gpui_kit::actions!(
    takewise,
    [
        Record,
        RecordPassage,
        Choose,
        ChoosePassage,
        Retake,
        PlayInContext,
        PlayPassage,
        MarkStart,
        Edit,
        PrevLine,
        NextLine,
        PrevTake,
        NextTake,
        CancelEdit,
        TogglePanel,
        ToggleMonitor,
        ClearClip,
        FilmQuieter,
        FilmLouder,
        MonitorQuieter,
        MonitorLouder,
        ToggleSetup,
        MeasureLatency,
        SetupListen,
        SetupNext,
        CloseSetup,
        InstallModel,
        InstallCommand,
        Quit,
    ]
);

/// The key context of the window while no text is being edited.
const CONTEXT: &str = "Recorder";
/// The key context while the line's text is edited (E): the recording keys
/// are off so they type into the field.
const EDITING: &str = "RecorderEditing";
/// The key context while the setup check is open: Space and Enter drive it.
const SETUP: &str = "RecorderSetup";
/// The keys, along the window's bottom edge.
const KEY_HINTS: &str = "Space record/stop · Enter choose · R retake · P play in context · ⇧ + Space/Enter/P: the whole passage · M mark start · E edit · ↑/↓ lines · ←/→ takes · A audio · C setup check · L latency · −/+ film · ,/. monitor · K clear clip";
/// The keys while monitoring is unavailable: the monitor volume keys are
/// not advertised.
const KEY_HINTS_NO_MONITOR: &str = "Space record/stop · Enter choose · R retake · P play in context · ⇧ + Space/Enter/P: the whole passage · M mark start · E edit · ↑/↓ lines · ←/→ takes · A audio · C setup check · L latency · −/+ film · K clear clip";
/// How often the input devices are checked while idle.
const INPUT_CHECK_S: f64 = 2.0;

pub(crate) mod colors {
    pub const BACKGROUND: u32 = 0x0010_1012;
    pub const PANEL: u32 = 0x0018_181b;
    pub const TEXT: u32 = 0x00ee_eeee;
    pub const DIM: u32 = 0x0080_8088;
    pub const FAINT: u32 = 0x0050_5058;
    pub const ACCENT: u32 = 0x00f5_c542;
    pub const SELECTED: u32 = 0x0030_3038;
    pub const RECORD: u32 = 0x00e5_484d;
    pub const GOOD: u32 = 0x004c_c38a;
    pub const WARN: u32 = 0x00f5_a524;
}

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("space", Record, Some(CONTEXT)),
        KeyBinding::new("shift-space", RecordPassage, Some(CONTEXT)),
        KeyBinding::new("enter", Choose, Some(CONTEXT)),
        KeyBinding::new("shift-enter", ChoosePassage, Some(CONTEXT)),
        KeyBinding::new("r", Retake, Some(CONTEXT)),
        KeyBinding::new("p", PlayInContext, Some(CONTEXT)),
        KeyBinding::new("shift-p", PlayPassage, Some(CONTEXT)),
        KeyBinding::new("m", MarkStart, Some(CONTEXT)),
        KeyBinding::new("e", Edit, Some(CONTEXT)),
        KeyBinding::new("up", PrevLine, Some(CONTEXT)),
        KeyBinding::new("down", NextLine, Some(CONTEXT)),
        KeyBinding::new("left", PrevTake, Some(CONTEXT)),
        KeyBinding::new("right", NextTake, Some(CONTEXT)),
        KeyBinding::new("a", TogglePanel, Some(CONTEXT)),
        KeyBinding::new("h", ToggleMonitor, Some(CONTEXT)),
        KeyBinding::new("k", ClearClip, Some(CONTEXT)),
        // Typed characters, so they work on QWERTY (shift-= types +) and
        // QWERTZ (+ has its own key) alike.
        KeyBinding::new("-", FilmQuieter, Some(CONTEXT)),
        KeyBinding::new("+", FilmLouder, Some(CONTEXT)),
        KeyBinding::new(",", MonitorQuieter, Some(CONTEXT)),
        KeyBinding::new(".", MonitorLouder, Some(CONTEXT)),
        KeyBinding::new("c", ToggleSetup, Some(CONTEXT)),
        KeyBinding::new("l", MeasureLatency, Some(CONTEXT)),
        KeyBinding::new("i", InstallModel, Some(CONTEXT)),
        KeyBinding::new("space", SetupListen, Some(SETUP)),
        KeyBinding::new("enter", SetupNext, Some(SETUP)),
        KeyBinding::new("escape", CloseSetup, Some(SETUP)),
        KeyBinding::new("c", ToggleSetup, Some(SETUP)),
        KeyBinding::new("a", TogglePanel, Some(SETUP)),
        KeyBinding::new("escape", CancelEdit, Some(EDITING)),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-w", Quit, None),
    ]);
}

/// The project as the window shows it.
struct Loaded {
    project: Project,
    version: ScriptVersion,
    state: State,
    /// Takes without a check against their line as it is now.
    unchecked: Vec<Unchecked>,
}

fn load(dir: &Path) -> Result<Loaded, tw::Error> {
    let (project, version) = Project::open_versioned(dir)?;
    let ledger = tw::read(dir)?;
    let state = tw::fold(&project, &ledger);
    let unchecked = tw::unchecked_takes(&project, &ledger);
    Ok(Loaded {
        project,
        version,
        state,
        unchecked,
    })
}

/// What a check worker's checks depend on: the model file it loads, the
/// `checker` it records (the model's file name and the preprocessing) and
/// the language.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckKey {
    model: PathBuf,
    checker: String,
    language: String,
}

/// The [`CheckKey`] of `project` with its models in `models_dir` (where
/// [`WhisperCli::discover`] looks for a relative `whisper_model`).
fn check_key(project: &Project, models_dir: &Path) -> CheckKey {
    let config = &project.config.check;
    let model = Path::new(&config.whisper_model).file_name().map_or_else(
        || config.whisper_model.clone(),
        |name| name.to_string_lossy().into_owned(),
    );
    CheckKey {
        model: models_dir.join(&config.whisper_model),
        checker: tw::checker(env!("CARGO_PKG_VERSION"), &model),
        language: config.language.clone(),
    }
}

/// whisper-cli with the project's model and the tools it runs, or, when it
/// cannot be set up, a stand-in that leaves every take's words unchecked
/// with the reason. Kills the tools an earlier Takewise left behind.
fn checker(project: &Project) -> Checker {
    let config = &project.config.check;
    let tag = worker_tag();
    match WhisperCli::discover(config) {
        Ok(mut whisper) => {
            let tools = vec![whisper.whisper.clone(), whisper.ffmpeg.clone()];
            reap_orphaned_tools(&tools);
            whisper.temp_prefix.clone_from(&tag);
            Checker {
                tools,
                transcriber: Arc::new(whisper),
                tag,
            }
        }
        Err(err) => Checker {
            transcriber: Arc::new(Unavailable {
                reason: err.to_string(),
                model: config.whisper_model.clone(),
            }),
            tools: Vec::new(),
            tag,
        },
    }
}

/// Writes whether the window opened to `result` (for `takewise record`,
/// which waits for the app and then reads it); nothing without a path.
fn report_start(result: Option<&Path>, outcome: Result<(), &str>) {
    let Some(path) = result else {
        return;
    };
    let reply = match outcome {
        Ok(()) => serde_json::json!({ "ok": true }),
        Err(error) => serde_json::json!({ "error": error }),
    };
    if let Err(err) = std::fs::write(path, reply.to_string()) {
        eprintln!("error: {}: {err}", path.display());
    }
}

/// Loads the project in `dir` and checks its film is there.
fn prepare(dir: &Path) -> Result<(PathBuf, Loaded), String> {
    // The real path, so a project opened through a symlink or `sub/..` is
    // the same project everywhere, rescue folders' names included.
    let dir = std::fs::canonicalize(dir)
        .or_else(|_| std::path::absolute(dir))
        .map_err(|err| format!("{}: {err}", dir.display()))?;
    let loaded = load(&dir).map_err(|err| err.to_string())?;
    let film = loaded.project.film_path();
    if !film.is_file() {
        return Err(format!("the film {} does not exist", film.display()));
    }
    Ok((dir, loaded))
}

/// Opens the recording window on the project in `dir` and runs until it
/// closes; writes whether it opened to `result`. Runs inside Takewise.app
/// (see `takewise record`).
pub fn run(dir: &Path, result: Option<&Path>) -> Result<(), String> {
    let (dir, loaded) = prepare(dir).inspect_err(|err| report_start(result, Err(err)))?;
    let result = result.map(Path::to_path_buf);
    gpui_kit::application().run(move |cx: &mut App| {
        set_up_app(cx);
        start::remember_opened(&dir);
        if let Err(err) = open_recording(dir, loaded, cx) {
            eprintln!("error: {err}");
            report_start(result.as_deref(), Err(&err));
            cx.quit();
            return;
        }
        report_start(result.as_deref(), Ok(()));
        cx.activate(true);
    });
    Ok(())
}

/// Takewise.app opened without a project: the start window, which opens
/// or creates one. Runs until the last window closes.
pub fn run_start() {
    gpui_kit::application().run(|cx: &mut App| {
        set_up_app(cx);
        if let Err(err) = start::open(cx) {
            eprintln!("error: cannot open the window: {err}");
            cx.quit();
            return;
        }
        cx.activate(true);
    });
}

/// The keys, the app menu and quitting once the last window closed.
fn set_up_app(cx: &mut App) {
    gpui_kit::init(cx);
    bind_keys(cx);
    start::bind_keys(cx);
    cx.on_action(|_: &Quit, cx: &mut App| {
        if !holds_unsaved_audio(cx) {
            cx.quit();
        }
    });
    cx.set_menus([Menu::new("Takewise").items([
        MenuItem::action("Open Project…", start::OpenProject),
        MenuItem::action("Install Command Line Tool…", InstallCommand),
        MenuItem::separator(),
        MenuItem::action("Quit Takewise", Quit),
    ])]);
    // From the menu with no window to show the outcome in, it is said in
    // a new start window.
    cx.on_action(|_: &InstallCommand, cx: &mut App| {
        let _ = start::open_with_message(cx, crate::cli_install::install());
    });
    cx.on_action(|_: &start::OpenProject, cx: &mut App| {
        let _ = start::open(cx);
    });
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
}

/// The recording windows open in this app, by project folder.
#[derive(Default)]
struct OpenProjects(Vec<(PathBuf, WeakEntity<RecordingWindow>, AnyWindowHandle)>);

impl Global for OpenProjects {}

/// The folder as compared between windows: resolved, so two paths to one
/// project match.
fn folder_key(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir)
        .or_else(|_| std::path::absolute(dir))
        .unwrap_or_else(|_| dir.to_path_buf())
}

/// Brings the recording window open on `dir` to the front, if there is
/// one; returns whether there was.
fn focus_open(dir: &Path, cx: &mut App) -> bool {
    let key = folder_key(dir);
    let open = cx.try_global::<OpenProjects>().and_then(|open| {
        open.0
            .iter()
            .find(|(folder, view, _)| *folder == key && view.upgrade().is_some())
            .map(|(_, _, window)| *window)
    });
    open.is_some_and(|window| {
        window
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    })
}

/// Quitting (⌘Q, ⌘W, the menu): whether a recording window still holds
/// audio only in memory, so the app must not quit. Each window first stops
/// its take and tries once more to save; one that still holds audio says
/// why it stays open and comes to the front.
fn holds_unsaved_audio(cx: &mut App) -> bool {
    let windows: Vec<_> = cx
        .try_global::<OpenProjects>()
        .map(|open| {
            open.0
                .iter()
                .filter_map(|(_, view, window)| Some((view.upgrade()?, *window)))
                .collect()
        })
        .unwrap_or_default();
    let mut holds = false;
    for (view, window) in windows {
        let keeps = view.update(cx, |view, cx| {
            cx.notify();
            view.keeps_unsaved_audio()
        });
        if keeps {
            let _ = window.update(cx, |_, window, _| window.activate_window());
        }
        holds |= keeps;
    }
    holds
}

/// Shows `message` in the recording window open on `dir`, if there is one.
fn tell_open(dir: &Path, message: String, cx: &mut App) {
    let key = folder_key(dir);
    let view = cx.try_global::<OpenProjects>().and_then(|open| {
        open.0
            .iter()
            .find(|(folder, _, _)| *folder == key)
            .and_then(|(_, view, _)| view.upgrade())
    });
    if let Some(view) = view {
        view.update(cx, |view, cx| {
            view.message = Some(message);
            cx.notify();
        });
    }
}

/// Opens a recording window on the loaded project in `dir`, inside the
/// running app.
fn open_recording(dir: PathBuf, loaded: Loaded, cx: &mut App) -> Result<(), String> {
    let media = RealMedia::open(&loaded.project.film_path())?;
    let title = format!(
        "Takewise — {}",
        dir.file_name().map_or_else(
            || dir.display().to_string(),
            |name| name.to_string_lossy().into_owned()
        )
    );
    let watcher = Watcher::new(&dir).ok();
    let checker = checker(&loaded.project);
    let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(title.into()),
            ..TitlebarOptions::default()
        }),
        ..WindowOptions::default()
    };
    gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| {
            let mut view =
                RecordingWindow::new(dir, loaded, Box::new(media), watcher, checker, window, cx);
            view.use_settings(start::settings_path());
            view.offers_model = true;
            view.model = model::prompt_for(&view.project.config.check, &view.models_dir);
            view
        })
    })
    .map(drop)
    .map_err(|err| format!("cannot open the window: {err}"))
}

/// A line's text being edited (E): the line and the script version it was
/// begun against stay fixed for the draft, whatever reloads happen.
struct Draft {
    input: Entity<InputState>,
    line: String,
    version: ScriptVersion,
}

pub struct RecordingWindow {
    dir: PathBuf,
    media: Box<dyn Media>,
    project: Project,
    version: ScriptVersion,
    state: State,
    /// Index of the current line in the script.
    line: usize,
    /// The selected take of the current line.
    take: Option<u32>,
    recorder: Recorder,
    message: Option<String>,
    /// Why recording is not possible: the input device, or script problems.
    input_problem: Option<String>,
    script_problem: Option<String>,
    inputs_checked_at: Option<f64>,
    editing: Option<Draft>,
    watcher: Option<Watcher>,
    checks: Checks,
    /// The `[check]` settings and preprocessing the check worker was made
    /// for ([`check_key`]); when they change, a new worker replaces it.
    checks_for: CheckKey,
    /// Makes the check worker's checker for the project as it is now.
    make_checker: Box<dyn Fn(&Project) -> Checker>,
    focus: FocusHandle,
    subscriptions: Vec<Subscription>,
    /// The connected inputs as last checked.
    inputs: Vec<AvailableInput>,
    panel: panel::Panel,
    /// The film and monitor volume in dB as set now, and when they last
    /// changed while not yet written to `project.toml`.
    film_gain_db: f64,
    monitor_db: f64,
    volumes_changed_at: Option<f64>,
    peak_meter: meter::PeakMeter,
    /// Peaks of takes committed in this session, for the "clipped" mark
    /// until their check says.
    take_peaks: HashMap<(String, u32), f64>,
    /// Where device choices are saved as defaults for new projects; none in
    /// tests.
    defaults_path: Option<PathBuf>,
    /// How far the current line is scrolled, and its text's layout from
    /// the last frame (with the text it laid out).
    scroll: prompter::Scroll,
    text_layout: Option<(String, gpui_kit::TextLayout)>,
    /// Which line of a passage take is being read (from 0), as last seen:
    /// when it moves on, the prompter starts the next line at the top and
    /// the current line follows it.
    reading: Option<usize>,
    /// The line list's scroll position, and the current line it was last
    /// scrolled to show.
    lines_scroll: ScrollHandle,
    lines_shown: Option<usize>,
    /// The setup check while it is open (C).
    setup: Option<setup::Setup>,
    /// Why the setup check should run (none ran, or the devices changed),
    /// shown as an offer.
    setup_offer: Option<String>,
    /// The offer to install the speech model while it is missing; none in
    /// tests unless a test sets one.
    model: Option<model::ModelPrompt>,
    /// Where a relative `[check] whisper_model` is found.
    models_dir: PathBuf,
    /// Whether the window offers to install a missing model (not in tests
    /// unless a test says so).
    offers_model: bool,
    /// The output the film was last set to play through; `None` is the
    /// system default (none chosen, or the chosen one could not be used).
    film_output: Option<String>,
    /// The latency calibration while the setup check runs it, and how its
    /// beep film is made.
    calibrating: Option<latency::Calibrating>,
    beep_film: latency::BeepFilm,
    /// The remembered setup, `remembered.json` beside the settings; none in
    /// tests unless a test sets one.
    remembered_path: Option<PathBuf>,
    /// The route in use and the local day the remembered setup was last
    /// looked up for.
    remembered_for: Option<(takewise_core::Route, jiff::civil::Date)>,
    /// Where audio goes that cannot be saved into the project when the
    /// window closes (`rescue/` beside the settings, see
    /// `takewise_core::rescue`); none in tests unless a test sets one.
    rescue_root: Option<PathBuf>,
    /// The takes and passages that were only in memory when closing was
    /// last refused: asked again about the same audio, the window writes
    /// its rescue copy and closes. Audio that came since is refused for
    /// once first, so its narrator hears of it.
    close_refused: Option<(usize, usize)>,
}

impl RecordingWindow {
    fn new(
        dir: PathBuf,
        loaded: Loaded,
        media: Box<dyn Media>,
        watcher: Option<Watcher>,
        checker: Checker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let on_quit = Self::keep_audio_when_closing(window, cx);
        let handle = window.window_handle();
        let me = cx.weak_entity();
        let key = folder_key(&dir);
        let open = cx.default_global::<OpenProjects>();
        open.0.retain(|(_, view, _)| view.upgrade().is_some());
        open.0.push((key, me, handle));
        let checks = Checks::new(dir.clone(), checker);
        let models_dir = tw::whisper::models_dir();
        let checks_for = check_key(&loaded.project, &models_dir);
        let loaded_unchecked = loaded.unchecked;
        let film_gain_db = loaded.project.config.record.film_gain_db;
        let monitor_db = loaded.project.config.monitor.volume_db;
        let panel = panel::Panel::new(film_gain_db, monitor_db, cx);
        let mut subscriptions = vec![on_quit];
        for (slider, film) in [(&panel.film_slider, true), (&panel.monitor_slider, false)] {
            subscriptions.push(cx.subscribe_in(
                slider,
                window,
                move |view: &mut Self, _, event: &SliderEvent, _, cx| {
                    let (SliderEvent::Change(value) | SliderEvent::Release(value)) = event;
                    let db = f64::from(value.end());
                    if film {
                        view.set_volumes(db, view.monitor_db);
                    } else {
                        view.set_volumes(view.film_gain_db, db);
                    }
                    cx.notify();
                },
            ));
        }
        let mut view = Self {
            dir,
            media,
            project: loaded.project,
            version: loaded.version,
            state: loaded.state,
            line: 0,
            take: None,
            recorder: Recorder::new(),
            message: None,
            input_problem: None,
            script_problem: None,
            inputs_checked_at: None,
            editing: None,
            watcher,
            checks,
            checks_for,
            make_checker: Box::new(self::checker),
            focus,
            subscriptions,
            inputs: Vec::new(),
            panel,
            film_gain_db,
            monitor_db,
            volumes_changed_at: None,
            peak_meter: meter::PeakMeter::default(),
            take_peaks: HashMap::new(),
            defaults_path: None,
            scroll: prompter::Scroll::default(),
            text_layout: None,
            reading: None,
            lines_scroll: ScrollHandle::new(),
            lines_shown: None,
            setup: None,
            setup_offer: None,
            model: None,
            models_dir,
            offers_model: false,
            film_output: None,
            calibrating: None,
            beep_film: crate::calibrate::make_beep_film,
            remembered_path: None,
            remembered_for: None,
            rescue_root: None,
            close_refused: None,
        };
        view.after_reload(None);
        view.queue_checks(&loaded_unchecked);
        view.recover();
        view.check_inputs();
        view.apply_output();
        view.park();
        view
    }

    /// Closing the window or quitting mid-take still keeps the take; the
    /// window stays open while audio exists only in memory, until it has a
    /// rescue copy. Quitting from outside (the Dock, a logout) cannot be
    /// refused: GPUI hears of it once it is underway, so `shutdown` then
    /// saves or rescues what it can. Returns the quit handler's
    /// subscription, to be kept.
    fn keep_audio_when_closing(window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        let this = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            this.update(cx, |view, cx| {
                cx.notify();
                if view.keeps_unsaved_audio() {
                    return false;
                }
                view.shutdown();
                true
            })
            .unwrap_or(true)
        });
        cx.on_app_quit(|view, _| {
            view.shutdown();
            async {}
        })
    }

    /// Adopts takes left by a recording that ended without a commit (a
    /// crash), and finishes and cuts a passage it left, as `takewise status
    /// --repair` would.
    fn recover(&mut self) {
        match tw::repair(&self.dir) {
            Ok(report)
                if !report.adopted.is_empty()
                    || !report.pieces.is_empty()
                    || !report.passages_published.is_empty() =>
            {
                self.reload();
                self.message = Some(passage::recovered_note(&report));
            }
            Ok(_) => {}
            Err(err) => {
                self.message = Some(format!("Could not look for interrupted takes: {err}"));
            }
        }
    }

    /// Before the window closes or the app quits: stops a take in progress
    /// and tries once more to save what waits. Returns whether audio is
    /// still only in memory, which closing would lose; the window then
    /// stays open, says why, and keeps retrying.
    ///
    /// Asked again while that audio still cannot be saved, it writes a
    /// rescue copy (see `takewise_core::rescue`) and lets the window
    /// close: a project whose disk is gone for good must not leave Force
    /// Quit, which loses the audio, as the only way out. Audio that has
    /// its rescue copy never holds the window.
    fn keeps_unsaved_audio(&mut self) -> bool {
        if self.recorder.is_recording() {
            self.finish_take();
        }
        if self.recorder.has_unsaved() {
            let update = self.recorder.save_unsaved_now(&self.dir);
            self.show(update);
        }
        if !self.recorder.needs_rescue() {
            self.close_refused = None;
            return false;
        }
        let (takes, passages) = self.recorder.in_memory_only();
        let asked_before = self.close_refused.replace((takes, passages)) == Some((takes, passages));
        let rescue_failed = if asked_before && self.rescue_root.is_some() {
            match self.rescue_unsaved() {
                Ok(()) => {
                    self.close_refused = None;
                    return false;
                }
                Err(failed) => Some(failed),
            }
        } else {
            None
        };
        self.message = Some(not_closing_note(
            takes,
            passages,
            self.recorder.save_error(),
            match rescue_failed.as_deref() {
                Some(failed) => RescueOffer::Failed(failed),
                None => self
                    .rescue_root
                    .as_deref()
                    .map_or(RescueOffer::None, RescueOffer::Into),
            },
        ));
        true
    }

    /// Writes a rescue copy of the audio that is only in memory and has
    /// none yet, and says where it is: also of the copies that were
    /// written when another could not be.
    fn rescue_unsaved(&mut self) -> Result<(), Box<recorder::RescueFailed>> {
        let root = self.rescue_root.clone().ok_or_else(|| {
            Box::new(recorder::RescueFailed::new(
                &"there is no folder for rescue copies",
            ))
        })?;
        #[cfg(test)]
        start::isolated::assert_temporary(&root);
        let at = jiff::Timestamp::now();
        let outcome = self.recorder.rescue(&root, &self.dir, at);
        let rescued = match &outcome {
            Ok(rescued) => rescued.as_ref(),
            Err(failed) => failed.rescued.as_ref(),
        };
        if let Some(rescued) = rescued {
            let note = rescued_note(rescued);
            // The window may be gone before anyone reads its message.
            eprintln!("takewise: {note}");
            self.message = Some(note);
        }
        outcome.map(drop)
    }

    /// When the window or the app is going for good: audio that is still
    /// only in memory gets its rescue copy, whether or not closing was
    /// asked about before (a quit from the Dock or a logout is not). What
    /// did not get one is said, and where its audio is when only the JSON
    /// file beside it could not be written.
    fn rescue_at_exit(&mut self) {
        if !self.recorder.needs_rescue() {
            return;
        }
        if let Err(failed) = self.rescue_unsaved() {
            let note = not_rescued_note(self.recorder.without_rescue_copy(), &failed);
            eprintln!("takewise: {note}");
            // After what was said of the copies that were written.
            self.message = recorder::join(failed.rescued.and(self.message.take()), Some(note));
        }
    }

    /// Says when this project has audio in the rescue folder, from a
    /// window that could not save it.
    fn note_rescued(&mut self) {
        let Some(root) = &self.rescue_root else {
            return;
        };
        let notes: Vec<String> = tw::rescue::notices(root, &self.dir)
            .into_iter()
            .map(|notice| format!("Rescued audio: {}.", notice.message))
            .collect();
        if !notes.is_empty() {
            // Whatever else opening had to say, this must not go unsaid.
            self.message = recorder::join(self.message.take(), Some(notes.join(" ")));
        }
    }

    /// Ends whatever runs: commits a take in progress (if it has audio),
    /// stops playback and the meter, and cancels the checks (their tools are
    /// killed; the takes are checked when the window opens again). Audio
    /// that still cannot be saved into the project gets a rescue copy.
    fn shutdown(&mut self) {
        self.calibrating = None;
        if self.recorder.is_recording() {
            self.finish_take();
        }
        if self.recorder.has_unsaved() {
            let update = self.recorder.save_unsaved_now(&self.dir);
            self.show(update);
            // A passage being cut into its takes is waited for.
            for update in self.recorder.finish_cuts_now(&self.dir) {
                self.show(update);
            }
        }
        self.rescue_at_exit();
        self.recorder.stop_playback(&mut *self.media);
        self.recorder.keep_meter(&mut *self.media, None);
        self.media.stop_channel_meter();
        self.save_volumes(true);
        self.checks.cancel();
        if let Some(prompt) = &self.model {
            prompt.cancel();
        }
    }

    fn line_id(&self) -> Option<&str> {
        self.project
            .script
            .lines
            .get(self.line)
            .map(|line| line.id.as_str())
    }

    fn line_state(&self) -> Option<&LineState> {
        let id = self.line_id()?;
        self.state.lines.iter().find(|line| line.id == id)
    }

    /// The line the prompter shows: while a take records, the line as the
    /// take started with it, whatever reloads change or select meanwhile
    /// (they reach the prompter once the take ends); else the current line.
    fn prompter_line(&self) -> Option<prompter::Reading<'_>> {
        if let Some(snapshot) = self.recorder.recording_snapshot() {
            return Some(prompter::Reading {
                text: &snapshot.text,
                start: snapshot.start,
                end: snapshot.end,
            });
        }
        let line = self.line_state()?;
        Some(prompter::Reading {
            text: &line.text,
            start: line.start,
            end: line.end,
        })
    }

    /// The note and the next line's text the prompter shows: while a take
    /// records, those the take started with, like its text.
    fn prompter_cues(&self) -> (Option<&str>, Option<&str>) {
        if let Some(cues) = self.recorder.recording_cues() {
            return cues;
        }
        let lines = &self.project.script.lines;
        (
            lines.get(self.line).and_then(|line| line.note.as_deref()),
            lines.get(self.line + 1).map(|line| line.text.as_str()),
        )
    }

    /// The row above the prompter's current line for the line just read:
    /// kept during a passage take, holding that line for a moment after
    /// the switch.
    fn prompter_passed(&self) -> prompter::Passed<'_> {
        if !self.recorder.is_recording_passage() {
            return prompter::Passed::Off;
        }
        self.recorder
            .passed_line(&*self.media)
            .map_or(prompter::Passed::Empty, prompter::Passed::Line)
    }

    #[cfg(test)]
    fn prompter_text(&self) -> Option<&str> {
        self.prompter_line().map(|reading| reading.text)
    }

    /// The key hints: the monitor keys only while monitoring can run.
    fn key_hints(&self) -> &'static str {
        if self.media.monitor_unavailable().is_some() {
            KEY_HINTS_NO_MONITOR
        } else {
            KEY_HINTS
        }
    }

    fn blocked(&self) -> Option<&str> {
        self.script_problem
            .as_deref()
            .or(self.input_problem.as_deref())
    }

    /// Re-reads the project, keeping the current line (by id) and take.
    fn reload(&mut self) {
        let current = self.line_id().map(str::to_owned);
        match load(&self.dir) {
            Ok(loaded) => {
                self.project = loaded.project;
                self.version = loaded.version;
                self.state = loaded.state;
                self.refresh_checks();
                self.sync_volumes();
                self.after_reload(current.as_deref());
                self.queue_checks(&loaded.unchecked);
            }
            Err(err) => self.message = Some(format!("Could not reload the project: {err}")),
        }
    }

    /// Replaces the check worker when `[check]` (the model or the language)
    /// or the preprocessing changed since it was made, so no check runs
    /// with stale settings; dropping the old one cancels its check. The
    /// takes its checks no longer match are queued again by the reload.
    /// A changed model also changes the install banner to the new one's.
    fn refresh_checks(&mut self) {
        let key = check_key(&self.project, &self.models_dir);
        if key == self.checks_for {
            return;
        }
        if key.model != self.checks_for.model && self.offers_model {
            if let Some(old) = self.model.take() {
                old.cancel();
            }
            self.model = model::prompt_for(&self.project.config.check, &self.models_dir);
        }
        self.replace_checks((self.make_checker)(&self.project));
    }

    /// Starts a new check worker with `checker` in place of the old one.
    fn replace_checks(&mut self, checker: Checker) {
        self.checks = Checks::new(self.dir.clone(), checker);
        self.checks_for = check_key(&self.project, &self.models_dir);
        self.pause_checks_while_recording();
    }

    /// Queues a check of each take without a current one (a new take, or a
    /// line whose text or slot changed).
    fn queue_checks(&mut self, unchecked: &[Unchecked]) {
        for u in unchecked {
            if let Some(snapshot) = self.project.snapshot(&u.take.line) {
                self.checks.queue(&u.take, &snapshot);
            }
        }
    }

    /// Starts installing the speech model (I), unless a take records.
    fn install_model(&mut self) {
        if self.recorder.is_recording() {
            return;
        }
        if let Some(prompt) = &mut self.model {
            prompt.start();
        }
    }

    /// The model is in place: checks now hear words, and the takes whose
    /// words went unchecked are checked again.
    fn model_installed(&mut self) {
        let Some(prompt) = self.model.take() else {
            return;
        };
        // The old worker checks without words; dropping it cancels it.
        self.replace_checks(prompt.installer().checker(&self.project));
        self.reload();
        let again = match tw::read(&self.dir) {
            Ok(ledger) => tw::words_not_checked_takes(&self.project, &ledger),
            Err(_) => Vec::new(),
        };
        for take in &again {
            if let Some(snapshot) = self.project.snapshot(&take.line) {
                self.checks.queue(take, &snapshot);
            }
        }
        self.message = Some(if again.is_empty() {
            "Installed the speech model; takes are now checked with words.".to_owned()
        } else {
            format!(
                "Installed the speech model; checking the words of {} take(s).",
                again.len()
            )
        });
    }

    /// Takes in finished checks: reloads to show their verdicts, and says
    /// which takes could not be checked at all.
    fn collect_checks(&mut self) {
        let done = self.checks.poll();
        if done.is_empty() {
            return;
        }
        self.reload();
        let failed: Vec<String> = done
            .iter()
            .filter_map(|d| {
                let err = d.result.as_ref().err()?;
                Some(format!("{} take {}: {err}", d.take.line, d.take.take))
            })
            .collect();
        if !failed.is_empty() {
            self.message = Some(format!("Could not check {}.", failed.join("; ")));
        }
    }

    fn after_reload(&mut self, keep_line: Option<&str>) {
        let count = self.project.script.lines.len();
        self.line = keep_line
            .and_then(|id| self.project.script.position(id))
            .unwrap_or(self.line)
            .min(count.saturating_sub(1));
        let problems = tw::validate(&self.project.script, self.project.film_duration());
        self.script_problem = if count == 0 {
            Some("The script has no lines yet; add them to script.toml.".to_owned())
        } else if problems.is_empty() {
            None
        } else {
            let list: Vec<String> = problems
                .iter()
                .map(|p| match &p.line {
                    Some(line) => format!("{line}: {}", p.message),
                    None => p.message.clone(),
                })
                .collect();
            Some(format!(
                "Fix script.toml before recording: {}",
                list.join("; ")
            ))
        };
        self.fix_take();
    }

    /// Keeps the selected take if the line still has it, else selects the
    /// chosen, then the proposed, then the newest take.
    fn fix_take(&mut self) {
        let Some(line) = self.line_state() else {
            self.take = None;
            return;
        };
        let has = |n: u32| line.takes.iter().any(|t| t.take == n);
        self.take = self
            .take
            .filter(|&n| has(n))
            .or(line.chosen.as_ref().map(|(n, _)| *n).filter(|&n| has(n)))
            .or(line.proposed)
            .or(line.takes.last().map(|t| t.take));
    }

    fn check_inputs(&mut self) {
        let inputs: Vec<AvailableInput> = self.media.inputs();
        let configured = self.project.config.input.device.clone();
        self.input_problem = tw::mic_advice(self.media.microphone_permission())
            .or_else(|| check_input(configured.as_deref(), &inputs).err());
        self.note_issue(panel::input_issue(configured.as_deref(), &inputs));
        self.inputs = inputs;
        self.use_remembered();
        let route = self.current_route();
        self.panel.calibration = panel::calibration_note(&self.project.config, &route);
        self.panel.remembered = self.remembered_note(&route);
        self.update_setup_offer();
        self.inputs_checked_at = Some(self.media.host_now_s());
    }

    /// Rests the film where the current line's take would start.
    fn park(&mut self) {
        let Some(line) = self.project.script.lines.get(self.line) else {
            return;
        };
        let rec = &self.project.config.record;
        let end = self
            .project
            .script
            .slot_end(self.line, self.project.film_duration());
        let plan = plan_take(line.start, end, rec.preroll_s, rec.tail_s);
        self.recorder.park(&mut *self.media, plan.film_from_s);
    }

    fn start_take(&mut self) {
        self.begin_take(false);
    }

    /// Starts a take of the current line, or with `whole_passage` of the
    /// lines from it through the end of its passage.
    fn begin_take(&mut self, whole_passage: bool) {
        self.check_inputs();
        if self.blocked().is_some() {
            return;
        }
        self.message = None;
        self.peak_meter.clear();
        self.scroll.reset();
        self.panel.picker = None;
        let route = self.current_route();
        if let Err(err) = self.recorder.start_take(
            &mut *self.media,
            &self.project,
            self.line,
            &route,
            whole_passage,
        ) {
            self.message = Some(err);
        }
        self.reading = self.recorder.passage_reading().map(|(index, _)| index);
        self.pause_checks_while_recording();
    }

    fn finish_take(&mut self) {
        let update = self.recorder.finish_take(&mut *self.media, &self.dir);
        self.show(update);
        self.pause_checks_while_recording();
    }

    /// Holds the checks back while a take records; they resume once it is
    /// committed.
    fn pause_checks_while_recording(&self) {
        self.checks.set_paused(self.recorder.is_recording());
    }

    /// Selects a committed take; returns the message saying so. When its
    /// line was removed or renamed meanwhile, the nearest line stays
    /// selected and the film rests there.
    fn took(&mut self, committed: Option<TakeRef>) -> Option<String> {
        let taken = committed?;
        self.reload();
        if self.line_id() == Some(taken.line.as_str()) {
            self.take = Some(taken.take);
        } else if self.project.script.position(&taken.line).is_none() {
            self.park();
            return Some(format!(
                "Recorded {} take {}: the line you recorded was removed or renamed; the take was saved.",
                taken.line, taken.take
            ));
        }
        Some(format!("Recorded {} take {}.", taken.line, taken.take))
    }

    /// Once per frame: reload on outside changes, keep the device check and
    /// meter current, and advance the recorder.
    fn tick(&mut self) {
        if self.watcher.as_ref().is_some_and(Watcher::changed) {
            self.reload();
        }
        self.collect_checks();
        if self.model.as_mut().is_some_and(model::ModelPrompt::poll) {
            self.model_installed();
        }
        let busy = self.recorder.is_recording() || self.recorder.is_playing();
        let now = self.media.host_now_s();
        if !busy
            && self
                .inputs_checked_at
                .is_none_or(|at| now - at >= INPUT_CHECK_S)
        {
            self.check_inputs();
        }
        let input = &self.project.config.input;
        let meter = match (&input.device, self.input_problem.is_none()) {
            (Some(device), true) => Some((device.clone(), input.channel)),
            _ => None,
        };
        self.recorder.keep_meter(
            &mut *self.media,
            meter
                .as_ref()
                .map(|(device, channel)| (device.as_str(), *channel)),
        );
        let update = self.recorder.tick(&mut *self.media, &self.dir);
        self.show(update);
        self.follow_reading();
        self.pause_checks_while_recording();
        self.tick_setup(now);
        self.tick_panel(now);
        self.save_volumes(false);
        let level = self.recorder.level_dbfs(&*self.media);
        self.peak_meter.feed(
            level,
            self.recorder.clip_peak_dbfs(),
            self.media.host_now_s(),
        );
    }

    /// How many rows the prompter scrolls the current line now: toward the
    /// row of the pace fill's current word, read from the last frame's
    /// layout of the same text; never back during a take.
    fn scroll_rows(&mut self, film_s: f64, now: f64) -> f32 {
        let recording = self.recorder.is_recording();
        let target = match (self.prompter_line(), &self.text_layout) {
            (Some(reading), Some((text, layout))) if text == reading.text => {
                let filled = prompter::filled_bytes(reading, film_s);
                prompter::current_row(layout, reading.text, filled).map_or(0.0, |(row, rows)| {
                    prompter::scroll_offset(row, rows, prompter::VISIBLE_ROWS)
                })
            }
            _ => 0.0,
        };
        self.scroll.aim(target, now, recording);
        self.scroll.at(now)
    }

    /// The lines and the current line's takes. The line list scrolls just
    /// enough to show the current line whenever that changed: with the
    /// arrows, with a passage take moving on, or with a reload.
    fn side_column(&mut self) -> gpui_kit::AnyElement {
        if self.lines_shown != Some(self.line) {
            self.lines_shown = Some(self.line);
            self.lines_scroll.scroll_to_item(self.line);
        }
        div()
            .w(px(300.))
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(colors::PANEL))
            .child(lines::lines(
                &self.state,
                self.line,
                self.recorder.recording_line(),
                &self.lines_scroll,
            ))
            .child(takes::takes(
                self.line_state(),
                self.take,
                &self
                    .line_id()
                    .map(|id| self.checks.pending_of(id))
                    .unwrap_or_default(),
                &self.clipped_takes(),
            ))
            .into_any_element()
    }

    /// The current line's takes marked "clipped": by their latest check,
    /// or before it by the peak recorded in this session.
    fn clipped_takes(&self) -> Vec<u32> {
        let Some(line) = self.line_state() else {
            return Vec::new();
        };
        line.takes
            .iter()
            .filter(|take| {
                take.clipped.unwrap_or_else(|| {
                    self.take_peaks
                        .get(&(line.id.clone(), take.take))
                        .is_some_and(|peak| *peak >= meter::CLIPPED_DBFS)
                })
            })
            .map(|take| take.take)
            .collect()
    }

    /// Shows what the recorder reports and selects a committed take. An
    /// interruption or error is shown together with the success, never
    /// replaced by it.
    fn show(&mut self, update: recorder::Update) {
        if let (Some(take), Some(peak)) = (&update.committed, update.peak_dbfs) {
            self.take_peaks.insert((take.line.clone(), take.take), peak);
        }
        let success = self.took(update.committed);
        let passage = update.passage.map(|passage| self.took_passage(&passage));
        let success = recorder::join(success, passage);
        if let Some(message) = recorder::join(success, update.message) {
            self.message = Some(message);
        }
    }

    fn record(&mut self, _: &Record, _: &mut Window, cx: &mut Context<Self>) {
        if self.recorder.is_recording() {
            self.finish_take();
        } else if self.recorder.is_playing() {
            self.recorder.stop_playback(&mut *self.media);
        } else {
            self.start_take();
        }
        cx.notify();
    }

    fn retake(&mut self, _: &Retake, _: &mut Window, cx: &mut Context<Self>) {
        if self.recorder.is_recording() {
            self.finish_take();
        }
        self.recorder.stop_playback(&mut *self.media);
        self.start_take();
        cx.notify();
    }

    fn choose(&mut self, _: &Choose, _: &mut Window, cx: &mut Context<Self>) {
        if self.recorder.is_recording() {
            return;
        }
        let (Some(line), Some(take)) = (self.line_id().map(str::to_owned), self.take) else {
            return;
        };
        match tw::choose_take(&self.dir, &line, take, "narrator") {
            Ok(()) => {
                self.reload();
                self.message = Some(format!("Chose {line} take {take}."));
            }
            Err(err) => self.message = Some(format!("Cannot choose the take: {err}")),
        }
        cx.notify();
    }

    fn play(&mut self, _: &PlayInContext, _: &mut Window, cx: &mut Context<Self>) {
        if self.recorder.is_recording() {
            return;
        }
        if self.recorder.is_playing() {
            self.recorder.stop_playback(&mut *self.media);
            cx.notify();
            return;
        }
        let take = self.line_state().and_then(|line| {
            line.takes
                .iter()
                .find(|t| Some(t.take) == self.take)
                .cloned()
        });
        let Some(take) = take else {
            self.message = Some("This line has no take to play yet.".to_owned());
            cx.notify();
            return;
        };
        self.message = None;
        if let Err(err) =
            self.recorder
                .start_playback(&mut *self.media, &self.project, self.line, &take)
        {
            self.message = Some(err);
        }
        cx.notify();
    }

    fn mark(&mut self, _: &MarkStart, _: &mut Window, cx: &mut Context<Self>) {
        cx.notify();
        if self.recorder.is_recording_passage() {
            self.message = Some(
                "M is off during a passage take: its lines keep the slots it started with."
                    .to_owned(),
            );
            return;
        }
        let Some(playhead) = self.recorder.playhead_s(&*self.media) else {
            self.message = Some(
                "M sets the line's start while the film plays: record (Space) or play in context (P)."
                    .to_owned(),
            );
            return;
        };
        let Some(id) = self.line_id().map(str::to_owned) else {
            return;
        };
        let script = &self.project.script;
        let duration = self.project.film_duration();
        let at = clamp_mark(script, self.line, playhead, duration);
        let start = clamp_mark(script, self.line, (at * 1000.0).round() / 1000.0, duration);
        let version = self.version;
        let what = format!("start {start:.3} s");
        self.write_field(&id, Field::Start(start), &what, &version, "Try again.");
    }

    /// Writes one field of a line against `version`; a file changed on disk
    /// since is reloaded and the edit refused, with `again` saying what to
    /// do. Returns whether it was written.
    fn write_field(
        &mut self,
        id: &str,
        field: Field,
        what: &str,
        version: &ScriptVersion,
        again: &str,
    ) -> bool {
        match tw::set_line_field(&self.dir, id, field, Some(version)) {
            Ok(_) => {
                self.reload();
                self.message = Some(format!("Saved {id}: {what}."));
                true
            }
            Err(tw::Error::ScriptChanged) => {
                self.reload();
                self.message = Some(format!(
                    "script.toml changed on disk, so {id}'s {what} was not saved; \
                     the window reloaded the script. {again}"
                ));
                false
            }
            Err(err) => {
                self.message = Some(format!("Not saved: {err}"));
                false
            }
        }
    }

    fn edit(&mut self, _: &Edit, window: &mut Window, cx: &mut Context<Self>) {
        if self.recorder.is_recording() || self.recorder.is_playing() {
            return;
        }
        let Some(line) = self.project.script.lines.get(self.line) else {
            return;
        };
        let text = line.text.clone();
        let id = line.id.clone();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(text));
        let subscription = cx.subscribe_in(&input, window, |view, _, event, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                view.save_edit(window, cx);
            }
        });
        self.subscriptions.push(subscription);
        input.update(cx, |input, cx| input.focus(window, cx));
        self.editing = Some(Draft {
            input,
            line: id,
            version: self.version,
        });
        cx.notify();
    }

    /// Saves the draft to the line it was begun on, against the version it
    /// was begun with: a script changed since refuses it, and a line removed
    /// since is never replaced by another.
    fn save_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = &self.editing else {
            return;
        };
        let text = draft.input.read(cx).value().trim().to_owned();
        let (id, version) = (draft.line.clone(), draft.version);
        if self.project.script.position(&id).is_none() {
            self.message = Some(format!(
                "{id} is no longer in the script, so the edit was not saved; press Esc."
            ));
        } else if text.is_empty() {
            self.message = Some("A line's text cannot be empty.".to_owned());
        } else if self.write_field(
            &id,
            Field::Text(text),
            "text",
            &version,
            "Press Esc, then E to edit the new version.",
        ) {
            self.close_editor(window, cx);
        }
        cx.notify();
    }

    fn cancel_edit(&mut self, _: &CancelEdit, window: &mut Window, cx: &mut Context<Self>) {
        self.close_editor(window, cx);
    }

    fn close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn move_line(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.recorder.is_recording() {
            return;
        }
        let count = self.project.script.lines.len();
        let Some(next) = self.line.checked_add_signed(delta).filter(|&i| i < count) else {
            return;
        };
        self.recorder.stop_playback(&mut *self.media);
        self.line = next;
        self.scroll.reset();
        self.take = None;
        self.fix_take();
        self.park();
        cx.notify();
    }

    fn move_take(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.recorder.is_recording() {
            return;
        }
        let Some(line) = self.line_state() else {
            return;
        };
        let numbers: Vec<u32> = line.takes.iter().map(|t| t.take).collect();
        let at = numbers
            .iter()
            .position(|&n| Some(n) == self.take)
            .unwrap_or(0);
        if let Some(&n) = at.checked_add_signed(delta).and_then(|i| numbers.get(i)) {
            self.recorder.stop_playback(&mut *self.media);
            self.take = Some(n);
        }
        cx.notify();
    }
}

impl RecordingWindow {
    /// The offer to run the setup check, while it is closed.
    fn render_setup_offer(&self) -> Option<gpui_kit::AnyElement> {
        self.setup_offer
            .as_ref()
            .filter(|_| self.setup.is_none())
            .map(|why| {
                div()
                    .px(px(16.))
                    .pt(px(8.))
                    .text_size(px(13.))
                    .text_color(rgb(colors::WARN))
                    .child(format!("Setup: {why}. Press C for the setup check."))
                    .into_any_element()
            })
    }

    /// The speech model banner: the offer with an Install button, then a
    /// progress bar while it downloads.
    fn render_model_banner(&self, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        let prompt = self.model.as_ref()?;
        // 0 to 1, so the cast loses nothing visible.
        #[allow(clippy::cast_possible_truncation)]
        let fraction = prompt.fraction().map(|f| f.clamp(0.0, 1.0) as f32);
        let button = (!prompt.installing()).then(|| {
            div()
                .id("install-model")
                .px(px(10.))
                .py(px(3.))
                .rounded(px(4.))
                .bg(rgb(colors::SELECTED))
                .text_color(rgb(colors::TEXT))
                .cursor_pointer()
                .child("Install")
                .on_click(cx.listener(|view, _, _, cx| {
                    view.install_model();
                    cx.notify();
                }))
        });
        let bar = fraction.map(|f| {
            div()
                .w(px(240.))
                .h(px(6.))
                .rounded(px(3.))
                .bg(rgb(colors::SELECTED))
                .child(
                    div()
                        .h_full()
                        .rounded(px(3.))
                        .bg(rgb(colors::ACCENT))
                        .w(px(240. * f)),
                )
        });
        Some(
            div()
                .px(px(16.))
                .pt(px(8.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.))
                .text_size(px(13.))
                .text_color(rgb(colors::WARN))
                .child(prompt.text())
                .children(button)
                .children(bar)
                .into_any_element(),
        )
    }
}

/// "1 take and 2 passages".
fn counted(takes: usize, passages: usize) -> String {
    let count = |n: usize, one: &str, many: &str| match n {
        0 => None,
        1 => Some(format!("1 {one}")),
        n => Some(format!("{n} {many}")),
    };
    [
        count(takes, "take", "takes"),
        count(passages, "passage", "passages"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" and ")
}

/// What a window that does not close says about the rescue copy.
#[derive(Clone, Copy)]
enum RescueOffer<'a> {
    /// There is no folder for one.
    None,
    /// Closing again writes one into this folder.
    Into(&'a Path),
    /// Writing one failed; some audio may have been written without its
    /// JSON file.
    Failed(&'a recorder::RescueFailed),
}

/// Why the window does not close: `takes` and `passages` exist only in
/// memory, and saving them failed with `error`.
fn not_closing_note(
    takes: usize,
    passages: usize,
    error: Option<&str>,
    rescue: RescueOffer<'_>,
) -> String {
    let what = counted(takes, passages);
    let is = if takes + passages == 1 { "is" } else { "are" };
    let why = error.map(|err| format!(" ({err})")).unwrap_or_default();
    let retrying = "Takewise keeps trying to save every 2 s; free disk space or fix the problem, then close the window again.";
    match rescue {
        RescueOffer::None => {
            format!("Not closing: {what} {is} only in memory, not saved yet{why}. {retrying}")
        }
        RescueOffer::Into(folder) => format!(
            "Not closing: {what} {is} only in memory, not saved yet{why}. {retrying} If it cannot be fixed, close again now: Takewise then writes a rescue copy to {} and closes.",
            folder.display()
        ),
        RescueOffer::Failed(failed) => {
            let kept = failed
                .audio_only
                .as_ref()
                .map(|kept| {
                    let files = if kept.takes + kept.passages == 1 {
                        "a WAV file without its JSON file; keep it"
                    } else {
                        "WAV files without their JSON files; keep them"
                    };
                    format!(
                        " The audio of {} is in {} as {files}.",
                        counted(kept.takes, kept.passages),
                        kept.folder.display()
                    )
                })
                .unwrap_or_default();
            format!(
                "Not closing: {what} {is} only in memory, not saved yet{why}, and the rescue copy could not be written either ({}).{kept} {retrying}",
                failed.error
            )
        }
    }
}

/// Where the rescue copies of audio that could not be saved are.
fn rescued_note(rescued: &recorder::Rescued) -> String {
    let what = counted(rescued.takes, rescued.passages);
    let (was, copy) = if rescued.takes + rescued.passages == 1 {
        ("was", "a rescue copy is")
    } else {
        ("were", "rescue copies are")
    };
    format!(
        "{what} could not be saved into the project and {was} only in memory: {copy} in {}. `takewise status` says how to bring the audio back.",
        rescued.folder.display()
    )
}

/// What the window says as it goes about the audio that got no rescue
/// copy: `without` takes and passages, of which `failed.audio_only` are in
/// the rescue folder as WAVs without their JSON file, and the rest are
/// nowhere but in the memory that is about to go.
fn not_rescued_note(without: (usize, usize), failed: &recorder::RescueFailed) -> String {
    let (kept_takes, kept_passages) = failed
        .audio_only
        .as_ref()
        .map_or((0, 0), |kept| (kept.takes, kept.passages));
    let kept = failed.audio_only.as_ref().map(|kept| {
        let (copy, files, keep) = if kept_takes + kept_passages == 1 {
            ("its rescue copy is", "a WAV file", "that file")
        } else {
            ("their rescue copies are", "WAV files", "those files")
        };
        format!(
            "{} could not be saved into the project, and {copy} incomplete: the audio is in {} as {files}, but the JSON that says where it belongs could not be written ({}). Keep {keep}.",
            counted(kept_takes, kept_passages),
            kept.folder.display(),
            kept.error
        )
    });
    let (takes, passages) = (
        without.0.saturating_sub(kept_takes),
        without.1.saturating_sub(kept_passages),
    );
    let lost = (takes + passages > 0).then(|| {
        format!(
            "{} could be neither saved nor rescued and {} lost: {}",
            counted(takes, passages),
            if takes + passages == 1 { "is" } else { "are" },
            failed.error
        )
    });
    recorder::join(kept, lost).unwrap_or_else(|| failed.error.clone())
}

/// The audio keys: the panel (A), monitoring (H), the clipping light (K),
/// the setup check (C) and its latency step alone (L), and the film and
/// monitor volume.
fn audio_actions(
    root: gpui_kit::Stateful<gpui_kit::Div>,
    cx: &mut Context<RecordingWindow>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    root.on_action(cx.listener(|view, _: &TogglePanel, _, cx| {
        view.panel.open = !view.panel.open;
        view.panel.picker = None;
        cx.notify();
    }))
    .on_action(cx.listener(|view, _: &ToggleMonitor, _, cx| {
        view.toggle_monitor();
        cx.notify();
    }))
    .on_action(cx.listener(|view, _: &ToggleSetup, _, cx| {
        view.toggle_setup();
        cx.notify();
    }))
    .on_action(cx.listener(|view, _: &MeasureLatency, _, cx| {
        view.measure_latency();
        cx.notify();
    }))
    .on_action(cx.listener(|view, _: &SetupListen, _, cx| {
        view.setup_listen();
        cx.notify();
    }))
    .on_action(cx.listener(|view, _: &SetupNext, _, cx| {
        view.setup_next();
        cx.notify();
    }))
    .on_action(cx.listener(|view, _: &CloseSetup, _, cx| {
        view.close_setup();
        cx.notify();
    }))
    .on_action(cx.listener(|view, _: &ClearClip, _, cx| {
        view.peak_meter.clear();
        cx.notify();
    }))
    .on_action(cx.listener(|view, _: &FilmQuieter, window, cx| {
        view.step_volume(-panel::STEP_DB, 0.0, window, cx);
    }))
    .on_action(cx.listener(|view, _: &FilmLouder, window, cx| {
        view.step_volume(panel::STEP_DB, 0.0, window, cx);
    }))
    .on_action(cx.listener(|view, _: &MonitorQuieter, window, cx| {
        view.step_volume(0.0, -panel::STEP_DB, window, cx);
    }))
    .on_action(cx.listener(|view, _: &MonitorLouder, window, cx| {
        view.step_volume(0.0, panel::STEP_DB, window, cx);
    }))
}

/// The keys for lines and takes: record, choose and play (with Shift the
/// whole passage), mark, edit and the arrows.
fn take_actions(
    root: gpui_kit::Stateful<gpui_kit::Div>,
    cx: &mut Context<RecordingWindow>,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    root.on_action(cx.listener(RecordingWindow::record))
        .on_action(cx.listener(RecordingWindow::record_passage))
        .on_action(cx.listener(RecordingWindow::retake))
        .on_action(cx.listener(RecordingWindow::choose))
        .on_action(cx.listener(RecordingWindow::choose_passage))
        .on_action(cx.listener(RecordingWindow::play))
        .on_action(cx.listener(RecordingWindow::play_passage))
        .on_action(cx.listener(RecordingWindow::mark))
        .on_action(cx.listener(RecordingWindow::edit))
        .on_action(cx.listener(RecordingWindow::cancel_edit))
        .on_action(cx.listener(|view, _: &PrevLine, _, cx| view.move_line(-1, cx)))
        .on_action(cx.listener(|view, _: &NextLine, _, cx| view.move_line(1, cx)))
        .on_action(cx.listener(|view, _: &PrevTake, _, cx| view.move_take(-1, cx)))
        .on_action(cx.listener(|view, _: &NextTake, _, cx| view.move_take(1, cx)))
}

impl Render for RecordingWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tick();
        window.request_animation_frame();

        let media = &*self.media;
        let film_s = self.recorder.film_now_s(media);
        let count_in = self.recorder.count_in(media);
        let level = self.recorder.level_dbfs(media);
        let hold = self.peak_meter.hold_dbfs(media.host_now_s());
        let recording = self.recorder.is_recording();
        let frame = self.media.latest_frame();

        let now = self.media.host_now_s();
        let scroll_rows = self.scroll_rows(film_s, now);
        let (note, next) = self.prompter_cues();
        let prompter = prompter::Prompter {
            film_s,
            reading: self.prompter_line(),
            note,
            next,
            passed: self.prompter_passed(),
            level_dbfs: level,
            hold_dbfs: hold,
            near_clip: self.peak_meter.near_clip(),
            recording,
            blocked: self.blocked(),
            message: self.message.as_deref(),
            editor: self.editing.as_ref().map(|draft| &draft.input),
            scroll_rows,
        };
        let text = prompter.reading.map(|reading| reading.text.to_owned());
        let (prompter, layout) = prompter.render();
        self.text_layout = text.zip(layout);
        let panel_view = self.panel.open.then(|| self.render_panel(cx));
        let setup_view = self.render_setup();
        let key_context = if self.editing.is_some() {
            EDITING
        } else if self.setup.is_some() {
            SETUP
        } else {
            CONTEXT
        };
        let offer = self.render_setup_offer();
        let model_view = self.render_model_banner(cx);
        let side_column = self.side_column();

        let root = div()
            .id("recording-window")
            .key_context(key_context)
            .track_focus(&self.focus);
        take_actions(audio_actions(root, cx), cx)
            .on_action(cx.listener(|view, _: &InstallModel, _, cx| {
                view.install_model();
                cx.notify();
            }))
            .on_action(cx.listener(|view, _: &InstallCommand, _, cx| {
                view.message = Some(crate::cli_install::install());
                cx.notify();
            }))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(colors::BACKGROUND))
            .text_color(rgb(colors::TEXT))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_row()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(video::video(frame, count_in, recording))
                            .child(prompter),
                    )
                    .children(panel_view)
                    .children(setup_view)
                    .child(side_column),
            )
            .children(model_view)
            .children(offer)
            .child(
                div()
                    .px(px(16.))
                    .py(px(8.))
                    .text_size(px(13.))
                    .text_color(rgb(colors::DIM))
                    .child(self.key_hints()),
            )
    }
}

#[cfg(test)]
mod note_tests {
    use std::path::PathBuf;

    use super::recorder::{AudioOnly, RescueFailed};
    use super::{RescueOffer, not_closing_note};

    #[test]
    fn a_refusal_says_where_audio_without_its_json_file_is() {
        let mut failed = RescueFailed::new(&"disk full");
        failed.audio_only = Some(AudioOnly {
            folder: PathBuf::from("/rescue/film.takewise-20261124T101530Z"),
            takes: 1,
            passages: 0,
            error: "disk full".to_owned(),
        });
        let one = not_closing_note(1, 0, None, RescueOffer::Failed(&failed));
        assert!(
            one.contains("as a WAV file without its JSON file; keep it."),
            "{one}"
        );
        let note = not_closing_note(2, 0, Some("disk gone"), RescueOffer::Failed(&failed));
        assert!(
            note.contains("could not be written either (disk full)")
                && note.contains("/rescue/film.takewise-20261124T101530Z as a WAV file"),
            "{note}"
        );
    }
}
