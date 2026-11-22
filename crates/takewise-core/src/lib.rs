//! Takewise's UI-free model: the project folder format, its safe write
//! protocol, the take ledger, speech detection and export.

pub mod edit;
pub mod error;
pub mod fsutil;
pub mod ledger;
pub mod project;
pub mod script;
pub mod state;
pub mod takes;

pub use edit::{Field, ScriptVersion, script_version, set_line_field};
pub use error::{Error, Result};
pub use fsutil::{ProjectLock, lock, write_atomic};
pub use ledger::{
    Checked, Chosen, Event, EventKind, Judged, Ledger, Reason, Recorded, Snapshot, Verdict, Word,
    WordDiff, WordDiffKind, append, read,
};
pub use project::{CheckConfig, FilmInfo, InputConfig, Project, ProjectConfig, RecordConfig};
pub use script::{Line, Problem, Script, validate};
pub use state::{LineState, State, TakeState, fold};
pub use takes::{
    PartialTake, RecordedInput, RepairReport, TakeRef, begin_take, commit_take, read_take_wav,
    repair, write_take_wav,
};
