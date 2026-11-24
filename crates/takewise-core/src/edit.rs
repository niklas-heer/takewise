//! Format-preserving edits of `script.toml` under the project lock.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::path::Path;
use std::str::FromStr;

use sha2::{Digest as _, Sha256};
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::error::{Error, Result, io};
use crate::fsutil::{lock, write_atomic};
use crate::project::{
    CalibrationInfo, ExportConfig, FilmInfo, PROJECT_FILE, SCRIPT_FILE, SetupInfo, parse_script,
    read_config, read_script_bytes,
};
use crate::script::validate;

/// One field of one line.
#[derive(Debug, Clone, PartialEq)]
pub enum Field {
    Text(String),
    Start(f64),
    End(Option<f64>),
    Note(Option<String>),
}

/// The SHA-256 of `script.toml`'s bytes, used to detect edits made elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScriptVersion(pub [u8; 32]);

impl ScriptVersion {
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

impl fmt::Display for ScriptVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for ScriptVersion {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let bytes = s.as_bytes();
        if bytes.len() != 64 {
            return Err("a script version is 64 hex digits".to_owned());
        }
        let mut out = [0u8; 32];
        for (slot, pair) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
            let pair = std::str::from_utf8(pair).map_err(|err| err.to_string())?;
            *slot = u8::from_str_radix(pair, 16).map_err(|err| err.to_string())?;
        }
        Ok(Self(out))
    }
}

/// The version of `script.toml` as it is on disk now (an absent file hashes as empty).
pub fn script_version(dir: &Path) -> Result<ScriptVersion> {
    Ok(ScriptVersion::of(&read_script_bytes(dir)?))
}

/// Changes one field of the line `line_id` in `script.toml`, keeping every
/// comment and the layout. Under the project lock: re-reads the file, rejects
/// the edit if `expected` no longer matches it or if the edit introduces a
/// validation problem, then replaces the file atomically.
pub fn set_line_field(
    dir: &Path,
    line_id: &str,
    field: Field,
    expected: Option<&ScriptVersion>,
) -> Result<ScriptVersion> {
    let path = dir.join(SCRIPT_FILE);
    let config = read_config(dir)?;
    let _guard = lock(dir)?;

    let bytes = read_script_bytes(dir)?;
    if expected.is_some_and(|version| *version != ScriptVersion::of(&bytes)) {
        return Err(Error::ScriptChanged);
    }
    let text = String::from_utf8(bytes).map_err(|err| Error::Parse {
        path: path.clone(),
        message: err.to_string(),
    })?;
    let before = parse_script(&path, &text)?;
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|err: toml_edit::TomlError| Error::Parse {
            path: path.clone(),
            message: err.to_string(),
        })?;

    let table = doc
        .get_mut("line")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|lines| {
            lines
                .iter_mut()
                .find(|table| table.get("id").and_then(Item::as_str) == Some(line_id))
        })
        .ok_or_else(|| Error::UnknownLine(line_id.to_owned()))?;
    match field {
        Field::Text(text) => set_value(table, "text", Some(Value::from(text))),
        Field::Start(start) => set_value(table, "start", Some(Value::from(start))),
        Field::End(end) => set_value(table, "end", end.map(Value::from)),
        Field::Note(note) => set_value(table, "note", note.map(Value::from)),
    }

    let edited = doc.to_string();
    let after = parse_script(&path, &edited)?;
    let duration = config.film_info.duration_s;
    let existing: HashSet<_> = validate(&before, duration)
        .into_iter()
        .map(|p| (p.line, p.code))
        .collect();
    let introduced: Vec<_> = validate(&after, duration)
        .into_iter()
        .filter(|p| !existing.contains(&(p.line.clone(), p.code)))
        .collect();
    if !introduced.is_empty() {
        return Err(Error::Invalid(introduced));
    }

    write_atomic(&path, edited.as_bytes())?;
    Ok(ScriptVersion::of(edited.as_bytes()))
}

/// Rewrites `[film_info]` in `project.toml` under the lock, keeping the rest
/// of the file as it is.
pub fn set_film_info(dir: &Path, info: &FilmInfo) -> Result<()> {
    let path = dir.join(PROJECT_FILE);
    let _guard = lock(dir)?;
    let text = fs::read_to_string(&path).map_err(io(&path))?;
    let parse_error = |message: String| Error::Parse {
        path: path.clone(),
        message,
    };
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|err: toml_edit::TomlError| parse_error(err.to_string()))?;
    let size = i64::try_from(info.size).map_err(|err| parse_error(err.to_string()))?;
    let table = doc
        .entry("film_info")
        .or_insert_with(toml_edit::table)
        .as_table_mut()
        .ok_or_else(|| parse_error("film_info is not a table".to_owned()))?;
    set_value(table, "duration_s", Some(Value::from(info.duration_s)));
    set_value(table, "size", Some(Value::from(size)));
    set_value(table, "modified", Some(Value::from(info.modified.as_str())));
    write_atomic(&path, doc.to_string().as_bytes())
}

/// Sets `[input] latency_offset_ms` in `project.toml` under the lock,
/// keeping the rest of the file, its comments and layout as they are.
pub fn set_latency_offset(dir: &Path, offset_ms: i32) -> Result<()> {
    edit_project(dir, |doc| {
        let input = table(doc, "input")?;
        set_value(
            input,
            "latency_offset_ms",
            Some(Value::from(i64::from(offset_ms))),
        );
        Ok(())
    })
}

/// Sets `[input] latency_offset_ms` together with the `[calibration]` it
/// was measured on, in one write under the lock.
pub fn set_calibration(dir: &Path, offset_ms: i32, info: &CalibrationInfo) -> Result<()> {
    edit_project(dir, |doc| write_calibration(doc, offset_ms, info))
}

/// [`set_calibration`], decided under the lock: `decide` gets the project
/// as it is on disk then, not as a caller read it some time ago, and
/// returns the offset and calibration to write, or `None` to leave the
/// file as it is. Returns whether it was written.
pub fn set_calibration_when(
    dir: &Path,
    decide: impl FnOnce(&crate::ProjectConfig) -> Option<(i32, CalibrationInfo)>,
) -> Result<bool> {
    edit_project_when(dir, |text, doc| {
        let config: crate::ProjectConfig = toml::from_str(text).map_err(|err| err.to_string())?;
        let Some((offset_ms, info)) = decide(&config) else {
            return Ok(false);
        };
        write_calibration(doc, offset_ms, &info)?;
        Ok(true)
    })
}

fn write_calibration(
    doc: &mut DocumentMut,
    offset_ms: i32,
    info: &CalibrationInfo,
) -> Result<(), String> {
    let input = table(doc, "input")?;
    set_value(
        input,
        "latency_offset_ms",
        Some(Value::from(i64::from(offset_ms))),
    );
    let cal = table(doc, "calibration")?;
    set_value(cal, "input", Some(Value::from(info.input.as_str())));
    set_value(cal, "output", Some(Value::from(info.output.as_str())));
    set_value(
        cal,
        "buffer_frames",
        info.buffer_frames.map(|n| Value::from(i64::from(n))),
    );
    set_value(cal, "date", Some(Value::from(info.date.as_str())));
    set_value(cal, "from", info.from.as_deref().map(Value::from));
    Ok(())
}

/// Writes the setup check's result as `[setup]` under the lock, keeping
/// the file's comments and layout; values the check did not measure are
/// removed.
pub fn set_setup(dir: &Path, info: &SetupInfo) -> Result<()> {
    edit_project(dir, |doc| write_setup(doc, info))
}

/// [`set_setup`], decided under the lock: `decide` gets the project as it
/// is on disk then, not as a caller read it some time ago, and returns the
/// result to write, or `None` to leave the file as it is. It runs under
/// the project lock, so what it does first (copying a room tone in) and
/// the `[setup]` that names it belong to one decision; when it fails,
/// nothing is written. Returns whether `[setup]` was written.
pub fn set_setup_when(
    dir: &Path,
    decide: impl FnOnce(&crate::ProjectConfig) -> Result<Option<SetupInfo>>,
) -> Result<bool> {
    // `decide` fails with an error of its own, not with a parse error of
    // `project.toml`.
    let mut failed = None;
    let written = edit_project_when(dir, |text, doc| {
        let config: crate::ProjectConfig = toml::from_str(text).map_err(|err| err.to_string())?;
        match decide(&config) {
            Ok(Some(info)) => write_setup(doc, &info).map(|()| true),
            Ok(None) => Ok(false),
            Err(err) => {
                failed = Some(err);
                Ok(false)
            }
        }
    })?;
    failed.map_or(Ok(written), Err)
}

fn write_setup(doc: &mut DocumentMut, info: &SetupInfo) -> Result<(), String> {
    let setup = table(doc, "setup")?;
    set_value(setup, "date", Some(Value::from(info.date.as_str())));
    set_value(setup, "input", Some(Value::from(info.input.as_str())));
    set_value(setup, "channel", Some(Value::from(i64::from(info.channel))));
    set_value(setup, "output", Some(Value::from(info.output.as_str())));
    set_value(
        setup,
        "room_floor_dbfs",
        info.room_floor_dbfs.map(|db| Value::from(round_db(db))),
    );
    set_value(
        setup,
        "speech_to_noise_db",
        info.speech_to_noise_db.map(|db| Value::from(round_db(db))),
    );
    set_value(setup, "from", info.from.as_deref().map(Value::from));
    Ok(())
}

/// Writes `[export]` under the lock, keeping the file's comments and
/// layout.
pub fn set_export(dir: &Path, config: &ExportConfig) -> Result<()> {
    edit_project(dir, |doc| {
        let export = table(doc, "export")?;
        set_value(
            export,
            "format",
            Some(Value::from(config.format.extension())),
        );
        set_value(
            export,
            "bit_depth",
            Some(Value::from(i64::from(config.bit_depth))),
        );
        set_value(
            export,
            "sample_rate",
            Some(Value::from(i64::from(config.sample_rate))),
        );
        set_value(
            export,
            "processing",
            Some(Value::from(config.processing.as_str())),
        );
        Ok(())
    })
}

/// A level to one decimal, as people read it.
fn round_db(db: f64) -> f64 {
    (db * 10.0).round() / 10.0
}

/// Sets the input device and channel and/or the output device in
/// `project.toml` under the lock, keeping its comments and layout. `None`
/// leaves that part as it is.
pub fn set_route(dir: &Path, input: Option<(&str, u16)>, output: Option<&str>) -> Result<()> {
    edit_project(dir, |doc| {
        if let Some((device, channel)) = input {
            let table = table(doc, "input")?;
            set_value(table, "device", Some(Value::from(device)));
            set_value(table, "channel", Some(Value::from(i64::from(channel))));
        }
        if let Some(device) = output {
            let table = table(doc, "output")?;
            set_value(table, "device", Some(Value::from(device)));
        }
        Ok(())
    })
}

/// Sets `[record] film_gain_db` and/or `[monitor] volume_db` under the
/// lock, keeping the file's comments and layout. `None` leaves that value
/// as it is.
pub fn set_volumes(dir: &Path, film_gain_db: Option<f64>, monitor_db: Option<f64>) -> Result<()> {
    edit_project(dir, |doc| {
        if let Some(db) = film_gain_db {
            set_value(table(doc, "record")?, "film_gain_db", Some(Value::from(db)));
        }
        if let Some(db) = monitor_db {
            set_value(table(doc, "monitor")?, "volume_db", Some(Value::from(db)));
        }
        Ok(())
    })
}

/// Edits `project.toml` with `edit` under the lock and writes it back
/// atomically; a file that no longer parses as a project is not written.
fn edit_project(
    dir: &Path,
    edit: impl FnOnce(&mut DocumentMut) -> Result<(), String>,
) -> Result<()> {
    edit_project_when(dir, |_, doc| edit(doc).map(|()| true)).map(drop)
}

/// [`edit_project`] for an edit that depends on what the file holds:
/// `edit` also gets the file's text as read under the lock, and says
/// whether to write its change (`false`: the file is left as it is).
/// Returns whether the file was written.
fn edit_project_when(
    dir: &Path,
    edit: impl FnOnce(&str, &mut DocumentMut) -> Result<bool, String>,
) -> Result<bool> {
    let path = dir.join(PROJECT_FILE);
    let _guard = lock(dir)?;
    let text = fs::read_to_string(&path).map_err(io(&path))?;
    let parse_error = |message: String| Error::Parse {
        path: path.clone(),
        message,
    };
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|err: toml_edit::TomlError| parse_error(err.to_string()))?;
    if !edit(&text, &mut doc).map_err(parse_error)? {
        return Ok(false);
    }
    let text = doc.to_string();
    toml::from_str::<crate::ProjectConfig>(&text).map_err(|err| parse_error(err.to_string()))?;
    write_atomic(&path, text.as_bytes())?;
    Ok(true)
}

/// The table `name`, created when missing.
fn table<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut Table, String> {
    doc.entry(name)
        .or_insert_with(toml_edit::table)
        .as_table_mut()
        .ok_or_else(|| format!("{name} is not a table"))
}

/// Replaces, inserts or removes `key`, keeping the old value's surrounding
/// whitespace and trailing comment.
fn set_value(table: &mut Table, key: &str, new: Option<Value>) {
    let Some(mut new) = new else {
        table.remove(key);
        return;
    };
    if let Some(Item::Value(old)) = table.get_mut(key) {
        *new.decor_mut() = old.decor().clone();
        *old = new;
    } else {
        table.insert(key, Item::Value(new));
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::{CalibrationInfo, Error, FilmInfo, Project};

    const SCRIPT: &str = r#"# Kindred promo narration
# drafted by an agent

[[line]]
id = "hook"
start = 0.0
text = "First words."   # keep this comment

# the second line
[[line]]
id = "today"
text = "Old text."  # inline comment
start = 5.0
note = "warm"

[[line]]
id = "question"
start = 10
text = "Third."
"#;

    fn project() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("p.takewise");
        let info = FilmInfo {
            duration_s: 51.5,
            size: 1,
            modified: "2026-11-22T20:05:11Z".to_owned(),
        };
        Project::init(&dir, Path::new("film.mp4"), info).unwrap();
        fs::write(dir.join("script.toml"), SCRIPT).unwrap();
        (tmp, dir)
    }

    fn script_text(dir: &Path) -> String {
        fs::read_to_string(dir.join("script.toml")).unwrap()
    }

    #[test]
    fn script_set_preserves_comments() {
        let (_tmp, dir) = project();
        set_line_field(
            &dir,
            "today",
            Field::Text("New text, with \"quotes\".".to_owned()),
            None,
        )
        .unwrap();
        let expected = SCRIPT.replace(r#""Old text.""#, r#"'New text, with "quotes".'"#);
        let actual = script_text(&dir);
        let reparsed = crate::project::read_script(&dir).unwrap();
        assert_eq!(reparsed.lines[1].text, "New text, with \"quotes\".");
        // toml_edit may pick either quoting style; compare with the value normalized.
        let normalized = actual.replace(
            r#""New text, with \"quotes\".""#,
            r#"'New text, with "quotes".'"#,
        );
        assert_eq!(normalized, expected);
    }

    #[test]
    fn script_set_changes_start_in_place() {
        let (_tmp, dir) = project();
        set_line_field(&dir, "today", Field::Start(6.5), None).unwrap();
        assert_eq!(
            script_text(&dir),
            SCRIPT.replace("start = 5.0", "start = 6.5")
        );
    }

    #[test]
    fn script_set_adds_and_removes_optional_fields() {
        let (_tmp, dir) = project();
        set_line_field(&dir, "hook", Field::End(Some(4.0)), None).unwrap();
        set_line_field(&dir, "today", Field::Note(None), None).unwrap();
        let script = crate::project::read_script(&dir).unwrap();
        assert_eq!(script.lines[0].end, Some(4.0));
        assert_eq!(script.lines[1].note, None);
        assert!(script_text(&dir).contains("# the second line"));
    }

    #[test]
    fn script_set_returns_new_version() {
        let (_tmp, dir) = project();
        let before = script_version(&dir).unwrap();
        let after =
            set_line_field(&dir, "hook", Field::Text("Hi.".to_owned()), Some(&before)).unwrap();
        assert_ne!(before, after);
        assert_eq!(after, script_version(&dir).unwrap());
        let parsed: ScriptVersion = after.to_string().parse().unwrap();
        assert_eq!(parsed, after);
    }

    #[test]
    fn script_set_rejects_stale_version() {
        let (_tmp, dir) = project();
        let seen = script_version(&dir).unwrap();
        fs::write(
            dir.join("script.toml"),
            format!("{SCRIPT}# edited elsewhere\n"),
        )
        .unwrap();
        let before = script_text(&dir);
        let err =
            set_line_field(&dir, "hook", Field::Text("Hi.".to_owned()), Some(&seen)).unwrap_err();
        assert!(matches!(err, Error::ScriptChanged), "{err:?}");
        assert_eq!(script_text(&dir), before);
    }

    #[test]
    fn script_set_rejects_invalid_start() {
        let (_tmp, dir) = project();
        let err = set_line_field(&dir, "today", Field::Start(12.0), None).unwrap_err();
        let Error::Invalid(problems) = err else {
            panic!("expected Error::Invalid, got {err:?}");
        };
        assert!(
            problems.iter().any(|p| p.code == "unordered"),
            "{problems:?}"
        );
        assert_eq!(script_text(&dir), SCRIPT);
    }

    #[test]
    fn write_preserves_project_toml_comments() {
        let (_tmp, dir) = project();
        let path = dir.join("project.toml");
        let original = fs::read_to_string(&path).unwrap();
        let commented = original.replacen(
            "[input]\n",
            "# the RODECaster's chat mix\n[input]\ndevice = \"Chat\"  # by name\n",
            1,
        );
        let commented = commented.replacen(
            "latency_offset_ms = 0",
            "latency_offset_ms = 0  # from the loopback test",
            1,
        );
        fs::write(&path, &commented).unwrap();

        set_latency_offset(&dir, -32).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert_eq!(
            written,
            commented.replace(
                "latency_offset_ms = 0  # from",
                "latency_offset_ms = -32  # from"
            )
        );
        let config = crate::project::read_config(&dir).unwrap();
        assert_eq!(config.input.latency_offset_ms, -32);
        assert_eq!(config.input.device.as_deref(), Some("Chat"));
    }

    #[test]
    fn set_route_preserves_comments() {
        let (_tmp, dir) = project();
        let path = dir.join("project.toml");
        let original = fs::read_to_string(&path).unwrap();
        let commented = original.replacen(
            "[input]\n",
            "# the RODECaster's chat mix\n[input]\ndevice = \"Chat\"  # by name\n",
            1,
        );
        let commented = commented.replacen("channel = 1", "channel = 1  # left", 1);
        fs::write(&path, &commented).unwrap();

        set_route(&dir, Some(("Main Stereo", 2)), Some("Main Stereo")).unwrap();

        let written = fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("# the RODECaster's chat mix\n[input]"),
            "{written}"
        );
        assert!(
            written.contains("device = \"Main Stereo\"  # by name"),
            "{written}"
        );
        assert!(written.contains("channel = 2  # left"), "{written}");
        let config = crate::project::read_config(&dir).unwrap();
        assert_eq!(config.input.device.as_deref(), Some("Main Stereo"));
        assert_eq!(config.input.channel, 2);
        assert_eq!(config.output.device.as_deref(), Some("Main Stereo"));

        // Only the output: the input stays.
        set_route(&dir, None, Some("Speakers")).unwrap();
        let config = crate::project::read_config(&dir).unwrap();
        assert_eq!(config.input.device.as_deref(), Some("Main Stereo"));
        assert_eq!(config.output.device.as_deref(), Some("Speakers"));
    }

    #[test]
    fn set_setup_writes_the_table() {
        let (_tmp, dir) = project();
        let info = SetupInfo {
            date: "2026-11-23".to_owned(),
            input: "Duo".to_owned(),
            channel: 2,
            output: "Duo Phones".to_owned(),
            room_floor_dbfs: Some(-68.04),
            speech_to_noise_db: Some(46.27),
            from: None,
        };
        set_setup(&dir, &info).unwrap();
        let config = Project::open(&dir).unwrap().config;
        assert_eq!(
            config.setup,
            Some(SetupInfo {
                room_floor_dbfs: Some(-68.0),
                speech_to_noise_db: Some(46.3),
                ..info.clone()
            })
        );
        // A gated input has no floor to store.
        let gated = SetupInfo {
            room_floor_dbfs: None,
            speech_to_noise_db: None,
            ..info
        };
        set_setup(&dir, &gated).unwrap();
        assert_eq!(
            Project::open(&dir).unwrap().config.setup,
            Some(gated.clone())
        );
        // A copied result says so; a measured one afterwards drops the mark.
        let copied = SetupInfo {
            from: Some(crate::project::REMEMBERED.to_owned()),
            ..gated.clone()
        };
        set_setup(&dir, &copied).unwrap();
        let text = fs::read_to_string(dir.join(PROJECT_FILE)).unwrap();
        assert!(text.contains("from = \"remembered\""), "{text}");
        assert!(
            Project::open(&dir)
                .unwrap()
                .config
                .setup
                .unwrap()
                .is_remembered()
        );
        set_setup(&dir, &gated).unwrap();
        assert_eq!(Project::open(&dir).unwrap().config.setup, Some(gated));
    }

    #[test]
    fn set_calibration_writes_offset_and_record() {
        let (_tmp, dir) = project();
        let info = CalibrationInfo {
            input: "Duo".to_owned(),
            output: "Duo".to_owned(),
            buffer_frames: Some(512),
            date: "2026-11-23".to_owned(),
            from: None,
        };
        set_calibration(&dir, 14, &info).unwrap();
        let config = crate::project::read_config(&dir).unwrap();
        assert_eq!(config.input.latency_offset_ms, 14);
        assert_eq!(config.calibration, Some(info.clone()));

        // A buffer size not known now removes the old one.
        let unknown = CalibrationInfo {
            buffer_frames: None,
            ..info.clone()
        };
        set_calibration(&dir, 15, &unknown).unwrap();
        let config = crate::project::read_config(&dir).unwrap();
        assert_eq!(config.calibration, Some(unknown));

        // A copied offset says so; measuring again drops the mark.
        let copied = CalibrationInfo {
            from: Some(crate::project::REMEMBERED.to_owned()),
            ..info.clone()
        };
        set_calibration(&dir, -29, &copied).unwrap();
        let config = crate::project::read_config(&dir).unwrap();
        assert!(config.calibration.unwrap().is_remembered());
        set_calibration(&dir, -30, &info).unwrap();
        let config = crate::project::read_config(&dir).unwrap();
        assert_eq!(config.calibration, Some(info));
    }

    #[test]
    fn set_volumes_writes_film_and_monitor_levels() {
        let (_tmp, dir) = project();
        set_volumes(&dir, Some(-9.0), None).unwrap();
        let config = crate::project::read_config(&dir).unwrap();
        assert!((config.record.film_gain_db + 9.0).abs() < f64::EPSILON);
        assert!((config.monitor.volume_db + 6.0).abs() < f64::EPSILON);
        set_volumes(&dir, None, Some(3.0)).unwrap();
        let config = crate::project::read_config(&dir).unwrap();
        assert!((config.record.film_gain_db + 9.0).abs() < f64::EPSILON);
        assert!((config.monitor.volume_db - 3.0).abs() < f64::EPSILON);
        assert!(!config.monitor.enabled, "enabled keeps its default");
    }

    #[test]
    fn set_latency_offset_adds_a_missing_input_table() {
        let (_tmp, dir) = project();
        let path = dir.join("project.toml");
        let original = fs::read_to_string(&path).unwrap();
        let start = original.find("[input]").unwrap();
        let end = original[start..]
            .find("\n\n")
            .map_or(original.len(), |i| start + i + 2);
        fs::write(&path, format!("{}{}", &original[..start], &original[end..])).unwrap();

        set_latency_offset(&dir, 12).unwrap();
        let config = crate::project::read_config(&dir).unwrap();
        assert_eq!(config.input.latency_offset_ms, 12);
        assert_eq!(config.input.channel, 1);
    }

    #[test]
    fn set_film_info_keeps_the_rest() {
        let (_tmp, dir) = project();
        let path = dir.join("project.toml");
        let original = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("# my film\n{original}")).unwrap();
        let info = FilmInfo {
            duration_s: 60.25,
            size: 99,
            modified: "2026-11-23T08:00:00Z".to_owned(),
        };
        set_film_info(&dir, &info).unwrap();
        let config = crate::project::read_config(&dir).unwrap();
        assert_eq!(config.film_info, info);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .starts_with("# my film\n")
        );
    }

    #[test]
    fn script_set_unknown_line() {
        let (_tmp, dir) = project();
        let err = set_line_field(&dir, "nope", Field::Text("x".to_owned()), None).unwrap_err();
        assert!(
            matches!(err, Error::UnknownLine(ref id) if id == "nope"),
            "{err:?}"
        );
    }
}
