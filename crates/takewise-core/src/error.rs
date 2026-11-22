//! Errors from the core library.

use std::path::PathBuf;

use crate::script::Problem;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{}: {message}", path.display())]
    Parse { path: PathBuf, message: String },
    #[error("a project already exists at {}", .0.display())]
    AlreadyExists(PathBuf),
    #[error("script.toml changed on disk since it was read; re-read it and try again")]
    ScriptChanged,
    #[error("the edit would make the script invalid: {}", describe(.0))]
    Invalid(Vec<Problem>),
    #[error("no line with id {0:?} in script.toml")]
    UnknownLine(String),
    #[error("line {line:?} has no take {take}")]
    UnknownTake { line: String, take: u32 },
    #[error("{line} take {take}: its file {} is missing", path.display())]
    MissingFile {
        line: String,
        take: u32,
        path: PathBuf,
    },
    #[error("no Takewise project at {} (missing project.toml)", .0.display())]
    NotAProject(PathBuf),
}

impl Error {
    /// A stable, kebab-case code for JSON output.
    pub fn code(&self) -> &'static str {
        match self {
            Error::Io { .. } => "io",
            Error::Parse { .. } => "parse",
            Error::AlreadyExists(_) => "already-exists",
            Error::NotAProject(_) => "not-a-project",
            Error::ScriptChanged => "script-changed",
            Error::Invalid(_) => "invalid-script",
            Error::UnknownLine(_) => "unknown-line",
            Error::UnknownTake { .. } => "unknown-take",
            Error::MissingFile { .. } => "missing-file",
        }
    }
}

/// Wraps an I/O error with the path it concerns.
pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.into();
    move |source| Error::Io { path, source }
}

fn describe(problems: &[Problem]) -> String {
    problems
        .iter()
        .map(|p| match &p.line {
            Some(line) => format!("{line}: {} ({})", p.message, p.code),
            None => format!("{} ({})", p.message, p.code),
        })
        .collect::<Vec<_>>()
        .join("; ")
}
