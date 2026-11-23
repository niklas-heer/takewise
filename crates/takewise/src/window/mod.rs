//! The recording window (GPUI): the film with the line to read, the line
//! and take lists, and the keys to record, play, choose, mark and edit.
//!
//! All writes go through `takewise-core`; the project lock is only held for
//! those short writes, never while recording. The microphone is never
//! monitored: capture feeds the take and the level meter only.

mod clock;
mod lines;
mod media;
mod prompter;
mod recorder;
mod takes;
#[cfg(test)]
mod tests;
mod video;
mod watch;
mod writer;

use std::path::{Path, PathBuf};

use gpui_kit::base::input::{InputEvent, InputState};
use gpui_kit::{
    App, AppContext as _, Bounds, Context, Entity, FocusHandle, InteractiveElement as _,
    IntoElement, KeyBinding, Menu, MenuItem, ParentElement as _, Render, Styled as _, Subscription,
    TitlebarOptions, Window, WindowBounds, WindowOptions, div, px, rgb, size,
};
use takewise_core::{
    self as tw, AvailableInput, Field, LineState, Project, ScriptVersion, State, TakeRef,
    check_input, clamp_mark, plan_take,
};

pub use clock::FilmClock;
pub use media::{Media, RealMedia};
use recorder::Recorder;
use watch::Watcher;

gpui_kit::actions!(
    takewise,
    [
        Record,
        Choose,
        Retake,
        PlayInContext,
        MarkStart,
        Edit,
        PrevLine,
        NextLine,
        PrevTake,
        NextTake,
        CancelEdit,
        Quit,
    ]
);

/// The key context of the window while no text is being edited.
const CONTEXT: &str = "Recorder";
/// The key context while the line's text is edited (E): the recording keys
/// are off so they type into the field.
const EDITING: &str = "RecorderEditing";
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
        KeyBinding::new("enter", Choose, Some(CONTEXT)),
        KeyBinding::new("r", Retake, Some(CONTEXT)),
        KeyBinding::new("p", PlayInContext, Some(CONTEXT)),
        KeyBinding::new("m", MarkStart, Some(CONTEXT)),
        KeyBinding::new("e", Edit, Some(CONTEXT)),
        KeyBinding::new("up", PrevLine, Some(CONTEXT)),
        KeyBinding::new("down", NextLine, Some(CONTEXT)),
        KeyBinding::new("left", PrevTake, Some(CONTEXT)),
        KeyBinding::new("right", NextTake, Some(CONTEXT)),
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
}

fn load(dir: &Path) -> Result<Loaded, tw::Error> {
    let (project, version) = Project::open_versioned(dir)?;
    let ledger = tw::read(dir)?;
    let state = tw::fold(&project, &ledger);
    Ok(Loaded {
        project,
        version,
        state,
    })
}

/// Opens the recording window on the project in `dir` and runs until it
/// closes. Runs inside Takewise.app (see `takewise record`).
pub fn run(dir: &Path) -> Result<(), String> {
    let dir = std::path::absolute(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let loaded = load(&dir).map_err(|err| err.to_string())?;
    let film = loaded.project.film_path();
    if !film.is_file() {
        return Err(format!("the film {} does not exist", film.display()));
    }
    let title = format!(
        "Takewise — {}",
        dir.file_name().map_or_else(
            || dir.display().to_string(),
            |name| name.to_string_lossy().into_owned()
        )
    );
    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        bind_keys(cx);
        cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
        cx.set_menus([Menu::new("Takewise").items([MenuItem::action("Quit Takewise", Quit)])]);
        let media = match RealMedia::open(&film) {
            Ok(media) => media,
            Err(err) => {
                eprintln!("error: {err}");
                cx.quit();
                return;
            }
        };
        let watcher = Watcher::new(&dir).ok();
        let bounds = Bounds::centered(None, size(px(1280.), px(820.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                ..TitlebarOptions::default()
            }),
            ..WindowOptions::default()
        };
        let opened = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| RecordingWindow::new(dir, loaded, Box::new(media), watcher, window, cx))
        });
        if let Err(err) = opened {
            eprintln!("error: cannot open the window: {err}");
            cx.quit();
            return;
        }
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.activate(true);
    });
    Ok(())
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
    focus: FocusHandle,
    subscriptions: Vec<Subscription>,
}

impl RecordingWindow {
    fn new(
        dir: PathBuf,
        loaded: Loaded,
        media: Box<dyn Media>,
        watcher: Option<Watcher>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        // Closing the window or quitting mid-take still keeps the take.
        let this = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            let _ = this.update(cx, |view, _| view.shutdown());
            true
        });
        let on_quit = cx.on_app_quit(|view, _| {
            view.shutdown();
            async {}
        });
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
            focus,
            subscriptions: vec![on_quit],
        };
        view.after_reload(None);
        view.recover();
        view.check_inputs();
        view.park();
        view
    }

    /// Adopts takes left by a recording that ended without a commit (a
    /// crash), as `takewise status --repair` would.
    fn recover(&mut self) {
        match tw::repair(&self.dir) {
            Ok(report) if !report.adopted.is_empty() => {
                let takes: Vec<String> = report
                    .adopted
                    .iter()
                    .map(|t| format!("{} take {}", t.line, t.take))
                    .collect();
                self.reload();
                self.message = Some(format!(
                    "Recovered {} from an interrupted recording.",
                    takes.join(", ")
                ));
            }
            Ok(_) => {}
            Err(err) => {
                self.message = Some(format!("Could not look for interrupted takes: {err}"));
            }
        }
    }

    /// Ends whatever runs: commits a take in progress (if it has audio),
    /// stops playback and the meter.
    fn shutdown(&mut self) {
        if self.recorder.is_recording() {
            self.finish_take();
        }
        if self.recorder.has_unsaved() {
            let update = self.recorder.save_unsaved_now(&self.dir);
            self.show(update);
        }
        self.recorder.stop_playback(&mut *self.media);
        self.recorder.keep_meter(&mut *self.media, None);
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

    /// The current line's text as the prompter shows it: while a take of
    /// the line records, the text the take started with (a reload of the
    /// script reaches the prompter once the take ends).
    fn prompter_text(&self) -> Option<&str> {
        let line = self.line_state()?;
        let recording = self
            .recorder
            .recording_text()
            .filter(|_| self.recorder.recording_line() == Some(line.id.as_str()));
        Some(recording.unwrap_or(&line.text))
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
                self.after_reload(current.as_deref());
            }
            Err(err) => self.message = Some(format!("Could not reload the project: {err}")),
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
        self.input_problem =
            check_input(self.project.config.input.device.as_deref(), &inputs).err();
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
        self.check_inputs();
        if self.blocked().is_some() {
            return;
        }
        self.message = None;
        if let Err(err) = self
            .recorder
            .start_take(&mut *self.media, &self.project, self.line)
        {
            self.message = Some(err);
        }
    }

    fn finish_take(&mut self) {
        let update = self.recorder.finish_take(&mut *self.media, &self.dir);
        self.show(update);
    }

    /// Selects a committed take; returns the message saying so.
    fn took(&mut self, committed: Option<TakeRef>) -> Option<String> {
        let taken = committed?;
        self.reload();
        if self.line_id() == Some(taken.line.as_str()) {
            self.take = Some(taken.take);
        }
        Some(format!("Recorded {} take {}.", taken.line, taken.take))
    }

    /// Once per frame: reload on outside changes, keep the device check and
    /// meter current, and advance the recorder.
    fn tick(&mut self) {
        if self.watcher.as_ref().is_some_and(Watcher::changed) {
            self.reload();
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
    }

    /// Shows what the recorder reports and selects a committed take. An
    /// interruption or error is shown together with the success, never
    /// replaced by it.
    fn show(&mut self, update: recorder::Update) {
        let success = self.took(update.committed);
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

impl Render for RecordingWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tick();
        window.request_animation_frame();

        let media = &*self.media;
        let film_s = self.recorder.film_now_s(media);
        let count_in = self.recorder.count_in(media);
        let level = self.recorder.level_dbfs(media);
        let recording = self.recorder.is_recording();
        let frame = self.media.latest_frame();

        let script = &self.project.script;
        let line = script.lines.get(self.line);
        let prompter = prompter::Prompter {
            film_s,
            line: self.line_state(),
            text: self.prompter_text(),
            note: line.and_then(|line| line.note.as_deref()),
            next: script
                .lines
                .get(self.line + 1)
                .map(|line| line.text.as_str()),
            level_dbfs: level,
            recording,
            blocked: self.blocked(),
            message: self.message.as_deref(),
            editor: self.editing.as_ref().map(|draft| &draft.input),
        };
        let key_context = if self.editing.is_some() {
            EDITING
        } else {
            CONTEXT
        };

        div()
            .id("recording-window")
            .key_context(key_context)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::record))
            .on_action(cx.listener(Self::retake))
            .on_action(cx.listener(Self::choose))
            .on_action(cx.listener(Self::play))
            .on_action(cx.listener(Self::mark))
            .on_action(cx.listener(Self::edit))
            .on_action(cx.listener(Self::cancel_edit))
            .on_action(cx.listener(|view, _: &PrevLine, _, cx| view.move_line(-1, cx)))
            .on_action(cx.listener(|view, _: &NextLine, _, cx| view.move_line(1, cx)))
            .on_action(cx.listener(|view, _: &PrevTake, _, cx| view.move_take(-1, cx)))
            .on_action(cx.listener(|view, _: &NextTake, _, cx| view.move_take(1, cx)))
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
                            .child(prompter.render()),
                    )
                    .child(
                        div()
                            .w(px(300.))
                            .h_full()
                            .flex()
                            .flex_col()
                            .bg(rgb(colors::PANEL))
                            .child(lines::lines(&self.state, self.line, self.recorder.recording_line()))
                            .child(takes::takes(self.line_state(), self.take)),
                    ),
            )
            .child(
                div()
                    .px(px(16.))
                    .py(px(8.))
                    .text_size(px(13.))
                    .text_color(rgb(colors::DIM))
                    .child(
                        "Space record/stop · Enter choose · R retake · P play in context · M mark start · E edit · ↑/↓ lines · ←/→ takes",
                    ),
            )
    }
}
