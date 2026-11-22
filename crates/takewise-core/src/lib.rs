//! Takewise's UI-free model: the project folder format, its safe write
//! protocol, the take ledger, speech detection and export.

pub mod edit;
pub mod error;
pub mod fsutil;
pub mod project;
pub mod script;

pub use edit::{Field, ScriptVersion, script_version, set_line_field};
pub use error::{Error, Result};
pub use fsutil::{ProjectLock, lock, write_atomic};
pub use project::{CheckConfig, FilmInfo, InputConfig, Project, ProjectConfig, RecordConfig};
pub use script::{Line, Problem, Script, validate};
