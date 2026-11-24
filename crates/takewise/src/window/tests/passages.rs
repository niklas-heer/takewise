//! Passages in the window: Shift+Space records several lines in one go,
//! Shift+P plays the passage and Shift+Enter chooses it, with the simulated
//! media. A take of line `a` begun at host 100 plays the film from 1.0 at
//! host 100.4, so its first sample (host 100.0) lies at film 0.6.

use super::*;
use crate::window::prompter::Passed;

/// Three sentences close together (one passage), a pause for the picture,
/// then two more.
const PASSAGES: &str = r#"[[line]]
id = "a"
start = 3.0
end = 5.0
text = "First sentence."

[[line]]
id = "b"
start = 5.4
end = 7.4
text = "Second sentence."
note = "Lighter."

[[line]]
id = "c"
start = 7.8
end = 9.8
text = "Third sentence."

[[line]]
id = "d"
start = 12.0
end = 14.0
text = "After the pause."

[[line]]
id = "e"
start = 14.3
text = "The last one."
"#;

/// Where the narrator speaks, in film seconds: inside each line's slot,
/// with pauses between the sentences.
const SPEECH: [(f64, f64); 3] = [(3.1, 4.8), (5.5, 7.2), (7.9, 9.6)];

/// Film time of the first sample of a take of line `a` begun at host 100.
const FIRST_FILM_S: f64 = 0.6;

fn open_passages(cx: &mut TestAppContext) -> Fixture {
    open_with(cx, &[(DEVICE, 48_000)], |dir| {
        fs::write(dir.join("script.toml"), PASSAGES).expect("script");
    })
}

/// Host time at which the film is at `film_s`, in a take of line `a` begun
/// at host 100.
fn host(film_s: f64) -> f64 {
    film_s + 99.4
}

/// The sample value heard at film time `film_s`: the voice inside
/// [`SPEECH`], digital silence in the pre-roll and the pauses.
fn heard(film_s: f64) -> i32 {
    if SPEECH
        .iter()
        .any(|(from, to)| (*from..*to).contains(&film_s))
    {
        1_000_000
    } else {
        0
    }
}

/// Presses `key` at host 100 on line `a` and delivers the capture's buffers
/// (100 ms each) as the narrator reads up to film time `until`. The frame
/// at host 100.1 sees the first buffer and schedules the film.
fn read_from_a(cx: &mut TestAppContext, f: &Fixture, key: &str, until: f64) {
    f.media.set_now(100.0);
    press(cx, f.window, &[key]);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let buffers = ((until - FIRST_FILM_S) * 10.0).round() as u32;
    for k in 0..buffers {
        let from = FIRST_FILM_S + 0.1 * f64::from(k);
        f.media
            .send_value(host(from) + LATENCY_S, 4800, heard(from + 0.05));
        if k == 0 {
            settle(cx, f);
            f.media.set_now(100.1);
            frame(cx, f.window);
        }
    }
    settle(cx, f);
}

/// Renders a frame at the host time the film is at `film_s`.
fn at_film(cx: &mut TestAppContext, f: &Fixture, film_s: f64) {
    f.media.set_now(host(film_s));
    frame(cx, f.window);
}

/// Renders frames until the stopped passage is cut into its takes (by the
/// worker thread) and the window took them in.
fn cut(cx: &mut TestAppContext, f: &Fixture) {
    for _ in 0..5000 {
        frame(cx, f.window);
        if !f.view.read_with(cx, |view, _| view.recorder.has_unsaved()) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("timed out waiting for the passage's takes");
}

/// Records the passage `a` to `c` whole with Shift+Space (it stops on its
/// own after `c`'s tail, at film 11.3) and waits for its takes.
fn record_passage(cx: &mut TestAppContext, f: &Fixture) {
    read_from_a(cx, f, "shift-space", 11.3);
    at_film(cx, f, 11.35);
    cut(cx, f);
}

/// The files under `takes/.passages/` whose names end with `suffix`.
fn passage_files(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir.join("takes/.passages")) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.to_string_lossy().ends_with(suffix))
        .collect();
    files.sort();
    files
}

/// The lines and takes of the `recorded` events, in ledger order.
fn recorded_takes(dir: &Path) -> Vec<(String, u32)> {
    recorded(dir)
        .into_iter()
        .map(|(line, take, _)| (line, take))
        .collect()
}

fn takes_of(lines: &[&str]) -> Vec<(String, u32)> {
    lines.iter().map(|line| ((*line).to_owned(), 1)).collect()
}

fn recording(cx: &mut TestAppContext, f: &Fixture) -> bool {
    f.view.read_with(cx, |view, _| view.recorder.is_recording())
}

/// A `chosen` event: line, take and by whom.
type Choice = (String, u32, String);

/// The `chosen` events.
fn chosen(dir: &Path) -> Vec<Choice> {
    tw::read(dir)
        .expect("ledger")
        .events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Chosen(chosen) => Some((event.line.clone(), event.take, chosen.by.clone())),
            _ => None,
        })
        .collect()
}

#[gpui_kit::test]
fn shift_space_records_a_passage_as_one_file_and_a_take_per_line(cx: &mut TestAppContext) {
    let f = open_passages(cx);
    read_from_a(cx, &f, "shift-space", 11.3);
    at_film(cx, &f, 3.1);
    assert!(recording(cx, &f));
    // It streams into the passage's partial files, which a repair leaves
    // alone while the take runs.
    assert_eq!(passage_files(&f.dir, ".wav.partial").len(), 1);
    assert_eq!(passage_files(&f.dir, ".json.partial").len(), 1);
    let report = tw::repair(&f.dir).expect("repair");
    assert!(report.passages_published.is_empty(), "{report:?}");
    assert!(report.pieces.is_empty(), "{report:?}");

    // One capture through b and c; it stops on its own tail_s (1.5 s) after
    // c's slot, the passage's last: d starts 2.2 s later.
    at_film(cx, &f, 11.25);
    assert!(recording(cx, &f));
    assert_eq!(
        f.media.0.borrow().plays.len(),
        2,
        "the warm-up and one play"
    );
    at_film(cx, &f, 11.35);
    assert!(!recording(cx, &f));
    cut(cx, &f);

    let wavs = passage_files(&f.dir, ".wav");
    assert_eq!(wavs.len(), 1, "one passage file");
    assert_eq!(passage_files(&f.dir, ".partial"), Vec::<PathBuf>::new());
    let (samples, _) = tw::read_take_wav(&wavs[0]).expect("the passage");
    assert_eq!(samples.len(), 107 * 4800, "film 0.6 to 11.3");

    let takes = recorded(&f.dir);
    assert_eq!(recorded_takes(&f.dir), takes_of(&["a", "b", "c"]));
    let pieces: Vec<_> = takes
        .iter()
        .map(|(_, _, rec)| rec.passage.clone().expect("cut from the passage"))
        .collect();
    for (index, piece) in pieces.iter().enumerate() {
        assert_eq!((piece.index as usize, piece.count), (index, 3));
        assert_eq!(wavs[0], f.dir.join(tw::passage_file(&piece.id)));
    }
    // The pieces cover the passage, cut in the pauses between the sentences.
    assert_eq!(pieces[0].from_sample, 0);
    assert_eq!(pieces[0].to_sample, pieces[1].from_sample);
    assert_eq!(pieces[1].to_sample, pieces[2].from_sample);
    assert_eq!(pieces[2].to_sample, 107 * 4800);
    let film = |sample: u64| FIRST_FILM_S + sample as f64 / 48_000.0;
    let (cut_b, cut_c) = (film(pieces[1].from_sample), film(pieces[2].from_sample));
    assert!((4.8..=5.5).contains(&cut_b), "{cut_b}");
    assert!((7.2..=7.9).contains(&cut_c), "{cut_c}");
    // Each take lies where it was recorded.
    assert!(close(takes[0].2.line_offset_s, FIRST_FILM_S - 3.0));
    assert!(close(takes[1].2.line_offset_s, cut_b - 5.4));
    assert!(close(takes[2].2.line_offset_s, cut_c - 7.8));
    assert!(!takes.iter().any(|(_, _, rec)| rec.recovered));
    assert!(!f.dir.join("takes/d").exists(), "d is not in the passage");

    let shown = message(cx, &f.view);
    assert!(
        shown.contains("Recorded a passage of 3 lines (a to c)."),
        "{shown}"
    );
    f.view.read_with(cx, |view, _| {
        assert_eq!(
            view.line, 0,
            "a passage read to its end goes back to its first line"
        );
        assert_eq!(view.take, Some(1), "its new take is selected");
        let take = &view.line_state().expect("line").takes[0];
        assert_eq!(
            super::super::takes::passage_mark(take).as_deref(),
            Some("passage 1/3")
        );
    });
    // So Shift+Space reads the whole passage again.
    f.media.set_now(200.0);
    press(cx, f.window, &["shift-space"]);
    let reading = f.view.read_with(cx, |view, _| {
        view.recorder
            .passage_reading()
            .map(|(index, id)| (index, id.to_owned()))
    });
    assert_eq!(reading, Some((0, "a".to_owned())));
    press(cx, f.window, &["space"]);
}

/// A passage whose first line is long enough to scroll.
fn long_first_line() -> String {
    let words = "every answer becomes a page worth opening again ".repeat(9);
    format!(
        "[[line]]\nid = \"a\"\nstart = 3.0\nend = 9.0\ntext = \"{}\"\n\n\
         [[line]]\nid = \"b\"\nstart = 9.4\nend = 11.4\ntext = \"Second sentence.\"\nnote = \"Lighter.\"\n\n\
         [[line]]\nid = \"c\"\nstart = 11.8\nend = 13.8\ntext = \"Third sentence.\"\n\n\
         [[line]]\nid = \"d\"\nstart = 16.0\ntext = \"After the pause.\"\n",
        words.trim()
    )
}

#[gpui_kit::test]
fn the_prompter_follows_the_passage_and_starts_each_line_at_the_top(cx: &mut TestAppContext) {
    let script = long_first_line();
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        fs::write(dir.join("script.toml"), &script).expect("script");
    });
    read_from_a(cx, &f, "shift-space", 0.7);
    let shown = |cx: &mut TestAppContext| {
        f.view.read_with(cx, |view, _| {
            let (note, next) = view.prompter_cues();
            (
                view.prompter_text().map(str::to_owned),
                note.map(str::to_owned),
                next.map(str::to_owned),
                view.recorder.recording_line().map(str::to_owned),
                view.line,
            )
        })
    };
    let scroll = |cx: &mut TestAppContext| {
        let now = f.media.0.borrow().now;
        f.view.read_with(cx, |view, _| view.scroll.at(now))
    };

    // Line a scrolls while the film crosses its slot.
    for step in 0..=23 {
        at_film(cx, &f, 3.0 + f64::from(step) * 0.25);
        frame(cx, f.window);
    }
    assert!(scroll(cx) >= 1.0, "the long line scrolled: {}", scroll(cx));
    let (text, _, next, line, current) = shown(cx);
    assert!(text.is_some_and(|text| text.starts_with("every answer")));
    assert_eq!(next.as_deref(), Some("Second sentence."));
    assert_eq!((line.as_deref(), current), (Some("a"), 0));

    // The film reaches b's start: its text, note and next line, from the
    // top, and the current line follows.
    at_film(cx, &f, 9.39);
    assert_eq!(shown(cx).3.as_deref(), Some("a"), "not before its start");
    at_film(cx, &f, 9.41);
    assert_eq!(
        shown(cx),
        (
            Some("Second sentence.".to_owned()),
            Some("Lighter.".to_owned()),
            Some("Third sentence.".to_owned()),
            Some("b".to_owned()),
            1
        )
    );
    assert!(scroll(cx).abs() < f32::EPSILON, "b starts at the top");
    at_film(cx, &f, 9.8);
    frame(cx, f.window);
    assert!(scroll(cx).abs() < f32::EPSILON, "a short line stays there");

    // c is the passage's last line: the line after the passage is next.
    at_film(cx, &f, 11.85);
    assert_eq!(
        shown(cx),
        (
            Some("Third sentence.".to_owned()),
            None,
            Some("After the pause.".to_owned()),
            Some("c".to_owned()),
            2
        )
    );
    press(cx, f.window, &["space"]);
    assert!(!recording(cx, &f));
    cut(cx, &f);
}

#[gpui_kit::test]
fn the_prompter_keeps_the_line_just_read_in_view_for_a_moment(cx: &mut TestAppContext) {
    let f = open_passages(cx);
    read_from_a(cx, &f, "shift-space", 0.7);
    // The prompter's text, the line kept above it, and how far the pace
    // guide filled the text.
    let shown = |cx: &mut TestAppContext, film_s: f64| {
        at_film(cx, &f, film_s);
        f.view.read_with(cx, |view, _| {
            let reading = view.prompter_line().expect("a line");
            (
                reading.text.to_owned(),
                match view.prompter_passed() {
                    Passed::Off => None,
                    Passed::Empty => Some(String::new()),
                    Passed::Line(text) => Some(text.to_owned()),
                },
                super::super::prompter::filled_bytes(reading, film_s),
            )
        })
    };
    let first = Some("First sentence.".to_owned());
    let empty = Some(String::new());
    // While a is read, its row above is kept, empty.
    let (text, passed, _) = shown(cx, 4.0);
    assert_eq!((text.as_str(), passed), ("First sentence.", empty.clone()));
    // At b's cue b is current, its pace guide filling from the cue, and a
    // stays above it, for 1.5 s of film.
    let (text, passed, filled) = shown(cx, 5.41);
    assert_eq!(
        (text.as_str(), passed.clone(), filled),
        ("Second sentence.", first.clone(), 0)
    );
    let (text, passed, filled) = shown(cx, 6.85);
    assert_eq!((text.as_str(), passed), ("Second sentence.", first));
    assert!(filled > 0, "{filled}");
    let (_, passed, _) = shown(cx, 6.95);
    assert_eq!(passed, empty, "the row stays, empty");
    // c follows b the same way.
    let (text, passed, _) = shown(cx, 7.85);
    assert_eq!(
        (text.as_str(), passed),
        ("Third sentence.", Some("Second sentence.".to_owned()))
    );
    press(cx, f.window, &["space"]);
    cut(cx, &f);
    assert!(
        f.view
            .read_with(cx, |view, _| view.prompter_passed() == Passed::Off)
    );

    // A take of one line has no such row.
    press(cx, f.window, &["down"]); // d
    begin(cx, &f, 300.0);
    assert!(recording(cx, &f));
    assert!(
        f.view
            .read_with(cx, |view, _| view.prompter_passed() == Passed::Off)
    );
    f.media.set_now(300.5);
    press(cx, f.window, &["space"]);
}

#[gpui_kit::test]
fn space_stops_a_passage_and_lines_not_read_get_no_take(cx: &mut TestAppContext) {
    let f = open_passages(cx);
    // c has begun (film 7.8), but nothing of it is read when Space stops.
    read_from_a(cx, &f, "shift-space", 7.9);
    f.media.set_now(host(7.9));
    press(cx, f.window, &["space"]);
    assert!(!recording(cx, &f));
    cut(cx, &f);

    assert_eq!(recorded_takes(&f.dir), takes_of(&["a", "b"]));
    let takes = recorded(&f.dir);
    let last = takes[1].2.passage.clone().expect("a piece");
    assert_eq!((last.index, last.count), (1, 2));
    assert_eq!(last.to_sample, 73 * 4800, "b's take runs to the stop");
    assert!(!f.dir.join("takes/c").exists());
    let shown = message(cx, &f.view);
    assert!(
        shown.contains("Recorded a passage of 2 lines (a to b). No take for c: not read"),
        "{shown}"
    );
    f.view.read_with(cx, |view, _| {
        assert_eq!(view.line, 2, "c was being read");
        assert_eq!(view.take, None);
    });

    // The passage is chosen for the lines it has takes of.
    press(cx, f.window, &["shift-enter"]);
    assert!(message(cx, &f.view).contains("This line has no take yet."));
    assert_eq!(chosen(&f.dir), Vec::<Choice>::new());
    press(cx, f.window, &["up", "shift-enter"]);
    let by = "narrator".to_owned();
    assert_eq!(
        chosen(&f.dir),
        vec![("a".to_owned(), 1, by.clone()), ("b".to_owned(), 1, by)]
    );
    let shown = message(cx, &f.view);
    assert!(
        shown.contains("Chose the passage for 2 lines (a to b). Left as they were: c"),
        "{shown}"
    );
}

#[gpui_kit::test]
fn a_passage_ends_at_a_gap_of_passage_gap_s(cx: &mut TestAppContext) {
    // c's slot ends 2.2 s before d starts. With the default gap of 1.0 s
    // the passage from a ends with c, and the take 1.5 s after c's slot.
    let f = open_passages(cx);
    read_from_a(cx, &f, "shift-space", 0.7);
    at_film(cx, &f, 11.35);
    assert!(!recording(cx, &f));
    cut(cx, &f);

    // With a gap of 2.5 s, d and e belong to it too.
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        fs::write(dir.join("script.toml"), PASSAGES).expect("script");
        let config = dir.join("project.toml");
        let text = fs::read_to_string(&config).expect("project.toml");
        assert!(text.contains("passage_gap_s = 1.0\n"), "{text}");
        let text = text.replacen("passage_gap_s = 1.0\n", "passage_gap_s = 2.5\n", 1);
        fs::write(&config, text).expect("project.toml");
    });
    read_from_a(cx, &f, "shift-space", 0.7);
    at_film(cx, &f, 11.35);
    assert!(recording(cx, &f));
    at_film(cx, &f, 12.05);
    let reading = f.view.read_with(cx, |view, _| {
        view.recorder.recording_line().map(str::to_owned)
    });
    assert_eq!(reading.as_deref(), Some("d"));
    press(cx, f.window, &["shift-space"]);
    assert!(!recording(cx, &f), "Shift+Space stops the take too");
    cut(cx, &f);
}

#[gpui_kit::test]
fn shift_space_on_a_passages_last_line_records_that_line(cx: &mut TestAppContext) {
    let f = open_passages(cx);
    press(cx, f.window, &["down", "down"]); // c, the last line of a..c
    f.media.set_now(100.0);
    press(cx, f.window, &["shift-space"]);
    f.media.send(100.05, 2400);
    settle(cx, &f);
    f.media.set_now(100.1);
    frame(cx, f.window);
    assert!(recording(cx, &f));
    assert!(
        f.view
            .read_with(cx, |view, _| view.recorder.passage_reading().is_none())
    );
    f.media.set_now(100.5);
    press(cx, f.window, &["shift-space"]);

    let takes = recorded(&f.dir);
    assert_eq!(recorded_takes(&f.dir), takes_of(&["c"]));
    assert_eq!(takes[0].2.passage, None, "an ordinary take");
    assert!(!f.dir.join("takes/.passages").exists());
    assert!(message(cx, &f.view).contains("Recorded c take 1."));
}

#[gpui_kit::test]
fn shift_p_plays_the_whole_passage_locked_to_the_film(cx: &mut TestAppContext) {
    let f = open_passages(cx);
    record_passage(cx, &f);
    f.media.set_now(200.0);
    press(cx, f.window, &["shift-p"]);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    // The passage file from its own first sample, film 0.6: both start at
    // host 200.3.
    let wav = passage_files(&f.dir, ".wav").remove(0);
    assert_eq!(f.media.0.borrow().opened_takes.last(), Some(&wav));
    let (film_s, host_s) = last_play(&f);
    assert!(
        close(film_s, FIRST_FILM_S) && close(host_s, 200.3),
        "{film_s} {host_s}"
    );
    let (from_s, at_host_s) = f.media.0.borrow().take_plays[0];
    assert!(
        close(from_s, 0.0) && close(at_host_s, 200.3),
        "{from_s} {at_host_s}"
    );
    // It plays to the passage's end, film 11.3, at host 211.0.
    f.media.set_now(210.9);
    frame(cx, f.window);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    f.media.set_now(211.1);
    frame(cx, f.window);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));

    // Shift+P again stops it, as P does.
    press(cx, f.window, &["shift-p"]);
    assert!(f.view.read_with(cx, |view, _| view.recorder.is_playing()));
    press(cx, f.window, &["shift-p"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));

    // From b's take, the film starts one pre-roll before b's piece, which
    // begins in the pause before b's start, and the passage file plays
    // from the sample the film starts at; both at host 300.3, to the
    // passage's end (film 11.3).
    let b_first = 5.4 + recorded(&f.dir)[1].2.line_offset_s;
    assert!(b_first < 5.4, "{b_first}");
    let from = b_first - 2.0;
    press(cx, f.window, &["down"]);
    f.media.set_now(300.0);
    press(cx, f.window, &["shift-p"]);
    frame(cx, f.window);
    assert_eq!(f.media.0.borrow().opened_takes.last(), Some(&wav));
    let (film_s, host_s) = last_play(&f);
    assert!(
        close(film_s, from) && close(host_s, 300.3),
        "{film_s} {host_s}"
    );
    let (from_s, at_host_s) = *f.media.0.borrow().take_plays.last().expect("a play");
    assert!(
        close(from_s, from - FIRST_FILM_S) && close(at_host_s, 300.3),
        "{from_s} {at_host_s}"
    );
    let end_host = 300.3 + 11.3 - from;
    for (now, playing) in [(end_host - 0.1, true), (end_host + 0.1, false)] {
        f.media.set_now(now);
        frame(cx, f.window);
        assert_eq!(
            f.view.read_with(cx, |view, _| view.recorder.is_playing()),
            playing,
            "at host {now}"
        );
    }
}

#[gpui_kit::test]
fn p_plays_a_later_piece_from_its_own_start(cx: &mut TestAppContext) {
    let f = open_passages(cx);
    record_passage(cx, &f);
    let takes = recorded(&f.dir);
    // b's take begins at the cut before it, after the film's start point
    // (b's pre-roll from film 3.4): it starts later, from its beginning.
    let b_first = 5.4 + takes[1].2.line_offset_s;
    press(cx, f.window, &["down"]); // b
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    let (film_s, host_s) = last_play(&f);
    assert!(
        close(film_s, 3.4) && close(host_s, 200.3),
        "{film_s} {host_s}"
    );
    let (from_s, at_host_s) = f.media.0.borrow().take_plays[0];
    assert!(
        close(from_s, 0.0) && close(at_host_s, 200.3 + b_first - 3.4),
        "{from_s} {at_host_s}"
    );
    press(cx, f.window, &["p"]);

    // a's take, the first piece, begins before the film's start point
    // (film 1.0), as a take of one line does: it starts 0.4 s in.
    press(cx, f.window, &["up"]); // a
    f.media.set_now(300.0);
    press(cx, f.window, &["p"]);
    let (from_s, at_host_s) = f.media.0.borrow().take_plays[1];
    assert!(
        close(from_s, 0.4) && close(at_host_s, 300.3),
        "{from_s} {at_host_s}"
    );
}

#[gpui_kit::test]
fn p_plays_a_piece_through_its_end_past_its_slot(cx: &mut TestAppContext) {
    let f = open(cx, &[(DEVICE, 48_000)]);
    // b (start 1.0) ends its slot where c starts (1.5), so play in context
    // of a take of one line stops at film 3.0, the tail after it. This
    // piece begins after that, at film 3.5, and lasts 1 s.
    let snapshot = Project::open(&f.dir)
        .expect("project")
        .snapshot("b")
        .expect("snapshot");
    let input = RecordedInput {
        sample_rate: 48_000,
        device: DEVICE.to_owned(),
        channel: 1,
        line_offset_s: 2.5,
        snapshot,
        dropouts: None,
        film_shift_ms: None,
        passage: Some(tw::PassagePiece {
            id: "00000000-0000-4000-8000-000000000000".to_owned(),
            index: 1,
            count: 2,
            from_sample: 48_000,
            to_sample: 96_000,
            preroll_floor_dbfs: -70.0,
        }),
    };
    let partial = tw::begin_take(&f.dir, "b", &input).expect("begin");
    tw::write_take_wav(&partial.wav, &vec![0; 48_000], 48_000).expect("wav");
    tw::commit_take(&f.dir, partial).expect("commit");
    f.view.update(cx, |view, _| view.reload());

    press(cx, f.window, &["down"]); // b
    f.media.set_now(200.0);
    press(cx, f.window, &["p"]);
    frame(cx, f.window);
    // The film plays from 0.0 at host 201.3 (after holding its first frame
    // through b's short pre-roll); the piece starts at film 3.5.
    let (from_s, at_host_s) = f.media.0.borrow().take_plays[0];
    assert!(
        close(from_s, 0.0) && close(at_host_s, 204.8),
        "{from_s} {at_host_s}"
    );
    // It plays through the piece's end (film 4.5) and the tail: to film
    // 6.0, host 207.3.
    for (now, playing) in [(204.5, true), (207.2, true), (207.4, false)] {
        f.media.set_now(now);
        frame(cx, f.window);
        assert_eq!(
            f.view.read_with(cx, |view, _| view.recorder.is_playing()),
            playing,
            "at host {now}"
        );
    }
}

#[gpui_kit::test]
fn shift_enter_chooses_the_passage_for_every_line(cx: &mut TestAppContext) {
    let f = open_passages(cx);
    record_passage(cx, &f);
    // Line b is read once more on its own: its take 2.
    press(cx, f.window, &["down"]);
    begin(cx, &f, 300.0);
    f.media.set_now(300.5);
    press(cx, f.window, &["space"]);
    assert_eq!(f.view.read_with(cx, |view, _| view.take), Some(2));

    press(cx, f.window, &["shift-enter"]);
    let shown = message(cx, &f.view);
    assert!(shown.contains("Take 2 was recorded on its own"), "{shown}");
    assert_eq!(chosen(&f.dir), Vec::<Choice>::new());
    press(cx, f.window, &["shift-p"]);
    assert!(!f.view.read_with(cx, |view, _| view.recorder.is_playing()));

    press(cx, f.window, &["left", "shift-enter"]);
    let by = || "narrator".to_owned();
    assert_eq!(
        chosen(&f.dir),
        vec![
            ("a".to_owned(), 1, by()),
            ("b".to_owned(), 1, by()),
            ("c".to_owned(), 1, by())
        ]
    );
    let shown = message(cx, &f.view);
    assert_eq!(shown, "Chose the passage for 3 lines (a to c).");
    // Enter still chooses one take.
    press(cx, f.window, &["right", "enter"]);
    assert_eq!(chosen(&f.dir).last(), Some(&("b".to_owned(), 2, by())));
}

/// Stops a passage take while `fail` makes publishing it fail: the audio is
/// kept (locked, so a repair leaves it alone) and published once saving
/// works again, with its takes cut exactly once.
fn a_passage_that_cannot_be_published_is_kept(cx: &mut TestAppContext, fail: impl Fn(bool)) {
    let f = open_passages(cx);
    read_from_a(cx, &f, "shift-space", 11.3);
    at_film(cx, &f, 11.25);
    fail(true);
    at_film(cx, &f, 11.35);
    assert!(!recording(cx, &f));
    let shown = message(cx, &f.view);
    assert!(shown.contains("The passage could not be saved"), "{shown}");
    assert!(shown.contains("kept in memory"), "{shown}");
    assert!(f.view.read_with(cx, |view, _| view.recorder.has_unsaved()));

    let report = tw::repair(&f.dir).expect("repair");
    assert!(report.passages_published.is_empty(), "{report:?}");
    assert!(report.pieces.is_empty(), "{report:?}");
    assert!(report.passages_failed.is_empty(), "{report:?}");
    assert_eq!(passage_files(&f.dir, ".wav"), Vec::<PathBuf>::new());
    assert_eq!(recorded(&f.dir).len(), 0);

    fail(false);
    f.media.set_now(host(20.0));
    cut(cx, &f);
    assert_eq!(recorded_takes(&f.dir), takes_of(&["a", "b", "c"]));
    assert!(recorded(&f.dir).iter().all(|(_, _, rec)| !rec.recovered));
    let wavs = passage_files(&f.dir, ".wav");
    assert_eq!(wavs.len(), 1);
    let (samples, _) = tw::read_take_wav(&wavs[0]).expect("the passage");
    assert_eq!(samples.len(), 107 * 4800);
    assert_eq!(passage_files(&f.dir, ".partial"), Vec::<PathBuf>::new());
    let shown = message(cx, &f.view);
    assert!(
        shown.contains("Recorded a passage of 3 lines (a to c)."),
        "{shown}"
    );
    assert!(shown.contains("saved now"), "{shown}");
    f.media.set_now(host(30.0));
    frame(cx, f.window);
    assert_eq!(recorded(&f.dir).len(), 3, "cut exactly once");
}

#[gpui_kit::test]
fn a_passage_that_cannot_be_published_is_kept_and_published_later(cx: &mut TestAppContext) {
    a_passage_that_cannot_be_published_is_kept(cx, super::super::recorder::commits_fail);
}

#[gpui_kit::test]
fn a_passage_whose_publish_failed_after_renaming_its_sidecar_is_finished(cx: &mut TestAppContext) {
    a_passage_that_cannot_be_published_is_kept(cx, |fail| {
        tw::takes::fail_commits_after_renames(fail.then_some(1));
    });
}

#[gpui_kit::test]
fn a_passage_that_cannot_be_begun_on_disk_is_written_when_it_stops(cx: &mut TestAppContext) {
    let f = open_passages(cx);
    // A file where the passages' directory belongs.
    fs::create_dir_all(f.dir.join("takes")).expect("takes");
    fs::write(f.dir.join("takes/.passages"), b"").expect("blocker");
    read_from_a(cx, &f, "shift-space", 7.5);
    let shown = message(cx, &f.view);
    assert!(shown.contains("kept in memory"), "{shown}");
    assert!(recording(cx, &f));
    fs::remove_file(f.dir.join("takes/.passages")).expect("unblock");
    f.media.set_now(host(7.5));
    press(cx, f.window, &["space"]);
    cut(cx, &f);
    assert_eq!(recorded_takes(&f.dir), takes_of(&["a", "b"]));
    let wavs = passage_files(&f.dir, ".wav");
    let (samples, _) = tw::read_take_wav(&wavs[0]).expect("the passage");
    assert_eq!(samples.len(), 69 * 4800, "written from memory");
}

#[gpui_kit::test]
fn the_window_stays_open_while_a_passage_is_only_in_memory(cx: &mut TestAppContext) {
    let f = open_passages(cx);
    // A file where the passages' directory belongs: the passage can be
    // neither begun nor published.
    fs::create_dir_all(f.dir.join("takes")).expect("takes");
    fs::write(f.dir.join("takes/.passages"), b"").expect("blocker");
    read_from_a(cx, &f, "shift-space", 7.5);
    f.media.set_now(host(7.5));
    press(cx, f.window, &["space"]);
    assert!(!recording(cx, &f));

    assert!(!VisualTestContext::from_window(f.window, cx).simulate_close());
    let shown = message(cx, &f.view);
    assert!(
        shown.starts_with("Not closing: 1 passage is only in memory"),
        "{shown}"
    );

    fs::remove_file(f.dir.join("takes/.passages")).expect("unblock");
    f.media.set_now(host(20.0));
    cut(cx, &f);
    assert_eq!(recorded_takes(&f.dir), takes_of(&["a", "b"]));
    assert!(VisualTestContext::from_window(f.window, cx).simulate_close());
}

#[gpui_kit::test]
fn closing_mid_passage_keeps_it_and_cuts_its_takes(cx: &mut TestAppContext) {
    let f = open_passages(cx);
    read_from_a(cx, &f, "shift-space", 7.5);
    at_film(cx, &f, 7.5);
    assert!(VisualTestContext::from_window(f.window, cx).simulate_close());
    // The takes are there when the window is gone: nothing waits for a
    // repair.
    assert_eq!(recorded_takes(&f.dir), takes_of(&["a", "b"]));
    assert_eq!(passage_files(&f.dir, ".wav").len(), 1);
    assert_eq!(passage_files(&f.dir, ".partial"), Vec::<PathBuf>::new());
}

#[gpui_kit::test]
fn a_capture_failure_stops_a_passage_and_keeps_what_was_read(cx: &mut TestAppContext) {
    let f = open_passages(cx);
    read_from_a(cx, &f, "shift-space", 7.5);
    f.media.0.borrow_mut().capture_error = Some("the device was unplugged".to_owned());
    at_film(cx, &f, 7.5);
    assert!(!recording(cx, &f));
    cut(cx, &f);
    assert_eq!(recorded_takes(&f.dir), takes_of(&["a", "b"]));
    let shown = message(cx, &f.view);
    assert!(
        shown.contains("Recorded a passage of 2 lines (a to b). No take for c"),
        "{shown}"
    );
    assert!(
        shown.contains("Recording stopped: the device was unplugged"),
        "{shown}"
    );
}

#[gpui_kit::test]
fn m_is_off_during_a_passage_take(cx: &mut TestAppContext) {
    let f = open_passages(cx);
    read_from_a(cx, &f, "shift-space", 0.7);
    f.media.set_now(host(5.6)); // b is being read
    press(cx, f.window, &["m"]);
    assert_eq!(start_of(&f.dir, "a"), 3.0);
    assert_eq!(start_of(&f.dir, "b"), 5.4);
    let shown = message(cx, &f.view);
    assert!(shown.contains("M is off during a passage take"), "{shown}");
    assert!(recording(cx, &f));
    press(cx, f.window, &["space"]);
    cut(cx, &f);
}

#[gpui_kit::test]
fn a_late_film_start_moves_the_passage_on_disk(cx: &mut TestAppContext) {
    let f = open_passages(cx);
    read_from_a(cx, &f, "shift-space", 0.7); // scheduled: film 1.0 at host 100.4
    let first_film_s = |path: &Path| {
        let sidecar: serde_json::Value =
            serde_json::from_slice(&fs::read(path).expect("sidecar")).expect("json");
        assert_eq!(sidecar["kind"], "passage");
        sidecar["first_sample_film_s"].as_f64().expect("placed")
    };
    let partial = passage_files(&f.dir, ".json.partial").remove(0);
    assert!(close(first_film_s(&partial), 0.6));
    // The player really starts 40 ms late: a crash from here on still cuts
    // the passage where it was recorded.
    f.media.0.borrow_mut().timebase = Some(Timebase {
        rate: 1.0,
        film_anchor_s: 1.0,
        host_anchor_s: 100.44,
    });
    f.media.set_now(100.5);
    frame(cx, f.window);
    assert!(close(first_film_s(&partial), 0.56));
    f.media.set_now(100.6);
    press(cx, f.window, &["space"]);
    cut(cx, &f);
    let published = passage_files(&f.dir, ".json").remove(0);
    assert!(close(first_film_s(&published), 0.56));
    let takes = recorded(&f.dir);
    assert!(close(takes[0].2.line_offset_s, 0.56 - 3.0));
}

#[gpui_kit::test]
fn opening_the_window_finishes_a_crashed_passage(cx: &mut TestAppContext) {
    let f = open_with(cx, &[(DEVICE, 48_000)], |dir| {
        fs::write(dir.join("script.toml"), PASSAGES).expect("script");
        let project = Project::open(dir).expect("project");
        let input = tw::PassageInput {
            sample_rate: 48_000,
            device: DEVICE.to_owned(),
            channel: 1,
            latency: tw::passage::Latency {
                input_s: LATENCY_S,
                offset_ms: 0.0,
            },
            preroll_s: 2.0,
            lines: ["a", "b", "c"]
                .iter()
                .map(|id| tw::PassageLine {
                    id: (*id).to_owned(),
                    snapshot: project.snapshot(id).expect("snapshot"),
                })
                .collect(),
            first_sample_film_s: Some(FIRST_FILM_S),
            dropouts: None,
            film_shift_ms: None,
        };
        let partial = tw::begin_passage(dir, &input).expect("begin");
        let mut wav = tw::PartialWav::create(&partial.wav, 48_000).expect("wav");
        // a and b were read (film 0.6 to 7.5) when the process died: no
        // finish, no publish.
        for k in 0..69 {
            let film_s = FIRST_FILM_S + 0.1 * f64::from(k) + 0.05;
            wav.write(&vec![heard(film_s); 4800]).expect("write");
        }
        wav.flush().expect("flush");
    });
    assert_eq!(recorded_takes(&f.dir), takes_of(&["a", "b"]));
    assert!(recorded(&f.dir).iter().all(|(_, _, rec)| rec.recovered));
    assert_eq!(passage_files(&f.dir, ".wav").len(), 1);
    let shown = message(cx, &f.view);
    assert!(
        shown.contains("Recovered a take 1, b take 1 from an interrupted recording."),
        "{shown}"
    );
    f.view.read_with(cx, |view, _| {
        assert_eq!(view.line_state().map(|line| line.takes.len()), Some(1));
        assert_eq!(view.take, Some(1));
    });
}
