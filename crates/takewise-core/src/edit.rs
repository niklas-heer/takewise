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
    FilmInfo, PROJECT_FILE, SCRIPT_FILE, parse_script, read_config, read_script_bytes,
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
    use crate::{Error, FilmInfo, Project};

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
