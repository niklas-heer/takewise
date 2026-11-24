//! End-to-end tests of the `takewise` binary.

// Test helpers fail loudly; clippy's in-test allowances cover only #[test] fns.
#![allow(clippy::unwrap_used, clippy::cast_possible_truncation)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::assert::OutputAssertExt as _;
use predicates::prelude::*;
use serde_json::Value;
use takewise_core::passage::Latency;
use takewise_core::{
    PartialPassage, PartialWav, PassageInput, PassageLine, Project, RecordedInput, TakeRef,
    begin_passage, begin_take, commit_take, publish_passage, write_take_wav,
};

const RATE: u32 = 48_000;

/// The CLI, with the app's defaults read from a file that does not exist
/// (tests never see the user's own device choices) and no Takewise.app to
/// start (`doctor` asks it for the microphone permission).
fn takewise() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_takewise"));
    cmd.env("TAKEWISE_SETTINGS", "/nonexistent/takewise-settings.toml");
    cmd.env("TAKEWISE_APP", NO_APP);
    cmd
}

const NO_APP: &str = "/nonexistent/Takewise.app";

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
        dropouts: None,
        film_shift_ms: None,
        passage: None,
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

    // New projects export clean; raw needs no ffmpeg.
    let (code, value) = json(
        takewise()
            .arg("export")
            .arg(&dir)
            .args(["--processing", "raw"]),
    );
    assert_eq!(code, 0, "{value}");
    assert!(dir.join("export/narration.wav").is_file());
    let manifest: Value =
        serde_json::from_slice(&fs::read(dir.join("export/manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["lines"][0]["id"], "hook");
    assert_eq!(manifest["lines"][0]["chosen_by"], "claude");
    assert_eq!(value["data"]["lines"][0]["take_file"], "takes/hook/001.wav");
    assert_eq!(value["data"]["warnings"], Value::Array(Vec::new()));

    // After a text edit the take still exports, with a warning.
    takewise()
        .args(["script", "set"])
        .arg(&dir)
        .args(["hook", "--text", "Hi there."])
        .assert()
        .success();
    let (code, value) = json(
        takewise()
            .arg("export")
            .arg(&dir)
            .args(["--processing", "raw"]),
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(
        value["data"]["warnings"][0]["code"],
        "recorded-against-old-text"
    );
    takewise()
        .arg("export")
        .arg(&dir)
        .args(["--processing", "raw"])
        .assert()
        .success()
        .stdout(predicate::str::contains("recorded-against-old-text"));

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
fn export_refuses_a_changed_or_missing_film() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::write(dir.join("script.toml"), SCRIPT).unwrap();
    let take = record_take(&dir, "hook");
    takewise()
        .arg("choose")
        .arg(&dir)
        .args(["hook", &take.take.to_string()])
        .assert()
        .success();
    let raw = ["--processing", "raw"];
    let export = |dir: &Path| json(takewise().arg("export").arg(dir).args(raw));

    // A film one second longer: the timings may no longer fit.
    let film = tmp.path().join("film.wav");
    write_take_wav(&film, &vec![0; 8_000 * 105 / 2], 8_000).unwrap();
    let (code, value) = export(&dir);
    assert_eq!(code, 1, "{value}");
    assert_eq!(
        value["data"]["refused"][0]["code"], "film-changed",
        "{value}"
    );
    assert!(!dir.join("export/narration.wav").exists());
    takewise()
        .arg("export")
        .arg(&dir)
        .args(raw)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("film-changed"))
        .stderr(predicate::str::contains("--refresh-film"));

    // After --refresh-film it exports.
    takewise()
        .arg("status")
        .arg(&dir)
        .arg("--refresh-film")
        .assert()
        .success();
    let (code, value) = export(&dir);
    assert_eq!(code, 0, "{value}");

    fs::remove_file(&film).unwrap();
    let (code, value) = export(&dir);
    assert_eq!(code, 1, "{value}");
    assert_eq!(
        value["data"]["refused"][0]["code"], "film-missing",
        "{value}"
    );
    assert!(!dir.join("export/manifest.json").exists());
}

#[test]
fn export_flags_override_config() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    // A new project exports clean.
    let config = fs::read_to_string(dir.join("project.toml")).unwrap();
    assert!(config.contains("[export]"), "{config}");
    assert!(config.contains("processing = \"clean\""), "{config}");
    fs::write(dir.join("script.toml"), SCRIPT).unwrap();
    record_take(&dir, "hook");
    takewise()
        .arg("choose")
        .arg(&dir)
        .args(["hook", "1"])
        .assert()
        .success();
    let (code, value) = json(
        takewise()
            .arg("export")
            .arg(&dir)
            .args(["--processing", "raw", "--bit-depth", "16"])
            .args(["--format", "wav", "--sample-rate", "48000"]),
    );
    assert_eq!(code, 0, "{value}");
    let settings = &value["data"]["settings"];
    assert_eq!(settings["processing"], "raw", "{value}");
    assert_eq!(settings["bit_depth"], 16, "{value}");
    let manifest: Value =
        serde_json::from_slice(&fs::read(dir.join("export/manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["processing"], "raw");
    assert_eq!(manifest["bit_depth"], 16);
    // A 16-bit PCM WAV header: bits per sample at byte 34.
    let wav = fs::read(dir.join("export/narration.wav")).unwrap();
    assert_eq!(&wav[12..16], b"fmt ");
    assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16);
    // The config is not changed by the flags.
    let config = fs::read_to_string(dir.join("project.toml")).unwrap();
    assert!(config.contains("processing = \"clean\""), "{config}");
    // The configured clean export skips noise reduction for this silent
    // pre-roll, and says so in the text and the manifest's warnings.
    if takewise_core::ffmpeg::Ffmpeg::find().is_some() {
        takewise()
            .arg("export")
            .arg(&dir)
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "warning: hook: takes/hook/001.wav: noise reduction skipped: the take's pre-roll is digital silence (a gated input) (noise-reduction-skipped)",
            ));
        let manifest: Value =
            serde_json::from_slice(&fs::read(dir.join("export/manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["warnings"][0]["code"], "noise-reduction-skipped");
    }
    // Values outside the supported set are usage errors.
    let (code, _) = json(
        takewise()
            .arg("export")
            .arg(&dir)
            .args(["--sample-rate", "96000"]),
    );
    assert_eq!(code, 2);
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
fn status_warns_about_stale_calibration_and_missing_setup() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    let data = status(&dir);
    let codes: Vec<&str> = data["notices"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["code"].as_str())
        .collect();
    assert_eq!(codes, ["no-setup"], "{data}");

    // An offset measured before calibrations recorded their devices.
    takewise_core::set_latency_offset(&dir, 20).unwrap();
    let data = status(&dir);
    let stale = data["notices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["code"] == "calibration-stale")
        .expect("a stale calibration notice");
    assert!(
        stale["message"]
            .as_str()
            .unwrap()
            .contains("calibrate --write"),
        "{stale}"
    );
    takewise()
        .arg("status")
        .arg(&dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("calibration-stale"));
    // Notices do not fail --strict.
    takewise()
        .arg("status")
        .arg(&dir)
        .arg("--strict")
        .assert()
        .code(0);
}

#[test]
fn status_says_when_the_project_has_rescued_audio() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::write(dir.join("script.toml"), SCRIPT).unwrap();
    // The rescue folder lies beside the settings file.
    let settings = tmp.path().join("Takewise/settings.toml");
    let root = takewise_core::rescue::beside(&settings);
    let (_, before) = json(
        takewise()
            .env("TAKEWISE_SETTINGS", &settings)
            .arg("status")
            .arg(&dir),
    );
    assert!(!before.to_string().contains("rescued-audio"), "{before}");

    let at = "2026-11-24T10:15:30Z".parse().unwrap();
    let mut rescue = takewise_core::rescue::Rescue::create(&root, &dir, at).unwrap();
    let input = RecordedInput {
        sample_rate: RATE,
        device: "Test Mic".to_owned(),
        channel: 1,
        line_offset_s: -2.0,
        snapshot: Project::open(&dir).unwrap().snapshot("hook").unwrap(),
        dropouts: None,
        film_shift_ms: None,
        passage: None,
    };
    let wav = rescue.add_take("hook", &input, &vec![0; 4800]).unwrap();
    let folder = rescue.finish().unwrap();
    let (code, status) = json(
        takewise()
            .env("TAKEWISE_SETTINGS", &settings)
            .arg("status")
            .arg(&dir),
    );
    assert_eq!(code, 0, "{status}");
    let notices = status["data"]["notices"].as_array().unwrap();
    let rescued: Vec<&Value> = notices
        .iter()
        .filter(|notice| notice["code"] == "rescued-audio")
        .collect();
    assert_eq!(rescued.len(), 1, "{status}");
    let message = rescued[0]["message"].as_str().unwrap();
    assert!(
        message.starts_with(
            "1 take (hook) could not be saved into this project on 2026-11-24 and is kept in "
        ),
        "{message}"
    );
    assert!(message.contains(&folder.display().to_string()), "{message}");
    assert!(wav.is_file());
    // A notice, not a problem: `--strict` does not count it, and other
    // projects do not see it.
    takewise()
        .env("TAKEWISE_SETTINGS", &settings)
        .arg("status")
        .arg(&dir)
        .assert()
        .stdout(predicate::str::contains("rescued-audio"));
    let other = tmp.path().join("other");
    fs::create_dir_all(&other).unwrap();
    let other = init(&other);
    let (_, status) = json(
        takewise()
            .env("TAKEWISE_SETTINGS", &settings)
            .arg("status")
            .arg(&other),
    );
    assert!(!status.to_string().contains("rescued-audio"), "{status}");
}

#[test]
fn init_starts_with_the_saved_devices() {
    let tmp = tempfile::tempdir().unwrap();
    let settings = tmp.path().join("settings.toml");
    fs::write(
        &settings,
        "input = \"Duo Main\"\nchannel = 2\noutput = \"Duo Main\"\n",
    )
    .unwrap();
    let film = film(tmp.path());
    let dir = tmp.path().join("promo.takewise");
    takewise()
        .env("TAKEWISE_SETTINGS", &settings)
        .arg("init")
        .arg(&dir)
        .arg("--film")
        .arg(&film)
        .assert()
        .success()
        .stdout(predicate::str::contains("Duo Main"));
    let config = Project::open(&dir).unwrap().config;
    assert_eq!(config.input.device.as_deref(), Some("Duo Main"));
    assert_eq!(config.input.channel, 2);
    assert_eq!(config.output.device.as_deref(), Some("Duo Main"));
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

    // A leftover partial is a notice: reported, but it does not fail --strict.
    let project = Project::open(&dir).unwrap();
    let input = RecordedInput {
        sample_rate: RATE,
        device: "Test Mic".to_owned(),
        channel: 1,
        line_offset_s: -2.0,
        snapshot: project.snapshot("hook").unwrap(),
        dropouts: None,
        film_shift_ms: None,
        passage: None,
    };
    begin_take(&dir, "hook", &input).unwrap();
    let (code, value) = json(takewise().arg("status").arg(&dir).arg("--strict"));
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["data"]["problems"], Value::Array(Vec::new()));
    assert_eq!(value["data"]["notices"][0]["code"], "partial-take");
    takewise()
        .arg("status")
        .arg(&dir)
        .assert()
        .success()
        .stdout(predicate::str::contains("notices:").and(predicate::str::contains("partial-take")));

    // A torn ledger line is a notice too.
    let ledger = dir.join("takes.jsonl");
    let mut text = fs::read_to_string(&ledger).unwrap();
    text.push_str("{\"v\":1,");
    fs::write(&ledger, text).unwrap();
    takewise()
        .arg("status")
        .arg(&dir)
        .arg("--strict")
        .assert()
        .code(0);

    // A validation problem still fails --strict.
    fs::write(dir.join("script.toml"), SCRIPT.replace("10.0", "90.0")).unwrap();
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
fn record_without_bundle_explains() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    let nowhere = tmp.path().join("Nowhere.app");
    takewise()
        .arg("record")
        .arg(&dir)
        .env("TAKEWISE_APP", &nowhere)
        .assert()
        .code(2)
        .stderr(predicate::str::contains("mise run bundle"));
    let (code, value) = json(
        takewise()
            .arg("record")
            .arg(&dir)
            .env("TAKEWISE_APP", &nowhere),
    );
    assert_eq!(code, 2, "{value}");
    assert_eq!(value["error"]["code"], "setup", "{value}");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Nowhere.app"),
        "{value}"
    );
}

#[test]
fn record_refuses_an_invalid_script() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    std::fs::write(
        dir.join("script.toml"),
        "[[line]]\nid = \"Bad Id\"\nstart = 1.0\ntext = \"Hello.\"\n",
    )
    .unwrap();
    let (code, value) = json(
        takewise()
            .arg("record")
            .arg(&dir)
            .env("TAKEWISE_APP", tmp.path().join("Nowhere.app")),
    );
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["error"]["code"], "invalid-script", "{value}");
    assert_eq!(value["data"]["problems"][0]["code"], "bad-id", "{value}");
}

#[test]
fn record_refuses_a_missing_film() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::remove_file(tmp.path().join("film.wav")).unwrap();
    // A bundle that would open is not even looked for.
    let (code, value) = json(takewise().arg("record").arg(&dir));
    assert_eq!(code, 2, "{value}");
    assert_eq!(value["ok"], false, "{value}");
    assert_eq!(value["error"]["code"], "setup", "{value}");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("film.wav") && message.contains("does not exist"),
        "{value}"
    );
}

#[test]
fn window_writes_why_it_could_not_open() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::remove_file(tmp.path().join("film.wav")).unwrap();
    let result = tmp.path().join("window.json");
    takewise()
        .arg("window")
        .arg(&dir)
        .arg("--result")
        .arg(&result)
        .assert()
        .code(2);
    let reply: Value = serde_json::from_str(&fs::read_to_string(&result).unwrap()).unwrap();
    assert!(
        reply["error"]
            .as_str()
            .is_some_and(|e| e.contains("does not exist")),
        "{reply}"
    );
}

/// Writes an executable shell script.
fn script(dir: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A directory with fake `ffmpeg` and `whisper-cli` (which hears "Hello
/// there." 0.1 s into the take's burst) and one with a model file, for
/// `PATH` and `TAKEWISE_MODELS`.
fn fake_whisper(tmp: &Path) -> (PathBuf, PathBuf) {
    let bin = tmp.join("bin");
    let models = tmp.join("models");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&models).unwrap();
    fs::write(models.join("ggml-large-v3-turbo.bin"), b"").unwrap();
    // ffmpeg's conversion: a short float WAV whisper's copy is made from.
    let converted = tmp.join("converted.wav");
    write_take_wav(&converted, &[0; 160], 16_000).unwrap();
    script(
        &bin,
        "ffmpeg",
        &format!(
            r#"for a; do last=$a; done; cp "{}" "$last""#,
            converted.display()
        ),
    );
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
    (bin, models)
}

fn check_cmd(dir: &Path, bin: &Path, models: &Path) -> Command {
    let mut cmd = takewise();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    cmd.arg("check")
        .arg(dir)
        .env("PATH", path)
        .env("TAKEWISE_MODELS", models);
    cmd
}

#[test]
fn cli_check_reports_verdicts_json() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::write(dir.join("script.toml"), SCRIPT).unwrap();
    record_take(&dir, "hook");
    record_take(&dir, "hook");
    let (bin, models) = fake_whisper(tmp.path());

    let (code, value) = json(&mut check_cmd(&dir, &bin, &models));
    assert_eq!(code, 0, "{value}");
    let checks = value["data"]["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 2, "{value}");
    let first = &checks[0];
    assert_eq!(
        (first["line"].as_str(), first["take"].as_u64()),
        (Some("hook"), Some(1))
    );
    assert_eq!(first["verdict"], "keep", "{first}");
    assert_eq!(first["transcript"][0]["text"], "Hello", "{first}");
    assert_eq!(
        first["checker"],
        format!(
            "takewise {} whisper ggml-large-v3-turbo.bin pre=hp60+norm",
            env!("CARGO_PKG_VERSION")
        )
    );
    let take = &status(&dir)["lines"][0]["takes"][0];
    assert_eq!(take["effective"], "keep", "{take}");
    assert_eq!(take["verdict_source"], "check", "{take}");

    // Everything is checked now: nothing to do.
    let (code, value) = json(&mut check_cmd(&dir, &bin, &models));
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["data"]["checks"], serde_json::json!([]), "{value}");

    // An explicit take runs again; a different text makes it a retake, and
    // a retake is a verdict, not an error.
    takewise()
        .args(["script", "set"])
        .arg(&dir)
        .args(["hook", "--text", "Hello over there."])
        .assert()
        .success();
    let (code, value) =
        json(check_cmd(&dir, &bin, &models).args(["--line", "hook", "--take", "2"]));
    assert_eq!(code, 0, "{value}");
    let checks = value["data"]["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 1, "{value}");
    assert_eq!(checks[0]["take"], 2);
    assert_eq!(checks[0]["verdict"], "retake", "{value}");
    assert_eq!(checks[0]["reasons"][0]["code"], "missing-words", "{value}");

    // Take 1 is stale now; take 2 is current.
    let (code, value) = json(check_cmd(&dir, &bin, &models).arg("--stale-only"));
    assert_eq!(code, 0, "{value}");
    let checks = value["data"]["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 1, "{value}");
    assert_eq!(checks[0]["take"], 1);
}

#[test]
fn cli_check_setup_errors_exit_2() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::write(dir.join("script.toml"), SCRIPT).unwrap();
    record_take(&dir, "hook");
    let (bin, _) = fake_whisper(tmp.path());
    let (code, value) = json(&mut check_cmd(&dir, &bin, &tmp.path().join("no-models")));
    assert_eq!(code, 2, "{value}");
    assert_eq!(value["error"]["code"], "missing-model", "{value}");

    let (code, value) = json(
        check_cmd(&dir, &bin, &tmp.path().join("no-models"))
            .args(["--line", "hook", "--take", "9"]),
    );
    assert_eq!(code, 2, "{value}");
    assert_eq!(value["error"]["code"], "unknown-take", "{value}");
}

#[test]
fn cli_check_explains_takes_left_unchecked_by_the_setup() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::write(dir.join("script.toml"), SCRIPT).unwrap();
    let take = record_take(&dir, "hook");
    let (bin, _) = fake_whisper(tmp.path());
    let no_models = tmp.path().join("no-models");
    // The window checked the take without the model, as it does.
    let missing =
        takewise_core::SetupError::MissingModel(no_models.join("ggml-large-v3-turbo.bin"));
    let unavailable = takewise_core::Unavailable {
        reason: missing.to_string(),
        model: "ggml-large-v3-turbo.bin".to_owned(),
    };
    takewise_core::check_take(&dir, &take, &unavailable, "test").unwrap();

    // Checking again cannot change it: the model is still missing, which
    // is a setup error, not "nothing to check".
    let output = check_cmd(&dir, &bin, &no_models).output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not found"), "{stderr}");
    let (code, value) = json(&mut check_cmd(&dir, &bin, &no_models));
    assert_eq!(code, 2, "{value}");
    assert_eq!(value["error"]["code"], "missing-model", "{value}");

    // With the model in place, the take is checked again.
    let (bin, models) = fake_whisper(tmp.path());
    let (code, value) = json(&mut check_cmd(&dir, &bin, &models));
    assert_eq!(code, 0, "{value}");
    assert_eq!(
        value["data"]["checks"].as_array().unwrap().len(),
        1,
        "{value}"
    );
}

/// Takewise.app started from Finder gets `PATH=/usr/bin:/bin`: the tools
/// are still found where Homebrew installs them, as the checks find them.
#[test]
fn doctor_finds_the_tools_with_a_minimal_path() {
    use std::os::unix::fs::PermissionsExt as _;
    let tmp = tempfile::tempdir().unwrap();
    let (_, value) = json(
        takewise()
            .arg("doctor")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", tmp.path())
            .env("TAKEWISE_APP", NO_APP),
    );
    let checks = value["data"]["checks"].as_array().unwrap();
    for tool in ["whisper-cli", "ffmpeg", "ffprobe"] {
        let installed = ["/usr/bin", "/bin", "/opt/homebrew/bin", "/usr/local/bin"]
            .iter()
            .map(|dir| Path::new(dir).join(tool))
            .find(|path| {
                fs::metadata(path)
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            });
        let check = checks.iter().find(|c| c["name"] == tool).unwrap();
        match installed {
            Some(path) => {
                assert_eq!(check["ok"], true, "{check}");
                assert_eq!(check["detail"], path.display().to_string(), "{check}");
            }
            None => assert_eq!(check["ok"], false, "{check}"),
        }
    }
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
    let model = checks
        .iter()
        .find(|c| c["name"] == "whisper-model")
        .unwrap();
    assert!(
        model["detail"]
            .as_str()
            .unwrap()
            .contains("takewise models install"),
        "{model}"
    );
    let film = checks.iter().find(|c| c["name"] == "film").unwrap();
    assert_eq!(film["ok"], true, "{film}");
    // A new project has no input device chosen, so it cannot record yet.
    let input = checks.iter().find(|c| c["name"] == "input").unwrap();
    assert_eq!(input["ok"], false, "{input}");
    assert!(
        input["detail"]
            .as_str()
            .unwrap()
            .contains("No input device chosen"),
        "{input}"
    );
    assert!(value["data"]["inputs"].is_array(), "{value}");
    // Without Takewise.app the permission cannot be asked: a warning.
    let warnings = value["data"]["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("microphone permission")),
        "{value}"
    );
}

#[test]
fn status_version_matches_the_script_it_shows() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use takewise_core::ScriptVersion;

    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    let texts = [
        SCRIPT.to_owned(),
        SCRIPT.replace("Hello there.", "Hi there."),
    ];
    fs::write(dir.join("script.toml"), &texts[0]).unwrap();

    // Another writer keeps replacing script.toml while status runs.
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (dir, texts, stop) = (dir.clone(), texts.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut n = 0usize;
            while !stop.load(Ordering::Relaxed) {
                n += 1;
                let staged = dir.join(".script.toml.test");
                fs::write(&staged, &texts[n % 2]).unwrap();
                fs::rename(&staged, dir.join("script.toml")).unwrap();
            }
        })
    };
    let mut mismatches = Vec::new();
    for _ in 0..30 {
        let data = status(&dir);
        let shown = data["lines"][0]["text"].as_str().unwrap();
        let source = texts.iter().find(|t| t.contains(shown)).unwrap();
        let expected = ScriptVersion::of(source.as_bytes()).to_string();
        if data["script_version"] != expected.as_str() {
            mismatches.push(shown.to_owned());
        }
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    assert!(
        mismatches.is_empty(),
        "status issued a version for a different script than it showed: {mismatches:?}"
    );
}

#[test]
fn missing_take_file_fails_choose_and_strict_status() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::write(dir.join("script.toml"), SCRIPT).unwrap();
    record_take(&dir, "hook");
    takewise()
        .arg("choose")
        .arg(&dir)
        .args(["hook", "1"])
        .assert()
        .success();
    fs::rename(dir.join("takes/hook/001.wav"), tmp.path().join("lost.wav")).unwrap();

    let (code, value) = json(takewise().arg("choose").arg(&dir).args(["hook", "1"]));
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["error"]["code"], "missing-file");

    let (code, value) = json(takewise().arg("status").arg(&dir).arg("--strict"));
    assert_eq!(code, 1, "{value}");
    let problems = value["data"]["problems"].as_array().unwrap();
    assert!(
        problems
            .iter()
            .any(|p| p["code"] == "missing-file" && p["line"] == "hook"),
        "{problems:?}"
    );
    assert_eq!(value["data"]["lines"][0]["takes"][0]["missing"], true);
}

#[test]
fn models_status_and_install_find_the_model_by_size() {
    let tmp = tempfile::tempdir().unwrap();
    let models = tmp.path().join("models");
    let (code, value) = json(
        takewise()
            .args(["models", "status"])
            .env("TAKEWISE_MODELS", &models),
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["data"]["state"], "missing", "{value}");
    assert_eq!(value["data"]["model"]["file"], "ggml-large-v3-turbo.bin");
    assert_eq!(value["data"]["model"]["size"], 1_624_555_275_u64);

    // A sparse file of the pinned size stands in for the model, so nothing
    // is downloaded.
    fs::create_dir_all(&models).unwrap();
    fs::File::create(models.join("ggml-large-v3-turbo.bin"))
        .unwrap()
        .set_len(1_624_555_275)
        .unwrap();
    let (code, value) = json(
        takewise()
            .args(["models", "status"])
            .env("TAKEWISE_MODELS", &models),
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["data"]["state"], "installed", "{value}");
    let (code, value) = json(
        takewise()
            .args(["models", "install"])
            .env("TAKEWISE_MODELS", &models),
    );
    assert_eq!(code, 0, "{value}");
    assert_eq!(value["data"]["result"], "already-installed", "{value}");
}

#[test]
fn models_install_refuses_a_foreign_file() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("ggml-large-v3-turbo.bin"), "not a model").unwrap();
    let (code, value) = json(
        takewise()
            .args(["models", "install"])
            .env("TAKEWISE_MODELS", tmp.path()),
    );
    assert_eq!(code, 2, "{value}");
    assert_eq!(value["error"]["code"], "wrong-file", "{value}");
    assert_eq!(
        fs::read_to_string(tmp.path().join("ggml-large-v3-turbo.bin")).unwrap(),
        "not a model"
    );
}

/// Three sentences 0.5 and 0.4 s apart: one passage.
const PASSAGE_SCRIPT: &str = r#"
[[line]]
id = "one"
start = 10.0
end = 13.0
text = "One."

[[line]]
id = "two"
start = 13.5
end = 16.0
text = "Two."

[[line]]
id = "three"
start = 16.4
end = 19.0
text = "Three."
"#;

/// A passage of the three lines whose first sample lies at 7 s of the film
/// (when `placed`), `tenths` of a second long, with a 440 Hz burst for each
/// sentence.
fn passage(dir: &Path, tenths: usize, placed: bool) -> (PassageInput, Vec<i32>) {
    let samples = |tenths: usize| tenths * RATE as usize / 10;
    let project = Project::open(dir).unwrap();
    let input = PassageInput {
        sample_rate: RATE,
        device: "Test Mic".to_owned(),
        channel: 1,
        latency: Latency {
            input_s: 0.01,
            offset_ms: 0.0,
        },
        preroll_s: 3.0,
        lines: project
            .script
            .lines
            .iter()
            .map(|line| PassageLine {
                id: line.id.clone(),
                snapshot: project.snapshot(&line.id).unwrap(),
            })
            .collect(),
        first_sample_film_s: placed.then_some(7.0),
        dropouts: None,
        film_shift_ms: None,
    };
    let mut audio = vec![0i32; samples(tenths)];
    // The sentences, in tenths of a second from the passage's first sample:
    // 10.0 to 12.8 s, 13.6 to 15.8 s and 16.5 to 18.6 s of the film.
    for (from, to) in [(30, 58), (66, 88), (95, 116)] {
        for (i, sample) in audio
            .iter_mut()
            .enumerate()
            .skip(samples(from))
            .take(samples(to - from))
        {
            let t = i as f64 / f64::from(RATE);
            *sample = ((t * 440.0 * std::f64::consts::TAU).sin() * 4_000_000.0) as i32;
        }
    }
    (input, audio)
}

/// Streams a passage into its partial files and stops there, as a crash
/// mid-take leaves it.
fn crashed_passage(dir: &Path, placed: bool) -> PartialPassage {
    let (input, audio) = passage(dir, 135, placed);
    let partial = begin_passage(dir, &input).unwrap();
    let mut wav = PartialWav::create(&partial.wav, RATE).unwrap();
    wav.write(&audio).unwrap();
    wav.flush().unwrap();
    drop(wav);
    partial
}

#[test]
fn choose_passage_chooses_its_takes_and_names_the_lines_it_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::write(dir.join("script.toml"), PASSAGE_SCRIPT).unwrap();
    // Stopped mid-word in the second sentence: the third has no piece.
    let (input, audio) = passage(&dir, 72, true);
    let partial = begin_passage(&dir, &input).unwrap();
    write_take_wav(&partial.wav, &audio, RATE).unwrap();
    let id = publish_passage(&dir, &partial).unwrap().id;

    // Published but not cut yet: status says so, and repair cuts it.
    let before = status(&dir);
    assert_eq!(before["notices"][0]["code"], "passage-not-split");
    let (code, value) = json(takewise().arg("status").arg(&dir).arg("--repair"));
    assert_eq!(code, 0, "{value}");
    let data = &value["data"];
    assert_eq!(data["repair"]["pieces"].as_array().unwrap().len(), 2);
    let notices = data["notices"].as_array().unwrap();
    assert!(
        notices.iter().all(|n| n["code"] != "passage-not-split"),
        "{data}"
    );
    for line in data["lines"].as_array().unwrap() {
        assert_eq!(line["passage"], "one", "{line}");
    }
    let take = &data["lines"][1]["takes"][0];
    assert_eq!(take["file"], "takes/two/001.wav");
    assert_eq!(take["passage"]["id"], id.as_str());
    assert_eq!(take["passage"]["index"], 1);
    assert_eq!(take["passage"]["count"], 2);
    assert_eq!(take["passage"]["from_sample"], 297_600);
    assert_eq!(take["passage"]["to_sample"], 345_600);
    assert!(take["passage"]["preroll_floor_dbfs"].is_number());
    assert_eq!(data["lines"][2]["takes"], serde_json::json!([]));

    let choose = |args: &[&str]| json(takewise().arg("choose").arg(&dir).args(args));
    let (code, value) = choose(&["--passage", &id, "--by", "claude"]);
    assert_eq!(code, 0, "{value}");
    assert_eq!(
        value["data"],
        serde_json::json!({
            "passage": id,
            "by": "claude",
            "chosen": [{ "line": "one", "take": 1 }, { "line": "two", "take": 1 }],
            "skipped": [{ "line": "three", "reason": "the passage has no take of this line" }],
        })
    );
    let lines = status(&dir)["lines"].clone();
    assert_eq!(lines[0]["chosen"]["by"], "claude");
    assert_eq!(lines[1]["chosen"]["take"], 1);
    assert_eq!(lines[2]["chosen"], Value::Null);

    // All or nothing: one missing file refuses the passage.
    let events = fs::read_to_string(dir.join("takes.jsonl")).unwrap();
    fs::remove_file(dir.join("takes/two/001.wav")).unwrap();
    let (code, value) = choose(&["--passage", &id]);
    assert_eq!(code, 1, "{value}");
    assert_eq!(value["error"]["code"], "missing-file");
    assert_eq!(fs::read_to_string(dir.join("takes.jsonl")).unwrap(), events);

    let (code, value) = choose(&["--passage", "0000"]);
    assert_eq!(code, 2, "{value}");
    assert_eq!(value["error"]["code"], "unknown-passage");
    // A line and a take, or a passage: never both, never neither.
    takewise()
        .arg("choose")
        .arg(&dir)
        .args(["one", "1", "--passage", &id])
        .assert()
        .code(2);
    takewise().arg("choose").arg(&dir).assert().code(2);
    takewise()
        .arg("choose")
        .arg(&dir)
        .args(["--passage", &id])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("is missing"));
}

#[test]
fn status_repair_finishes_a_crashed_passage_and_reports_an_unplaced_one() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = init(tmp.path());
    fs::write(dir.join("script.toml"), PASSAGE_SCRIPT).unwrap();
    let crashed = crashed_passage(&dir, true);
    let unplaced = crashed_passage(&dir, false);
    let unplaced_wav = format!("takes/.passages/.{}.wav.partial", unplaced.id);

    let before = status(&dir);
    assert_eq!(before["partials"].as_array().unwrap().len(), 4, "{before}");
    let codes: Vec<&str> = before["notices"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["code"].as_str())
        .filter(|code| code.contains("passage"))
        .collect();
    assert_eq!(codes.len(), 2, "{before}");
    assert!(codes.contains(&"partial-passage") && codes.contains(&"passage-unplaced"));

    let (code, value) = json(takewise().arg("status").arg(&dir).arg("--repair"));
    assert_eq!(code, 0, "{value}");
    let data = &value["data"];
    assert_eq!(data["repair"]["passages_published"][0], crashed.id.as_str());
    assert_eq!(data["repair"]["pieces"].as_array().unwrap().len(), 3);
    assert_eq!(
        data["repair"]["passages_unplaced"][0],
        unplaced_wav.as_str()
    );
    for line in data["lines"].as_array().unwrap() {
        let take = &line["takes"][0];
        assert_eq!(take["passage"]["id"], crashed.id.as_str(), "{line}");
        assert_eq!(take["passage"]["count"], 3);
        assert_eq!(take["missing"], false);
    }
    assert!(
        dir.join(format!("takes/.passages/{}.wav", crashed.id))
            .is_file()
    );
    // The unplaced one is kept, and still reported with its path.
    assert!(unplaced.wav.is_file() && unplaced.meta.is_file());
    assert_eq!(data["partials"].as_array().unwrap().len(), 2);
    let notice = data["notices"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["code"] == "passage-unplaced")
        .unwrap_or_else(|| panic!("{data}"));
    assert!(
        notice["message"].as_str().unwrap().contains(&unplaced_wav),
        "{notice}"
    );

    takewise()
        .arg("status")
        .arg(&dir)
        .arg("--repair")
        .assert()
        .success()
        .stdout(predicate::str::contains("interrupted before it was placed"));
}
