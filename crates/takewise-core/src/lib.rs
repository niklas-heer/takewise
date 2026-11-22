//! Takewise's UI-free model: the project folder format, its safe write
//! protocol, the take ledger, speech detection, export, and the timing and
//! session rules of the recording window.

pub mod check;
pub mod edit;
pub mod error;
pub mod export;
pub mod fsutil;
pub mod ledger;
pub mod project;
pub mod script;
pub mod session;
pub mod speech;
pub mod state;
pub mod takes;
#[cfg(test)]
mod testutil;
pub mod timing;
pub mod words;

pub use edit::{
    Field, ScriptVersion, script_version, set_film_info, set_latency_offset, set_line_field,
};
pub use error::{Error, Result};
pub use export::{ExportOptions, ExportReport, ExportWarning, ManifestLine, Refusal, export};
pub use fsutil::{ProjectLock, lock, write_atomic};
pub use ledger::{
    Checked, Chosen, Event, EventKind, Judged, Ledger, Reason, Recorded, Snapshot, Verdict, Word,
    WordDiff, WordDiffKind, append, read,
};
pub use project::{CheckConfig, FilmInfo, InputConfig, Project, ProjectConfig, RecordConfig};
pub use script::{Line, Problem, Script, validate};
pub use session::{AvailableInput, RECORD_RATE, SessionState, can_record, check_input};
pub use speech::{Analysis, analyze};
pub use state::{LineState, State, TakeState, fold};
pub use takes::{
    Judgment, PartialTake, PartialWav, RecordedInput, RepairReport, TakeRef, WavLock, begin_take,
    choose_take, commit_take, judge_take, read_take_wav, repair, update_take_meta, write_take_wav,
};
pub use timing::{
    Assembler, Dropout, GAP_TOLERANCE_S, RecordPlan, Timebase, TimebaseHistory, clamp_mark,
    line_offset_s, pace_fill, plan_take, sample_film_time,
};
