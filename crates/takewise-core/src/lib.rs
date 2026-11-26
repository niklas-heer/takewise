//! Takewise's UI-free model: the project folder format, its safe write
//! protocol, the take ledger, speech detection, export, and the timing and
//! session rules of the recording window.

pub mod check;
pub mod clean;
pub mod edit;
pub mod error;
pub mod export;
pub mod ffmpeg;
pub mod film;
pub mod fsutil;
pub mod ledger;
pub mod models;
pub mod passage;
pub mod permission;
pub mod project;
pub mod remembered;
pub mod render;
pub mod rescue;
pub mod room;
pub mod route;
pub mod run_check;
pub mod script;
pub mod session;
pub mod setup;
pub mod speech;
pub mod state;
pub mod takes;
#[cfg(test)]
mod testutil;
pub mod timing;
pub mod wav;
pub mod whisper;
pub mod words;

pub use check::Transcript;
pub use edit::{
    Field, ScriptVersion, Volumes, clear_output_device, refresh_film, script_version,
    set_calibration, set_export, set_film, set_latency_offset, set_line_field, set_route,
    set_setup, set_volumes,
};
pub use error::{Error, Result};
pub use export::{
    ExportOptions, ExportReport, ExportSettings, ExportWarning, FilmOnDisk, ManifestLine,
    ManifestWord, Refusal, export, narration_file,
};
pub use film::{FilmChange, FilmWarning};
pub use fsutil::{ProjectLock, lock, write_atomic};
pub use ledger::{
    Checked, Chosen, Event, EventKind, Judged, Ledger, PassagePiece, Reason, Recorded, Snapshot,
    Verdict, Word, WordDiff, WordDiffKind, append, read,
};
pub use passage::{
    PartialPassage, Passage, PassageChoice, PassageInput, PassageLine, UnsavedPassage,
    begin_passage, choose_passage, passage_file, publish_passage, read_passage, save_passage,
    update_passage_meta,
};
pub use permission::{MicPermission, mic_advice};
pub use project::{
    CalibrationInfo, CheckConfig, ExportConfig, ExportFormat, FilmInfo, InputConfig, MonitorConfig,
    OutputConfig, PreviousFilm, Processing, Project, ProjectConfig, RecordConfig, SetupInfo,
};
pub use route::{
    Route, calibration_stale, effective_latency_offset, load_defaults, save_defaults,
    settings_path, setup_warning,
};
pub use run_check::{Unchecked, check_take, checker, unchecked_takes, words_not_checked_takes};
pub use script::{Line, Problem, Script, validate};
pub use session::{AvailableInput, RECORD_RATE, SessionState, can_record, check_input};
pub use speech::{Analysis, analyze, analyze_take, analyze_with_floor};
pub use state::{LineState, State, TakeState, fold};
pub use takes::{
    Judgment, PartialTake, PartialWav, PassageFailure, RecordedInput, RepairReport, TakeRef,
    WavLock, begin_take, choose_take, commit_take, finish_interrupted_commit, judge_take,
    read_take_wav, repair, rewrite_take_wav, update_take_meta, write_take_wav,
};
pub use timing::{
    Assembler, Dropout, GAP_TOLERANCE_S, RecordPlan, Timebase, TimebaseHistory, clamp_mark,
    line_offset_s, pace_fill, plan_take, sample_film_time,
};
pub use whisper::{SetupError, Transcriber, Unavailable, WhisperCli};
