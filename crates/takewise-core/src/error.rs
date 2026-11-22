//! Errors from the core library.

use std::path::PathBuf;

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
        }
    }
}

/// Wraps an I/O error with the path it concerns.
pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.into();
    move |source| Error::Io { path, source }
}
