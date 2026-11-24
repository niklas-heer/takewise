//! Rescue copies in the window: audio that still cannot be saved into its
//! project when the window closes or the app quits is written to the
//! rescue folder beside the settings (see `takewise_core::rescue`), and
//! only then may the window go.

use super::*;

/// The rescue folder of the test's own settings.
pub(super) fn rescue_root() -> PathBuf {
    tw::rescue::beside(&crate::window::start::isolate_settings())
}

/// The one rescue copy made of the project's audio, and its folder.
pub(super) fn rescued(dir: &Path) -> (PathBuf, tw::rescue::RescuedAudio) {
    let mut found = tw::rescue::rescued_for(&rescue_root(), dir);
    assert_eq!(found.len(), 1, "{found:?}");
    let found = found.remove(0);
    assert_eq!(found.audio.len(), 1, "{found:?}");
    (
        found.folder,
        found.audio.into_iter().next().expect("a copy"),
    )
}

/// Brings a rescue copy back by hand, as `docs/reference.md` says, and has
/// a repair adopt it.
pub(super) fn bring_back(
    dir: &Path,
    folder: &Path,
    rescued: &tw::rescue::RescuedAudio,
) -> tw::RepairReport {
    let audio = dir.join(&rescued.restore.audio);
    fs::create_dir_all(audio.parent().expect("a folder")).expect("folder");
    fs::copy(folder.join(&rescued.audio), &audio).expect("audio");
    let sidecar = serde_json::to_vec_pretty(&rescued.sidecar).expect("json");
    fs::write(dir.join(&rescued.restore.sidecar), sidecar).expect("sidecar");
    tw::repair(dir).expect("repair")
}

/// Makes line `a`'s takes impossible to begin or save: a file where their
/// directory belongs, as a project folder that cannot be written.
fn block_takes_of_a(dir: &Path) -> PathBuf {
    let blocker = dir.join("takes/a");
    fs::create_dir_all(dir.join("takes")).expect("takes");
    fs::write(&blocker, b"").expect("blocker");
    blocker
}

#[gpui_kit::test]
fn closing_again_rescues_a_take_that_cannot_be_saved(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    use_settings(cx, &f);
    let blocker = block_takes_of_a(&f.dir);
    record_take(cx, &f);
    assert_eq!(recorded(&f.dir).len(), 0);
    let root = rescue_root();

    // Closing is refused once, and the window says what closing again
    // does. Nothing is written yet: saving may still work.
    assert!(!VisualTestContext::from_window(f.window, cx).simulate_close());
    let shown = message(cx, &f.view);
    assert!(
        shown.starts_with("Not closing: 1 take is only in memory, not saved yet ("),
        "{shown}"
    );
    let offer = format!(
        "If it cannot be fixed, close again now: Takewise then writes a rescue copy to {} and closes.",
        root.display()
    );
    assert!(shown.ends_with(&offer), "{shown}");
    assert!(!root.exists(), "no copy before it is asked for");

    // Saving still fails. Closing again writes the copy; the window goes.
    assert!(VisualTestContext::from_window(f.window, cx).simulate_close());
    let (folder, copy) = rescued(&f.dir);
    assert!(folder.starts_with(&root), "{}", folder.display());
    let name = folder.file_name().expect("name").to_string_lossy();
    assert!(name.starts_with("test.takewise-20"), "{name}");
    assert_eq!(copy.kind, tw::rescue::Kind::Take);
    assert_eq!(copy.lines, ["a"]);
    assert_eq!((copy.sample_rate, copy.samples), (48_000, 2400));
    // The audio, and with it the line as it was read and where the take
    // lies against it.
    let (samples, rate) = tw::read_take_wav(&folder.join(&copy.audio)).expect("the copy");
    assert_eq!((samples.len(), rate), (2400, 48_000));
    let snapshot = Project::open(&f.dir).expect("project").snapshot("a");
    let snapshot = snapshot.expect("snapshot");
    assert_eq!(copy.sidecar["snapshot"]["text"], snapshot.text);
    assert_eq!(copy.sidecar["snapshot"]["start"], snapshot.start);
    assert_eq!(copy.sidecar["device"], DEVICE);
    assert!(copy.sidecar["line_offset_s"].is_f64(), "{:?}", copy.sidecar);
    assert_eq!(recorded(&f.dir).len(), 0, "the project is as it was");

    // `status` would say so, and the next window on the project does.
    let notices = tw::rescue::notices(&root, &f.dir);
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0]
            .message
            .starts_with("1 take (a) could not be saved")
    );

    // Once the project can be written again, the copy comes back.
    fs::remove_file(blocker).expect("unblock");
    let report = bring_back(&f.dir, &folder, &copy);
    assert_eq!(report.adopted.len(), 1, "{report:?}");
    let takes = recorded(&f.dir);
    assert_eq!((takes[0].0.as_str(), takes[0].2.samples), ("a", 2400));
    let file = f.dir.join(&takes[0].2.file);
    assert_eq!(tw::read_take_wav(&file).expect("the take").0, samples);
}

#[gpui_kit::test]
fn audio_that_came_since_the_refusal_is_refused_once_too(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    use_settings(cx, &f);
    block_takes_of_a(&f.dir);
    record_take(cx, &f);
    assert!(!VisualTestContext::from_window(f.window, cx).simulate_close());
    // Another take that cannot be saved: the narrator hears of both
    // before a rescue copy stands in for them.
    begin(cx, &f, 120.0);
    f.media.set_now(120.5);
    press(cx, f.window, &["space"]);
    assert!(!VisualTestContext::from_window(f.window, cx).simulate_close());
    let shown = message(cx, &f.view);
    assert!(
        shown.starts_with("Not closing: 2 takes are only in memory"),
        "{shown}"
    );
    assert!(!rescue_root().exists());
    // ⌘Q asks the same question: now both get their copy, in one folder.
    assert!(!cx.update(crate::window::holds_unsaved_audio));
    let found = tw::rescue::rescued_for(&rescue_root(), &f.dir);
    assert_eq!(found.len(), 1, "{found:?}");
    let files: Vec<&str> = found[0].audio.iter().map(|a| a.audio.as_str()).collect();
    assert_eq!(files, ["001-take-a.wav", "002-take-a.wav"]);
}

#[gpui_kit::test]
fn a_quit_from_outside_rescues_what_cannot_be_saved(cx: &mut TestAppContext) {
    // The Dock's Quit and a logout end the app without asking the window:
    // GPUI only says that the app is quitting. A take still being read,
    // whose audio could not even be begun on disk, is rescued then.
    let f = open(cx, &[(DEVICE, 48_000)]);
    use_settings(cx, &f);
    block_takes_of_a(&f.dir);
    f.media.set_now(100.0);
    press(cx, f.window, &["space"]);
    f.media.send(100.2, 4800);
    settle(cx, &f);
    f.media.set_now(100.3);
    frame(cx, f.window);
    assert!(message(cx, &f.view).contains("kept in memory"));
    f.media.send(100.3, 4800);
    settle(cx, &f);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_recording()));

    cx.quit();
    let (folder, copy) = rescued(&f.dir);
    assert_eq!(
        (copy.lines.as_slice(), copy.samples),
        (&["a".to_owned()][..], 9600)
    );
    let (samples, _) = tw::read_take_wav(&folder.join(&copy.audio)).expect("the copy");
    assert_eq!(samples.len(), 9600);
    // Quitting twice does not copy twice.
    f.view.update(cx, |view, _| view.shutdown());
    rescued(&f.dir);
}

#[gpui_kit::test]
fn a_quit_from_outside_keeps_a_take_that_can_be_saved_in_its_project(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    use_settings(cx, &f);
    begin(cx, &f, 100.0);
    cx.quit();
    assert_eq!(recorded(&f.dir).len(), 1);
    assert!(!rescue_root().exists(), "nothing needed a rescue copy");
}

#[gpui_kit::test]
fn the_window_stays_open_when_the_rescue_copy_cannot_be_written_either(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    use_settings(cx, &f);
    begin(cx, &f, 100.0); // streaming into its partial files
    // The disk fails for good: finishing the audio fails, and so does
    // every whole WAV written after it, in the project and elsewhere.
    f.view
        .read_with(cx, |view, _| view.recorder.fail_next_finish());
    tw::takes::fail_wav_writes_after(Some(100));
    f.media.set_now(100.5);
    press(cx, f.window, &["space"]);
    assert!(message(cx, &f.view).contains("could not be saved"));

    assert!(!VisualTestContext::from_window(f.window, cx).simulate_close());
    assert!(message(cx, &f.view).contains("close again now"));
    // Asked again: the copy fails too, so the audio stays where it is, in
    // memory, and the window with it.
    for _ in 0..2 {
        assert!(!VisualTestContext::from_window(f.window, cx).simulate_close());
        let shown = message(cx, &f.view);
        assert!(
            shown.starts_with("Not closing: 1 take is only in memory, not saved yet (")
                && shown.contains("and the rescue copy could not be written either (")
                && shown.contains("keeps trying to save every 2 s"),
            "{shown}"
        );
    }
    assert_eq!(tw::rescue::rescued_for(&rescue_root(), &f.dir), []);
    let leftovers = fs::read_dir(rescue_root()).map_or(0, Iterator::count);
    assert_eq!(leftovers, 0, "no empty rescue folders");

    // The disk works again: the take is saved where it belongs, and the
    // window closes without a rescue copy.
    tw::takes::fail_wav_writes_after(None);
    assert!(VisualTestContext::from_window(f.window, cx).simulate_close());
    let takes = recorded(&f.dir);
    assert_eq!(takes.len(), 1);
    assert_eq!(takes[0].2.samples, 2400);
    assert_eq!(tw::rescue::rescued_for(&rescue_root(), &f.dir), []);
}

#[gpui_kit::test]
fn a_rescued_take_that_is_saved_after_all_says_its_copy_is_spare(cx: &mut TestAppContext) {
    // Two windows, one quit: this window's audio got its copy, but the
    // app stayed open for the other one, and saving worked later.
    let f = open(cx, &[(DEVICE, 48_000)]);
    use_settings(cx, &f);
    let blocker = block_takes_of_a(&f.dir);
    record_take(cx, &f);
    f.view.update(cx, |view, _| {
        view.rescue_unsaved().expect("rescued");
        // Audio with a rescue copy no longer holds the window or the app.
        assert!(!view.recorder.needs_rescue());
    });
    assert!(!cx.update(crate::window::holds_unsaved_audio));
    let (folder, copy) = rescued(&f.dir);
    let shown = message(cx, &f.view);
    assert_eq!(
        shown,
        format!(
            "1 take could not be saved into the project and was only in memory: a rescue copy is in {}. `takewise status` says how to bring the audio back.",
            folder.display()
        )
    );

    fs::remove_file(blocker).expect("unblock");
    f.media.set_now(110.0);
    frame(cx, f.window);
    assert_eq!(recorded(&f.dir).len(), 1, "saved on retry");
    let shown = message(cx, &f.view);
    let spare = format!(
        "Its rescue copy {} is no longer needed.",
        folder.join(&copy.audio).display()
    );
    assert!(shown.contains(&spare), "{shown}");
    assert!(folder.join(&copy.audio).is_file(), "nothing removes it");
}

#[gpui_kit::test]
fn a_window_says_when_its_project_has_rescued_audio(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    let root = rescue_root();
    let snapshot = Project::open(&f.dir).expect("project").snapshot("b");
    let input = RecordedInput {
        sample_rate: 48_000,
        device: DEVICE.to_owned(),
        channel: 1,
        line_offset_s: -2.0,
        snapshot: snapshot.expect("snapshot"),
        dropouts: None,
        film_shift_ms: None,
        passage: None,
    };
    let at = "2026-11-24T10:15:30Z".parse().expect("a time");
    let mut rescue = tw::rescue::Rescue::create(&root, &f.dir, at).expect("folder");
    rescue.add_take("b", &input, &[0; 480]).expect("copy");
    let folder = rescue.finish().expect("synced");

    // As when the window opens.
    use_settings(cx, &f);
    let shown = message(cx, &f.view);
    assert!(
        shown.starts_with(
            "Rescued audio: 1 take (b) could not be saved into this project on 2026-11-24 and is kept in "
        ) && shown.contains(&folder.display().to_string()),
        "{shown}"
    );
}
