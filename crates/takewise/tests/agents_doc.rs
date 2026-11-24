//! Keeps `docs/agents.md` true: runs every command of its `sh` blocks, in
//! order, against a scratch project, writes the files it shows, and checks
//! that its JSON examples have the shape of the real output.
//!
//! The guide's conventions, which this test relies on:
//! - each line of a `sh` block is one command (blank lines and `#` comments
//!   are skipped); `takewise record` is the narrator's part, so the test
//!   records in its place: all lines as one passage (take 1 of each line),
//!   then one more take per line;
//! - the passage id the guide shows ([`GUIDE_PASSAGE`]) stands for the id
//!   of the passage the test recorded;
//! - a block preceded by `<!-- file: <path> -->` is written to that path;
//! - a `json` block preceded by `<!-- output of: <command> -->` is the
//!   output of that command (any command printing JSON, such as `cat` of
//!   the manifest) as the guide ran it last; every key it shows must be in
//!   the real output (arrays by their first element).
//!
//! The tools are real `ffmpeg` and `ffprobe` and a fake `whisper-cli` that
//! hears "Hello there." in every take; the model is a sparse file of the
//! pinned size, so `models install` finds it installed.

// Test helpers fail loudly; clippy's in-test allowances cover only #[test] fns.
#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use takewise_core::passage::{Latency, split};
use takewise_core::{
    PassageInput, PassageLine, Project, RecordedInput, begin_passage, begin_take, commit_take,
    publish_passage, write_take_wav,
};

const RATE: u32 = 48_000;

/// The passage id in the guide's commands and examples.
const GUIDE_PASSAGE: &str = "0d9c5a0e-6a0c-4e52-9d33-3c1f5a2b7e10";

enum Item {
    /// One command line of a `sh` block, with its line in the guide.
    Command(String, usize),
    File(String, String),
    Shape(String, String, usize),
}

/// The guide's commands, files and JSON examples, in order.
fn parse(guide: &str) -> Vec<Item> {
    let mut items = Vec::new();
    let mut lines = guide.lines().enumerate().peekable();
    let mut pending: Option<String> = None;
    while let Some((n, line)) = lines.next() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed
            .strip_prefix("<!--")
            .and_then(|r| r.strip_suffix("-->"))
        {
            pending = Some(rest.trim().to_owned());
            continue;
        }
        let Some(lang) = trimmed.strip_prefix("```") else {
            if !trimmed.is_empty() {
                pending = None;
            }
            continue;
        };
        let mut body = Vec::new();
        for (_, line) in lines.by_ref() {
            if line.trim_start().starts_with("```") {
                break;
            }
            body.push(line);
        }
        let marker = pending.take();
        let text = body.join("\n") + "\n";
        match (lang, marker.as_deref()) {
            (_, Some(m)) if m.starts_with("file:") => {
                items.push(Item::File(m["file:".len()..].trim().to_owned(), text));
            }
            ("json", Some(m)) if m.starts_with("output of:") => {
                items.push(Item::Shape(
                    m["output of:".len()..].trim().to_owned(),
                    text,
                    n + 1,
                ));
            }
            ("sh", _) => {
                for (i, line) in body.iter().enumerate() {
                    let command = line.trim();
                    if !command.is_empty() && !command.starts_with('#') {
                        items.push(Item::Command(command.to_owned(), n + 2 + i));
                    }
                }
            }
            _ => {}
        }
    }
    items
}

/// Writes an executable shell script.
fn script(dir: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The directory of a real tool, from the places Homebrew installs them.
fn tool_dir(tool: &str) -> PathBuf {
    ["/opt/homebrew/bin", "/usr/local/bin"]
        .iter()
        .map(PathBuf::from)
        .find(|dir| dir.join(tool).is_file())
        .unwrap_or_else(|| panic!("the guide test needs {tool} (brew install ffmpeg)"))
}

/// Fills `audio` with a 440 Hz tone for one second from sample `from`.
fn burst(audio: &mut [i32], from: usize) {
    for (i, sample) in audio.iter_mut().enumerate().skip(from).take(RATE as usize) {
        let t = i as f64 / f64::from(RATE);
        *sample = ((t * 440.0 * std::f64::consts::TAU).sin() * 4_000_000.0) as i32;
    }
}

/// The narrator reading every line in one go: a passage from 2 s before the
/// first line to 2 s after the last line's start, each line spoken for a
/// second from its start. Cut into takes; returns the passage's id.
#[allow(clippy::cast_sign_loss)]
fn record_passage(dir: &Path) -> String {
    let project = Project::open(dir).unwrap();
    let lines = &project.script.lines;
    let first_s = lines[0].start - 2.0;
    let seconds = lines[lines.len() - 1].start + 2.0 - first_s;
    let sample = |seconds: f64| (seconds * f64::from(RATE)).round().max(0.0) as usize;
    let mut audio = vec![0i32; sample(seconds)];
    for line in lines {
        burst(&mut audio, sample(line.start - first_s));
    }
    let input = PassageInput {
        sample_rate: RATE,
        device: "Test Mic".to_owned(),
        channel: 1,
        latency: Latency {
            input_s: 0.0,
            offset_ms: 0.0,
        },
        preroll_s: project.config.record.preroll_s,
        lines: lines
            .iter()
            .map(|line| PassageLine {
                id: line.id.clone(),
                snapshot: project.snapshot(&line.id).unwrap(),
            })
            .collect(),
        first_sample_film_s: Some(first_s),
        dropouts: Some(Vec::new()),
        film_shift_ms: None,
    };
    let partial = begin_passage(dir, &input).unwrap();
    write_take_wav(&partial.wav, &audio, RATE).unwrap();
    let passage = publish_passage(dir, &partial).unwrap();
    let takes = split(dir, &passage.id).unwrap().takes;
    assert_eq!(takes.len(), lines.len(), "one take per line");
    passage.id
}

/// The narrator's part: the whole script as one passage, then one take per
/// line, a 440 Hz burst 2 s into it, the take starting 2 s before the line.
/// Returns the passage's id.
fn record_takes(dir: &Path) -> String {
    let passage = record_passage(dir);
    let project = Project::open(dir).unwrap();
    for line in &project.script.lines {
        let input = RecordedInput {
            sample_rate: RATE,
            device: "Test Mic".to_owned(),
            channel: 1,
            line_offset_s: -2.0,
            snapshot: project.snapshot(&line.id).unwrap(),
            dropouts: None,
            film_shift_ms: None,
            passage: None,
        };
        let partial = begin_take(dir, &line.id, &input).unwrap();
        let mut audio = vec![0i32; 5 * RATE as usize];
        burst(&mut audio, 2 * RATE as usize);
        write_take_wav(&partial.wav, &audio, RATE).unwrap();
        commit_take(dir, partial).unwrap();
    }
    passage
}

/// Whether every key `example` shows is in `real`, at the same place.
fn same_shape(example: &Value, real: &Value, at: &str) -> Result<(), String> {
    match (example, real) {
        (Value::Object(want), Value::Object(have)) => {
            for (key, value) in want {
                let Some(found) = have.get(key) else {
                    return Err(format!("{at}.{key} is missing"));
                };
                same_shape(value, found, &format!("{at}.{key}"))?;
            }
            Ok(())
        }
        (Value::Array(want), Value::Array(have)) => match (want.first(), have.first()) {
            (Some(w), Some(h)) => same_shape(w, h, &format!("{at}[0]")),
            (Some(_), None) => Err(format!("{at} is empty")),
            _ => Ok(()),
        },
        (Value::Object(_) | Value::Array(_), other) => {
            Err(format!("{at} is {other}, not an object or array"))
        }
        _ => Ok(()),
    }
}

/// The scratch directory the guide runs in, with its tools and model.
struct Scratch {
    tmp: tempfile::TempDir,
    work: PathBuf,
    models: PathBuf,
    /// `PATH` for the commands.
    path: String,
}

fn scratch() -> Scratch {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    let bin = tmp.path().join("bin");
    let models = tmp.path().join("models");
    for dir in [&work, &bin, &models] {
        fs::create_dir_all(dir).unwrap();
    }
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_takewise"), bin.join("takewise")).unwrap();
    let json = r#"{"transcription":[{"tokens":[
        {"text":" Hello","offsets":{"from":2100,"to":2400},"p":0.9,"t_dtw":210},
        {"text":" there","offsets":{"from":2500,"to":2800},"p":0.9,"t_dtw":250},
        {"text":".","offsets":{"from":2800,"to":2900},"p":0.9,"t_dtw":280}]}]}"#;
    script(
        &bin,
        "whisper-cli",
        &format!(
            "while [ $# -gt 0 ]; do if [ \"$1\" = -of ]; then out=$2; fi; shift; done\n\
             cat > \"$out.json\" <<'EOF'\n{json}\nEOF"
        ),
    );
    fs::File::create(models.join("ggml-large-v3-turbo.bin"))
        .unwrap()
        .set_len(takewise_core::models::PINNED.size)
        .unwrap();
    let ffmpeg = tool_dir("ffmpeg");
    let ffprobe = tool_dir("ffprobe");
    let path = format!(
        "{}:{}:{}:/usr/bin:/bin",
        bin.display(),
        ffmpeg.display(),
        ffprobe.display()
    );
    // The film: 51.5 s of silence that ffprobe reads, wherever the guide
    // says it is.
    let film = work.join("renders/promo.mp4");
    fs::create_dir_all(film.parent().unwrap()).unwrap();
    write_take_wav(&film, &vec![0; 8_000 * 103 / 2], 8_000).unwrap();

    Scratch {
        tmp,
        work,
        models,
        path,
    }
}

#[test]
fn every_command_in_the_agent_guide_runs() {
    let guide_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/agents.md");
    let guide = fs::read_to_string(&guide_path).unwrap();
    let items = parse(&guide);
    assert!(
        items
            .iter()
            .filter(|i| matches!(i, Item::Command(..)))
            .count()
            >= 8,
        "the guide shows too few commands"
    );

    let scratch = scratch();
    let (tmp, work, models, path) = (&scratch.tmp, &scratch.work, &scratch.models, &scratch.path);
    let mut outputs: HashMap<String, Value> = HashMap::new();
    let mut passage: Option<String> = None;
    for item in &items {
        match item {
            Item::File(path, text) => fs::write(work.join(path), text).unwrap(),
            Item::Command(command, line) if command.starts_with("takewise record ") => {
                let dir = command.split_whitespace().nth(2).unwrap();
                passage = Some(record_takes(&work.join(dir)));
                assert!(command.split_whitespace().count() == 3, "line {line}");
            }
            Item::Command(command, line) => {
                // The guide's passage id stands for the recorded passage's.
                let run = match &passage {
                    Some(id) => command.replace(GUIDE_PASSAGE, id),
                    None => command.clone(),
                };
                assert!(
                    passage.is_some() || !command.contains(GUIDE_PASSAGE),
                    "line {line}: the guide names a passage before one is recorded"
                );
                let output = Command::new("/bin/sh")
                    .arg("-c")
                    .arg(&run)
                    .current_dir(work)
                    .env_clear()
                    .env("PATH", path)
                    .env("HOME", tmp.path())
                    .env("TAKEWISE_MODELS", models)
                    .env("TAKEWISE_SETTINGS", tmp.path().join("no-settings.toml"))
                    .env("TAKEWISE_APP", tmp.path().join("Nowhere.app"))
                    .output()
                    .unwrap();
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                if std::env::var_os("TAKEWISE_GUIDE_DUMP").is_some() {
                    println!("$ {command}\n{stdout}{stderr}");
                }
                let code = output.status.code().unwrap();
                // `doctor` reports findings on a test machine (no input
                // chosen, no Takewise.app); everything else succeeds.
                let allowed: &[i32] = if command.contains(" doctor") {
                    &[0, 1]
                } else {
                    &[0]
                };
                assert!(
                    allowed.contains(&code),
                    "docs/agents.md line {line}: `{command}` exited {code}\n{stdout}{stderr}"
                );
                match serde_json::from_str::<Value>(&stdout) {
                    Ok(value) => {
                        outputs.insert(command.clone(), value);
                    }
                    Err(err) => assert!(
                        !command.contains("--json"),
                        "line {line}: `{command}` printed no JSON ({err}): {stdout}"
                    ),
                }
            }
            Item::Shape(command, example, line) => {
                let real = outputs.get(command).unwrap_or_else(|| {
                    panic!("line {line}: the guide shows output of `{command}` before running it")
                });
                let example: Value = serde_json::from_str(example)
                    .unwrap_or_else(|err| panic!("line {line}: the example is not JSON: {err}"));
                if let Err(drift) = same_shape(&example, real, "") {
                    panic!("line {line}: output of `{command}`: {drift}\nreal: {real:#}");
                }
            }
        }
    }
}

#[test]
fn shapes_compare_keys_and_first_elements() {
    let real: Value =
        serde_json::json!({"ok": true, "data": {"lines": [{"id": "a", "takes": []}]}});
    assert!(
        same_shape(
            &serde_json::json!({"data": {"lines": [{"id": "x"}]}}),
            &real,
            ""
        )
        .is_ok()
    );
    assert!(same_shape(&serde_json::json!({"data": {"nope": 1}}), &real, "").is_err());
    assert!(
        same_shape(
            &serde_json::json!({"data": {"lines": [{"takes": [{"take": 1}]}]}}),
            &real,
            ""
        )
        .is_err()
    );
}
