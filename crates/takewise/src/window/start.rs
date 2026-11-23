//! The start window: what Takewise.app shows when it is opened without a
//! project (from the Dock or Finder). It opens a project folder, starts a
//! new one from a finished film, lists the projects opened recently, and
//! installs the command line tool. A project already open is brought to
//! the front instead of opening twice, and a film whose project was
//! started before opens that project; a film whose usual project folder
//! belongs to another film of the same name gets its own.
//!
//! The recent projects are a convenience kept beside the app's settings
//! (`recent.json` next to `settings.toml`); failing to read or write them
//! is quiet.

use std::path::{Path, PathBuf};

use gpui_kit::{
    App, AppContext as _, Bounds, Context, FocusHandle, InteractiveElement as _, IntoElement,
    KeyBinding, ParentElement as _, PathPromptOptions, Render, StatefulInteractiveElement as _,
    Styled as _, TitlebarOptions, Window, WindowBounds, WindowOptions, div, px, rgb, size,
};
use takewise_core as tw;

use super::{InstallCommand, colors, focus_open, open_recording, prepare, tell_open};

gpui_kit::actions!(takewise_start, [OpenProject]);

/// How many recent projects are kept.
pub const MAX_RECENT: usize = 8;
/// The key context of the start window.
const CONTEXT: &str = "Start";

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("cmd-o", OpenProject, None)]);
}

/// What a folder chosen with "Open project…" is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Picked {
    /// A project: open it.
    Project(PathBuf),
    /// Not a project yet: ask for the film and start one.
    New(PathBuf),
}

pub fn classify(folder: &Path) -> Picked {
    if folder.join(tw::project::PROJECT_FILE).is_file() {
        Picked::Project(folder.to_owned())
    } else {
        Picked::New(folder.to_owned())
    }
}

/// Where a new project for `film` goes when `folder` was chosen: the folder
/// itself when it is named `….takewise` or is empty, else a new
/// `<film name>.takewise` inside it (never the film's own folder's files).
pub fn new_project_dir(folder: &Path, film: &Path) -> PathBuf {
    let named = folder.extension().is_some_and(|ext| ext == "takewise");
    let empty = std::fs::read_dir(folder).is_ok_and(|mut entries| entries.next().is_none());
    if named || empty {
        return folder.to_owned();
    }
    let stem = film
        .file_stem()
        .map_or_else(|| "narration".into(), |stem| stem.to_string_lossy());
    folder.join(format!("{stem}.takewise"))
}

/// What starting a project for `film` in `folder` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Open the project already started for this film.
    Open(PathBuf),
    /// Start a project in `dir`; `note` says why it is not the usual
    /// `<film name>.takewise`.
    Start { dir: PathBuf, note: Option<String> },
    /// Neither project folder is free for this film.
    Taken(String),
}

/// Whose project folder `dir` is.
enum Owner {
    /// It is no project.
    Free,
    /// A project whose `project.toml` cannot be read.
    Unreadable,
    /// The project of this film.
    Film(PathBuf),
}

fn owner(dir: &Path) -> Owner {
    if !matches!(classify(dir), Picked::Project(_)) {
        return Owner::Free;
    }
    match tw::project::read_config(dir) {
        Ok(config) if config.film.is_absolute() => Owner::Film(config.film),
        Ok(config) => Owner::Film(dir.join(config.film)),
        Err(_) => Owner::Unreadable,
    }
}

/// Whether the project in `dir` belongs to `film` (one it cannot read is
/// taken as its own, so opening it says what is wrong).
fn belongs_to(dir: &Path, film: &Path) -> bool {
    match owner(dir) {
        Owner::Free => false,
        Owner::Unreadable => true,
        Owner::Film(own) => resolved(&own) == resolved(film),
    }
}

/// What starting a project for `film` in `folder` does: open the project
/// at [`new_project_dir`] when it was started for this film, or start one
/// there. When that folder is another film's project (`Promo.mp4` beside
/// `Promo.mov`), the project is `<film file name>.takewise` instead
/// (`Promo.mov.takewise`).
pub fn start_target(folder: &Path, film: &Path) -> Target {
    let dir = new_project_dir(folder, film);
    if matches!(owner(&dir), Owner::Free) {
        return Target::Start { dir, note: None };
    }
    if belongs_to(&dir, film) {
        return Target::Open(dir);
    }
    let name = |path: &Path| {
        path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    };
    let film_name = name(film);
    let other = match owner(&dir) {
        Owner::Film(other) => name(&other),
        Owner::Free | Owner::Unreadable => String::new(),
    };
    let parent = if dir == folder {
        folder.parent().unwrap_or(folder)
    } else {
        folder
    };
    let own = parent.join(format!("{film_name}.takewise"));
    if matches!(owner(&own), Owner::Free) {
        let note = format!(
            "{} is the project of {other}, so {film_name} got its own project, {}.",
            name(&dir),
            name(&own)
        );
        return Target::Start {
            dir: own,
            note: Some(note),
        };
    }
    if belongs_to(&own, film) {
        return Target::Open(own);
    }
    Target::Taken(format!(
        "{} and {} are projects of other films; open a folder of its own to start one for {film_name}.",
        name(&dir),
        name(&own)
    ))
}

/// The settings file the app reads and writes: [`tw::settings_path`]; in
/// tests only the one [`isolate_settings`] made for the test's thread, so a
/// test never touches the narrator's own settings or recent projects.
pub fn settings_path() -> Option<PathBuf> {
    #[cfg(test)]
    {
        isolated::current()
    }
    #[cfg(not(test))]
    {
        tw::settings_path()
    }
}

/// The recent projects file: `recent.json` beside the settings file.
pub fn recent_path() -> Option<PathBuf> {
    settings_path()?.parent().map(|dir| dir.join("recent.json"))
}

/// A project folder as stored in the recent projects: resolved (no `..`,
/// no symlinks) when it exists, so one project is listed once.
fn resolved(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir)
        .or_else(|_| std::path::absolute(dir))
        .unwrap_or_else(|_| dir.to_path_buf())
}

/// The recent projects in `path`, most recent first; folders that are no
/// longer projects are left out.
pub fn load_recent(path: &Path) -> Vec<PathBuf> {
    read_recent(path)
        .into_iter()
        .filter(|dir| matches!(classify(dir), Picked::Project(_)))
        .collect()
}

/// The stored recent projects, resolved and each once.
fn read_recent(path: &Path) -> Vec<PathBuf> {
    let stored: Vec<PathBuf> = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let mut recent: Vec<PathBuf> = Vec::with_capacity(stored.len());
    for dir in stored.iter().map(|dir| resolved(dir)) {
        if !recent.contains(&dir) {
            recent.push(dir);
        }
    }
    recent
}

/// Puts `dir` first in the recent projects in `path`, keeping
/// [`MAX_RECENT`].
pub fn remember(path: &Path, dir: &Path) -> std::io::Result<()> {
    #[cfg(test)]
    isolated::assert_temporary(path);
    let dir = resolved(dir);
    let mut recent = read_recent(path);
    recent.retain(|known| *known != dir);
    recent.insert(0, dir);
    recent.truncate(MAX_RECENT);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_vec_pretty(&recent).map_err(std::io::Error::other)?;
    tw::write_atomic(path, &text).map_err(std::io::Error::other)
}

/// Remembers a project the app opened, quietly.
pub fn remember_opened(dir: &Path) {
    #[cfg(test)]
    assert!(
        isolated::current().is_some(),
        "a test remembered a project without isolate_settings()"
    );
    if let Some(path) = recent_path() {
        let _ = remember(&path, dir);
    }
}

/// Makes this test thread's settings and recent projects live in a new
/// temporary folder; returns the settings file there.
#[cfg(test)]
pub fn isolate_settings() -> PathBuf {
    isolated::make()
}

/// The per-thread settings folder of tests.
#[cfg(test)]
pub(crate) mod isolated {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    thread_local! {
        static DIR: RefCell<Option<tempfile::TempDir>> = const { RefCell::new(None) };
    }

    pub fn make() -> PathBuf {
        DIR.with_borrow_mut(|dir| {
            let dir = dir.get_or_insert_with(|| tempfile::tempdir().expect("settings dir"));
            dir.path().join("settings.toml")
        })
    }

    pub fn current() -> Option<PathBuf> {
        DIR.with_borrow(|dir| dir.as_ref().map(|dir| dir.path().join("settings.toml")))
    }

    /// Panics unless `path` is in the temporary folder: tests never write
    /// the narrator's own settings.
    pub fn assert_temporary(path: &Path) {
        let tmp = std::env::temp_dir();
        let real = std::fs::canonicalize(&tmp).unwrap_or_else(|_| tmp.clone());
        assert!(
            path.starts_with(&tmp) || path.starts_with(&real),
            "a test wrote settings outside the temporary folder (use isolate_settings()): {}",
            path.display()
        );
    }
}

pub struct StartWindow {
    focus: FocusHandle,
    recent: Vec<PathBuf>,
    pub message: Option<String>,
}

/// Opens a start window.
pub fn open(cx: &mut App) -> Result<(), String> {
    open_window(None, cx)
}

/// Opens a start window that shows `message`.
pub fn open_with_message(cx: &mut App, message: String) -> Result<(), String> {
    open_window(Some(message), cx)
}

fn open_window(message: Option<String>, cx: &mut App) -> Result<(), String> {
    let recent = recent_path().map(|p| load_recent(&p)).unwrap_or_default();
    let bounds = Bounds::centered(None, size(px(560.), px(520.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some("Takewise".into()),
            ..TitlebarOptions::default()
        }),
        ..WindowOptions::default()
    };
    gpui_kit::open_window(options, cx, |window, cx| {
        cx.new(|cx| {
            let mut view = StartWindow::new(recent, window, cx);
            view.message = message;
            view
        })
    })
    .map(drop)
    .map_err(|err| err.to_string())
}

impl StartWindow {
    pub fn new(recent: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            focus,
            recent,
            message: None,
        }
    }

    /// "Open project…": a folder picker; a project opens, any other folder
    /// asks for the film to start one.
    fn choose_folder(window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(folder) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |view, window, cx| view.picked(&folder, window, cx));
        })
        .detach();
    }

    fn picked(&mut self, folder: &Path, window: &mut Window, cx: &mut Context<Self>) {
        match classify(folder) {
            Picked::Project(dir) => self.open_project(&dir, window, cx),
            Picked::New(folder) => self.choose_film(folder, window, cx),
        }
    }

    /// A folder that is no project yet: the film picker, then a new
    /// project for that film.
    fn choose_film(&mut self, folder: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.message = Some(format!(
            "{} is not a Takewise project yet: choose the finished film to start one.",
            folder.display()
        ));
        cx.notify();
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose Film".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = chosen.await else {
                return;
            };
            let Some(film) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update_in(cx, |view, window, cx| {
                view.start_project(&folder, &film, window, cx);
            });
        })
        .detach();
    }

    /// Starts a project for `film` as `takewise init` would, then opens it;
    /// opens the one already started for it there instead.
    fn start_project(
        &mut self,
        folder: &Path,
        film: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (dir, note) = match start_target(folder, film) {
            Target::Open(dir) => return self.open_project(&dir, window, cx),
            Target::Start { dir, note } => (dir, note),
            Target::Taken(why) => {
                self.message = Some(why);
                cx.notify();
                return;
            }
        };
        match crate::cli::init(&dir, film) {
            Ok(_) => {
                self.open_project(&dir, window, cx);
                if let Some(note) = note {
                    tell_open(&dir, note, cx);
                }
            }
            Err(err) => {
                self.message = Some(format!("Could not start a project: {err}"));
                cx.notify();
            }
        }
    }

    /// Opens the recording window on the project in `dir` (brings it to
    /// the front when it is open already) and closes this window; says why
    /// not when it cannot.
    pub fn open_project(&mut self, dir: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if focus_open(dir, cx) {
            if let Ok(dir) = std::path::absolute(dir) {
                remember_opened(&dir);
            }
            window.remove_window();
            return;
        }
        let opened = prepare(dir).and_then(|(dir, loaded)| {
            remember_opened(&dir);
            open_recording(dir, loaded, cx)
        });
        match opened {
            Ok(()) => window.remove_window(),
            Err(err) => {
                self.message = Some(format!("Cannot open {}: {err}", dir.display()));
                cx.notify();
            }
        }
    }

    fn recent_row(idx: usize, dir: &Path, cx: &mut Context<Self>) -> impl IntoElement {
        let name = dir.file_name().map_or_else(
            || dir.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let parent = dir
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let target = dir.to_owned();
        div()
            .id(("recent", idx))
            .px(px(10.))
            .py(px(6.))
            .rounded(px(4.))
            .cursor_pointer()
            .hover(|style| style.bg(rgb(colors::SELECTED)))
            .flex()
            .flex_col()
            .child(div().text_color(rgb(colors::TEXT)).child(name))
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(colors::DIM))
                    .child(parent),
            )
            .on_click(cx.listener(move |view, _, window, cx| {
                view.open_project(&target, window, cx);
            }))
    }
}

/// A button with `label`.
fn button(id: &'static str, label: &'static str) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .px(px(14.))
        .py(px(7.))
        .rounded(px(5.))
        .bg(rgb(colors::SELECTED))
        .text_color(rgb(colors::TEXT))
        .cursor_pointer()
        .child(label)
}

impl Render for StartWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .recent
            .clone()
            .iter()
            .enumerate()
            .map(|(idx, dir)| Self::recent_row(idx, dir, cx).into_any_element())
            .collect();
        let recent = if rows.is_empty() {
            div()
                .text_color(rgb(colors::DIM))
                .child("No recent projects.")
                .into_any_element()
        } else {
            div().flex().flex_col().children(rows).into_any_element()
        };
        div()
            .id("start-window")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(|_, _: &OpenProject, window, cx| {
                Self::choose_folder(window, cx);
            }))
            .on_action(cx.listener(|view, _: &InstallCommand, _, cx| {
                view.message = Some(crate::cli_install::install());
                cx.notify();
            }))
            .size_full()
            .flex()
            .flex_col()
            .gap(px(16.))
            .px(px(28.))
            .py(px(24.))
            .bg(rgb(colors::BACKGROUND))
            .text_color(rgb(colors::TEXT))
            .text_size(px(14.))
            .child(div().text_size(px(22.)).child("Takewise"))
            .child(
                div()
                    .text_color(rgb(colors::DIM))
                    .child("Record narration over a finished film, line by line."),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(10.))
                    .child(button("open-project", "Open project…").on_click(cx.listener(
                        |_, _, window, cx| Self::choose_folder(window, cx),
                    )))
                    .child(
                        button("install-command", "Install Command Line Tool…").on_click(
                            cx.listener(|view, _, _, cx| {
                                view.message = Some(crate::cli_install::install());
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(colors::DIM))
                    .child(
                        "Open a .takewise project folder, or any folder to start a new project there from a film.",
                    ),
            )
            .children(
                self.message
                    .clone()
                    .map(|m| div().text_color(rgb(colors::WARN)).child(m)),
            )
            .child(div().text_size(px(15.)).child("Recent projects"))
            .child(
                div()
                    .id("recent-projects")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .child(recent),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{Entity, TestAppContext, point};

    use super::*;

    /// A project folder (only its `project.toml` matters here).
    fn project_at(dir: &Path) {
        std::fs::create_dir_all(dir).expect("dir");
        std::fs::write(dir.join(tw::project::PROJECT_FILE), "").expect("project.toml");
    }

    #[test]
    fn a_project_folder_opens_and_any_other_starts_one() {
        let tmp = tempfile::tempdir().expect("dir");
        let project = tmp.path().join("promo.takewise");
        project_at(&project);
        assert_eq!(classify(&project), Picked::Project(project.clone()));
        let plain = tmp.path().join("Films");
        std::fs::create_dir(&plain).expect("dir");
        assert_eq!(classify(&plain), Picked::New(plain));
    }

    #[test]
    fn a_new_project_never_spills_into_a_full_folder() {
        let tmp = tempfile::tempdir().expect("dir");
        let film = tmp.path().join("Films/Kindred Promo.mov");
        // A folder named .takewise, or an empty one: the project goes there.
        let named = tmp.path().join("promo.takewise");
        assert_eq!(new_project_dir(&named, &film), named);
        let empty = tmp.path().join("Empty");
        std::fs::create_dir(&empty).expect("dir");
        assert_eq!(new_project_dir(&empty, &film), empty);
        // A folder with other files: a new project folder inside it.
        let films = tmp.path().join("Films");
        std::fs::create_dir(&films).expect("dir");
        std::fs::write(films.join("Kindred Promo.mov"), b"").expect("film");
        assert_eq!(
            new_project_dir(&films, &film),
            films.join("Kindred Promo.takewise")
        );
    }

    /// A project started for `film` in `dir`.
    fn project_for(dir: &Path, film: &Path) {
        let info = tw::FilmInfo {
            duration_s: 10.0,
            size: 1,
            modified: "2026-11-23T00:00:00Z".to_owned(),
        };
        tw::Project::init(dir, film, info).expect("project");
    }

    #[test]
    fn a_film_whose_project_exists_opens_it() {
        let tmp = tempfile::tempdir().expect("dir");
        let films = tmp.path().join("Films");
        std::fs::create_dir(&films).expect("dir");
        let film = films.join("Kindred Promo.mov");
        std::fs::write(&film, b"").expect("film");
        let project = films.join("Kindred Promo.takewise");
        assert_eq!(
            start_target(&films, &film),
            Target::Start {
                dir: project.clone(),
                note: None
            }
        );
        project_for(&project, &film);
        assert_eq!(start_target(&films, &film), Target::Open(project));
    }

    #[test]
    fn a_film_of_the_same_name_gets_its_own_project() {
        let tmp = tempfile::tempdir().expect("dir");
        let films = tmp.path().join("Films");
        std::fs::create_dir(&films).expect("dir");
        let (mp4, mov) = (films.join("Promo.mp4"), films.join("Promo.mov"));
        std::fs::write(&mp4, b"").expect("film");
        std::fs::write(&mov, b"").expect("film");
        project_for(&films.join("Promo.takewise"), &mp4);
        let Target::Start { dir, note } = start_target(&films, &mov) else {
            panic!("a new project for Promo.mov");
        };
        assert_eq!(dir, films.join("Promo.mov.takewise"));
        let note = note.expect("says why");
        assert!(
            note.contains("Promo.takewise is the project of Promo.mp4")
                && note.contains("Promo.mov.takewise"),
            "{note}"
        );
        // Started once, it opens next time.
        project_for(&dir, &mov);
        assert_eq!(start_target(&films, &mov), Target::Open(dir));
        assert_eq!(
            start_target(&films, &mp4),
            Target::Open(films.join("Promo.takewise"))
        );
    }

    #[test]
    fn recent_projects_most_recent_first() {
        let tmp = tempfile::tempdir().expect("dir");
        let path = tmp.path().join("settings/recent.json");
        assert_eq!(load_recent(&path), Vec::<PathBuf>::new());
        let root = std::fs::canonicalize(tmp.path()).expect("canonical");
        let dirs: Vec<PathBuf> = (0..10)
            .map(|n| root.join(format!("p{n}.takewise")))
            .collect();
        for dir in &dirs {
            project_at(dir);
            remember(&path, dir).expect("remember");
        }
        // Opened again: first, once.
        remember(&path, &dirs[3]).expect("remember");
        let recent = load_recent(&path);
        assert_eq!(recent.len(), MAX_RECENT);
        assert_eq!(recent[0], dirs[3]);
        assert_eq!(recent[1], dirs[9]);
        assert_eq!(recent.iter().filter(|d| **d == dirs[3]).count(), 1);
        // A project that is gone is left out.
        std::fs::remove_dir_all(&dirs[9]).expect("remove");
        assert!(!load_recent(&path).contains(&dirs[9]));
    }

    #[test]
    #[should_panic(expected = "isolate_settings")]
    fn a_test_that_remembers_without_isolated_settings_panics() {
        let tmp = tempfile::tempdir().expect("dir");
        remember_opened(tmp.path());
    }

    #[test]
    fn recent_projects_are_stored_resolved_and_once() {
        let tmp = tempfile::tempdir().expect("dir");
        let path = tmp.path().join("settings/recent.json");
        let dir = tmp.path().join("p.takewise");
        project_at(&dir);
        std::fs::create_dir_all(dir.join("takes")).expect("takes");
        remember(&path, &dir.join("takes").join("..")).expect("remember");
        remember(&path, &dir).expect("remember");
        let resolved = std::fs::canonicalize(&dir).expect("canonical");
        assert_eq!(read_recent(&path), vec![resolved.clone()]);
        // Entries an older version stored unresolved are cleaned on read.
        let raw = serde_json::to_vec(&vec![dir.join("takes/.."), dir.clone()]).expect("json");
        std::fs::write(&path, raw).expect("write");
        assert_eq!(load_recent(&path), vec![resolved]);
    }

    #[test]
    fn isolated_settings_live_in_a_temporary_folder() {
        let settings = isolate_settings();
        assert!(
            settings.starts_with(std::env::temp_dir()),
            "{}",
            settings.display()
        );
        assert_eq!(settings_path(), Some(settings.clone()));
        let tmp = tempfile::tempdir().expect("dir");
        project_at(tmp.path());
        remember_opened(tmp.path());
        assert_eq!(
            load_recent(&settings.with_file_name("recent.json")),
            vec![std::fs::canonicalize(tmp.path()).expect("canonical")]
        );
    }

    fn open_start(cx: &mut TestAppContext, recent: Vec<PathBuf>) -> Entity<StartWindow> {
        isolate_settings();
        cx.update(|cx| {
            gpui_kit::init(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.), px(0.)),
                    size: size(px(560.), px(520.)),
                })),
                ..WindowOptions::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| StartWindow::new(recent, window, cx))
            })
            .expect("open test window")
            .1
        })
    }

    #[gpui_kit::test]
    fn a_project_that_cannot_open_says_why(cx: &mut TestAppContext) {
        let tmp = tempfile::tempdir().expect("dir");
        let gone = tmp.path().join("gone.takewise");
        let view = open_start(cx, vec![gone.clone()]);
        let window = cx.windows()[0];
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |view, cx| view.open_project(&gone, window, cx));
        })
        .expect("window");
        let message = view.read_with(cx, |view, _| view.message.clone().unwrap_or_default());
        assert!(message.starts_with("Cannot open"), "{message}");
        assert_eq!(cx.windows().len(), 1, "the start window stays");
    }
}
