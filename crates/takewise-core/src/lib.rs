//! Takewise's UI-free model: the project folder format, its safe write
//! protocol, the take ledger, speech detection and export.

pub mod error;
pub mod project;
pub mod script;

pub use error::{Error, Result};
pub use project::{CheckConfig, FilmInfo, InputConfig, Project, ProjectConfig, RecordConfig};
pub use script::{Line, Problem, Script, validate};
