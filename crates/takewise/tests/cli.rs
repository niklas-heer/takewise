//! End-to-end tests of the `takewise` binary.

// Test helpers fail loudly; clippy's in-test allowances cover only #[test] fns.
#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::assert::OutputAssertExt as _;
use predicates::prelude::*;
use serde_json::Value;
use takewise_core::{Project, RecordedInput, TakeRef, begin_take, commit_take, write_take_wav};

const RATE: u32 = 48_000;

fn takewise() -> Command {
    Command::new(env!("CARGO_BIN_EXE_takewise"))
}

/// A 51.5 s WAV that ffprobe reads as the film.
fn film(dir: &Path) -> PathBuf {
    let path = dir.join("film.wav");
    write_take_wav(&path, &vec![0; 8_000 * 103 / 2], 8_000).unwrap();
    path
}

fn init(tmp: &Path) -> PathBuf {
    let dir = tmp.join("promo.takewise");
    takewise()
        .arg("init")
        .arg(&dir)
        .arg("--film")
        .arg(film(tmp))
        .assert()
        .success();
    dir
}

fn json(cmd: &mut Command) -> (i32, Value) {
    let output = cmd.arg("--json").output().unwrap();
    let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "stdout is not JSON ({err}): {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.code().unwrap(), value)
}

fn status(dir: &Path) -> Value {
    let (code, value) = json(takewise().arg("status").arg(dir));
    assert_eq!(code, 0, "{value}");
    value["data"].clone()
}

const SCRIPT: &str = r#"# narration
[[line]]
id = "hook"
start = 10.0
text = "Hello there."
"#;

/// Records a take with a 440 Hz burst from 2 s to 3 s into it.
fn record_take(dir: &Path, line: &str) -> TakeRef {
    let project = Project::open(dir).unwrap();
    let input = RecordedInput {
        sample_rate: RATE,
        device: "Test Mic".to_owned(),
        channel: 1,
        line_offset_s: -2.0,
        snapshot: project.snapshot(line).unwrap(),
    };
    let partial = begin_take(dir, line, &input).unwrap();
    let mut audio = vec![0i32; 5 * RATE as usize];
    for (i, sample) in audio
        .iter_mut()
        .enumerate()
        .skip(2 * RATE as usize)
        .take(RATE as usize)
    {
        let t = i as f64 / f64::from(RATE);
        *sample = ((t * 440.0 * std::f64::consts::TAU).sin() * 4_000_000.0) as i32;
    }
    write_take_wav(&partial.wav, &audio, RATE).unwrap();
    commit_take(dir, partial).unwrap()
}

#[test]
fn version_prints() {
    takewise()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::starts_with("takewise "));
}

#[test]
fn init_status_roundtrip_json() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("promo.takewise");
    let (code, value) = json(
        takewise()
            .arg("init")
            .arg(&dir)
            .arg("--film")
            .arg(film(tmp.path())),
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"]["film_info"]["duration_s"], 51.5);
    assert!(dir.join("project.toml").is_file());

    let data = status(&dir);
    assert_eq!(data["lines"], Value::Array(Vec::new()));
    assert_eq!(data["problems"], Value::Array(Vec::new()), "{data}");
    assert_eq!(data["film"]["changed"], false);
    assert_eq!(data["script_version"].as_str().unwrap().len(), 64);

    let (code, value) = json(
        takewise()
            .arg("init")
            .arg(&dir)
            .arg("--film")
            .arg(film(tmp.path())),
    );
    assert_eq!(code, 2);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "already-exists");
}

#[test]
fn script_set_then_status() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::write(dir.join("script.toml"), SCRIPT).unwrap();
    let version = status(&dir)["script_version"].as_str().unwrap().to_owned();

    let (code, value) = json(takewise().args(["script", "set"]).arg(&dir).args([
        "hook",
        "--text",
        "Hello, there.",
        "--expect",
        &version,
    ]));
    assert_eq!(code, 0, "{value}");
    let data = status(&dir);
    assert_eq!(data["lines"][0]["id"], "hook");
    assert_eq!(data["lines"][0]["text"], "Hello, there.");
    assert!(
        fs::read_to_string(dir.join("script.toml"))
            .unwrap()
            .starts_with("# narration\n")
    );

    // The version seen before the edit is stale now.
    let (code, value) = json(
        takewise()
            .args(["script", "set"])
            .arg(&dir)
            .args(["hook", "--start", "11", "--expect", &version]),
    );
    assert_eq!(code, 2);
    assert_eq!(value["error"]["code"], "script-changed");

    let (code, value) = json(
        takewise()
            .args(["script", "set"])
            .arg(&dir)
            .args(["hook", "--start", "60"]),
    );
    assert_eq!(code, 2);
    assert_eq!(value["error"]["code"], "invalid-script");

    // Exactly one field per call.
    takewise()
        .args(["script", "set"])
        .arg(&dir)
        .args(["hook", "--text", "a", "--start", "1"])
        .assert()
        .code(2);
}

#[test]
fn choose_then_export_writes_wav_and_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::write(dir.join("script.toml"), SCRIPT).unwrap();
    let take = record_take(&dir, "hook");
    assert_eq!(take.take, 1);

    let (code, value) = json(
        takewise()
            .arg("choose")
            .arg(&dir)
            .args(["hook", "1", "--by", "claude"]),
    );
    assert_eq!(code, 0, "{value}");
    let data = status(&dir);
    assert_eq!(data["lines"][0]["chosen"]["take"], 1);
    assert_eq!(data["lines"][0]["chosen"]["by"], "claude");

    let (code, value) = json(takewise().arg("export").arg(&dir));
    assert_eq!(code, 0, "{value}");
    assert!(dir.join("export/narration.wav").is_file());
    let manifest: Value =
        serde_json::from_slice(&fs::read(dir.join("export/manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["lines"][0]["id"], "hook");
    assert_eq!(manifest["lines"][0]["chosen_by"], "claude");
    assert_eq!(value["data"]["lines"][0]["take_file"], "takes/hook/001.wav");

    takewise()
        .arg("choose")
        .arg(&dir)
        .args(["hook", "7"])
        .assert()
        .code(2);
    takewise()
        .arg("choose")
        .arg(&dir)
        .args(["nope", "1"])
        .assert()
        .code(2);
}

#[test]
fn export_without_choice_exits_1_with_line_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::write(dir.join("script.toml"), SCRIPT).unwrap();
    record_take(&dir, "hook");
    let (code, value) = json(takewise().arg("export").arg(&dir));
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "export-refused");
    assert_eq!(value["data"]["refused"][0]["code"], "no-choice");
    assert_eq!(value["data"]["refused"][0]["lines"][0], "hook");

    takewise()
        .arg("export")
        .arg(&dir)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("hook"));

    let (code, value) = json(takewise().arg("export").arg(&dir).arg("--use-proposed"));
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["data"]["lines"][0]["chosen_by"], "proposed");
}

#[test]
fn judge_rejects_bad_score_exit_2() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::write(dir.join("script.toml"), SCRIPT).unwrap();
    record_take(&dir, "hook");
    let judge = |extra: &[&str]| {
        let mut cmd = takewise();
        cmd.arg("judge")
            .arg(&dir)
            .args(["hook", "1", "--reason", "clear", "--by", "claude"])
            .args(extra);
        cmd
    };
    judge(&["--verdict", "keep", "--score", "1.5"])
        .assert()
        .code(2);
    judge(&["--verdict", "keep", "--score", "-0.1"])
        .assert()
        .code(2);
    judge(&["--verdict", "great"]).assert().code(2);

    let (code, value) = json(&mut judge(&["--verdict", "keep", "--score", "0.8"]));
    assert_eq!(code, 0, "{value}");
    let take = &status(&dir)["lines"][0]["takes"][0];
    assert_eq!(take["effective"], "keep");
    assert_eq!(take["verdict_source"], "judge:claude");
}

#[test]
fn status_strict_exit_codes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    takewise()
        .arg("status")
        .arg(&dir)
        .arg("--strict")
        .assert()
        .code(0);

    fs::write(dir.join("script.toml"), SCRIPT).unwrap();
    let (code, value) = json(takewise().arg("status").arg(&dir).arg("--strict"));
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["error"]["code"], "strict");
    assert_eq!(value["data"]["lines"][0]["id"], "hook");

    record_take(&dir, "hook");
    takewise()
        .arg("choose")
        .arg(&dir)
        .args(["hook", "1"])
        .assert()
        .success();
    takewise()
        .arg("status")
        .arg(&dir)
        .arg("--strict")
        .assert()
        .code(0);

    // A leftover partial is a problem until someone looks at it.
    let project = Project::open(&dir).unwrap();
    let input = RecordedInput {
        sample_rate: RATE,
        device: "Test Mic".to_owned(),
        channel: 1,
        line_offset_s: -2.0,
        snapshot: project.snapshot("hook").unwrap(),
    };
    begin_take(&dir, "hook", &input).unwrap();
    takewise()
        .arg("status")
        .arg(&dir)
        .arg("--strict")
        .assert()
        .code(1);
}

#[test]
fn status_repair_adopts_and_reports() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::write(dir.join("script.toml"), SCRIPT).unwrap();
    record_take(&dir, "hook");
    // Simulate a crash after the renames: a second take without its event.
    fs::copy(
        dir.join("takes/hook/001.wav"),
        dir.join("takes/hook/002.wav"),
    )
    .unwrap();
    fs::copy(
        dir.join("takes/hook/001.json"),
        dir.join("takes/hook/002.json"),
    )
    .unwrap();
    assert_eq!(status(&dir)["unadopted"][0], "takes/hook/002.wav");

    let (code, value) = json(takewise().arg("status").arg(&dir).arg("--repair"));
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["data"]["repair"]["adopted"][0]["take"], 2);
    assert_eq!(
        value["data"]["lines"][0]["takes"].as_array().unwrap().len(),
        2
    );
}

#[test]
fn status_text_lists_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::write(dir.join("script.toml"), SCRIPT).unwrap();
    record_take(&dir, "hook");
    takewise()
        .arg("status")
        .arg(&dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("hook").and(predicate::str::contains("proposed 1")));
}

#[test]
fn unknown_project_exit_2() {
    let tmp = tempfile::tempdir().unwrap();
    let (code, value) = json(takewise().arg("status").arg(tmp.path().join("missing")));
    assert_eq!(code, 2);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "not-a-project");
    takewise()
        .arg("status")
        .arg(tmp.path().join("missing"))
        .assert()
        .code(2);
    takewise().arg("frobnicate").assert().code(2);
}

#[test]
fn record_and_check_are_stubs_exit_2() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    takewise()
        .arg("record")
        .arg(&dir)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("milestone 2"));
    let (code, value) = json(takewise().arg("check").arg(&dir));
    assert_eq!(code, 2);
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("milestone 3")
    );
}

#[test]
fn doctor_reports_checks() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    let (code, value) = json(
        takewise()
            .arg("doctor")
            .arg(&dir)
            .env("TAKEWISE_MODELS", tmp.path().join("no-models")),
    );
    assert_eq!(code, 1, "the model is missing: {value}");
    let checks = value["data"]["checks"].as_array().unwrap();
    let names: Vec<&str> = checks.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert!(
        names.contains(&"ffprobe") && names.contains(&"whisper-model") && names.contains(&"film"),
        "{names:?}"
    );
    let film = checks.iter().find(|c| c["name"] == "film").unwrap();
    assert_eq!(film["ok"], true, "{film}");
}
