# Milestone 1: Core and CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The project folder format, its safe write protocol, the ledger, speech detection, export and a `takewise` CLI that agents can use end to end, with no GUI and no media bridges yet.

**Architecture:** `takewise-core` holds all logic as a UI-free library with no `unsafe`. `takewise` is the binary: a thin clap layer over the core that prints human text or `--json`. Recording arrives in milestone 2; tests create takes by writing WAVs through the same core API the app will use.

**Tech Stack:** Rust 1.97.1 (edition 2024), serde + toml + toml_edit, serde_json, hound (WAV), jiff (timestamps), thiserror, clap (derive), std `File::lock` for advisory locks; dev: tempfile, assert_cmd, predicates.

**Spec:** `docs/design.md` (read sections "Project folder", "Timing model", "Checks" (speech part only), "CLI", "Export").

## Global Constraints

- Workspace lints from the root `Cargo.toml` apply (`unsafe_code = "deny"`, clippy pedantic as warnings, `unwrap_used = "deny"`); `mise run check` must pass: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- Pin dependencies to exact minor versions current on crates.io; keep features minimal.
- Sample rate is 48000; takes and export are mono 24-bit PCM WAV.
- Line ids match `^[a-z0-9]+(-[a-z0-9]+)*$`, are unique, and starts are strictly increasing.
- `takes.jsonl` is only ever appended to; nothing deletes takes, `.partial` files or ledger lines.
- Every CLI command supports `--json` (one JSON object on stdout). Exit codes: 0 success, 1 a finding (export refused, `status --strict` with problems), 2 usage or setup error.
- Times in TOML and JSON are seconds as `f64`; sample positions are `u64`/`i64` integers.
- Build with `CARGO_PROFILE_DEV_DEBUG=0`; run `mise run clean` at the end if `df -h ~` shows less than 20 GB free.

## Review Focus

1. **Hand-edited `script.toml` with comments and odd formatting** → `script set` changes one value and keeps every comment, blank line and key order. (Task 2 test `script_set_preserves_comments`.)
2. **Ledger whose last line was cut off by a crash** → reading ignores the torn line, status reports it, and the next append starts on a fresh line instead of gluing onto the fragment. (Task 3 test `append_after_torn_tail_starts_new_line`.)
3. **A line moved after its takes were recorded** → export places the takes relative to the new start, and status marks checks as stale. (Task 4 test `export_follows_moved_line`, Task 3 test `check_stale_after_slot_change`.)
4. **A take with only silence or background noise** → speech detection returns `None`; export of such a chosen take fails with a clear reason instead of writing silence. (Task 4 test `silent_take_has_no_speech`.)
5. **A first line starting at 0.0 with negative `line_offset_s`** → samples before film zero are cropped, not wrapped or panicking. (Task 4 test `export_crops_before_film_zero`.)

---

### Task 1: Workspace, project model and validation

**Files:**
- Create: `crates/takewise-core/Cargo.toml`, `crates/takewise-core/src/lib.rs`, `src/project.rs`, `src/script.rs`, `src/error.rs`
- Create: `crates/takewise/Cargo.toml`, `crates/takewise/src/main.rs` (prints version; real CLI in Task 5)
- Test: unit tests in each module

**Interfaces:**
- Produces:
  - `pub struct Project { pub dir: PathBuf, pub config: ProjectConfig, pub script: Script }` with `Project::open(dir: &Path) -> Result<Project>` and `Project::init(dir: &Path, film: &Path, film_info: FilmInfo) -> Result<Project>` (fails if `project.toml` already exists).
  - `ProjectConfig { format: u32, film: PathBuf, sample_rate: u32, film_info: FilmInfo, input: InputConfig, record: RecordConfig, check: CheckConfig }` mirroring the design's `project.toml`, with the design's defaults (`preroll_s = 2.0`, `tail_s = 1.5`, `film_gain_db = -12.0`, `channel = 1`, `latency_offset_ms = 0`, `language = "en"`, `whisper_model = "ggml-large-v3-turbo.bin"`).
  - `FilmInfo { duration_s: f64, size: u64, modified: String }`.
  - `Script { lines: Vec<Line> }`, `Line { id: String, start: f64, end: Option<f64>, text: String, note: Option<String> }`, `Script::slot_end(&self, idx: usize, film_duration: f64) -> f64` (explicit end, else next start, else film end).
  - `fn validate(script: &Script, film_duration: f64) -> Vec<Problem>`; `Problem { line: Option<String>, code: &'static str, message: String }` with codes `bad-id`, `duplicate-id`, `unordered`, `empty-slot`, `out-of-film`.
  - `pub enum Error` (thiserror) and `pub type Result<T>`.
  - `Project::film_path(&self) -> PathBuf` resolves a relative `film` against `dir`.

- [ ] **Step 1: Write failing tests** `validate_accepts_design_example` (the design's `hook` line, film 51.5 s → no problems), `validate_flags_each_problem` (one script per code, asserting the code), `slot_end_defaults` (no end → next start; last line → film duration), `init_then_open_roundtrips`, `init_refuses_existing_project`.
- [ ] **Step 2: Run** `cargo test -p takewise-core` → FAIL (missing items).
- [ ] **Step 3: Implement** the types with serde (`#[serde(rename = "line")]` for the array of tables), `init` writing `project.toml` and an empty `script.toml` (a comment header only) and creating `takes/`.
- [ ] **Step 4: Run** `cargo test -p takewise-core` → PASS; `mise run check` → PASS.
- [ ] **Step 5: Commit** `feat(core): add the project model and validation`.

### Task 2: Lock, atomic writes and script edits

**Files:**
- Create: `crates/takewise-core/src/fsutil.rs`, `crates/takewise-core/src/edit.rs`
- Test: unit tests in both modules

**Interfaces:**
- Consumes: `Project`, `Script`, `validate` (Task 1).
- Produces:
  - `pub struct ProjectLock` (RAII guard) and `fn lock(dir: &Path) -> Result<ProjectLock>`: creates `.lock` if needed and takes `File::lock()` (exclusive, blocking).
  - `fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()>`: temp file in the same directory, fsync, rename.
  - `pub enum Field { Text(String), Start(f64), End(Option<f64>), Note(Option<String>) }`.
  - `fn set_line_field(dir: &Path, line_id: &str, field: Field, expected: Option<&ScriptVersion>) -> Result<ScriptVersion>`: under the lock, re-read `script.toml` with `toml_edit`, change one value of the line with that id, reject (`Error::ScriptChanged`) if `expected` is given and differs from the file on disk, reject if the result fails `validate` (`Error::Invalid(Vec<Problem>)`), write atomically, return the new version.
  - `pub struct ScriptVersion(pub [u8; 32])`: a SHA-256 of the file bytes (`sha2` crate); `fn script_version(dir: &Path) -> Result<ScriptVersion>`.

- [ ] **Step 1: Write failing tests** `script_set_preserves_comments` (file with a header comment, an inline comment and blank lines; set `text` of the second line; assert the rest of the file is byte-identical except that value), `script_set_rejects_stale_version`, `script_set_rejects_invalid_start` (moving a start past the next line's start → `Error::Invalid` with `unordered`, file unchanged), `script_set_unknown_line`, `lock_is_exclusive` (a second `lock` from another thread blocks until the first guard drops; use a channel and a timeout).
- [ ] **Step 2: Run** `cargo test -p takewise-core edit fsutil` → FAIL.
- [ ] **Step 3: Implement** as specified.
- [ ] **Step 4: Run** the tests → PASS; `mise run check` → PASS.
- [ ] **Step 5: Commit** `feat(core): edit script.toml safely under a project lock`.

### Task 3: Ledger, takes and folding

**Files:**
- Create: `crates/takewise-core/src/ledger.rs`, `crates/takewise-core/src/takes.rs`, `crates/takewise-core/src/state.rs`
- Test: unit tests in each module

**Interfaces:**
- Consumes: `lock`, `write_atomic`, `Project`, `Line` (Tasks 1–2).
- Produces:
  - `pub enum EventKind { Recorded(Recorded), Checked(Checked), Judged(Judged), Chosen(Chosen) }`, serialized as the design's flat JSON: `{"v":1,"at":"…","kind":"recorded","line":"hook","take":1,…}` (internally tagged by `kind`).
  - `Recorded { file: String, sample_rate: u32, samples: u64, device: String, channel: u16, line_offset_s: f64, snapshot: Snapshot, recovered: bool }`; `Snapshot { text: String, start: f64, end: f64 }` (end resolved via `slot_end`).
  - `Checked { snapshot: Snapshot, transcript: Option<Vec<Word>>, word_diffs: Vec<WordDiff>, speech: Option<(f64, f64)>, overrun_s: Option<f64>, peak_dbfs: f64, clipped_samples: u64, noise_floor_dbfs: f64, verdict: Verdict, reasons: Vec<Reason>, checker: String }` (fields filled by milestone 3; define the types now: `Word { text, start, end, confidence: Option<f32> }`, `WordDiff { kind: Missing|Extra|Substituted, expected: Option<String>, heard: Option<String>, at: Option<f64> }`, `Reason { code, message }`, `Verdict { Keep, Retake, Listen }` serialized lowercase).
  - `Judged { snapshot: Snapshot, by: String, verdict: Verdict, score: Option<f64>, reason: String }`; `Chosen { by: String }`.
  - `fn append(dir: &Path, event: &Event) -> Result<()>`: under the lock; if the file does not end in `\n`, write a `\n` first; then one `write_all` of one JSON line opened with `O_APPEND`.
  - `fn read(dir: &Path) -> Result<Ledger>`; `Ledger { events: Vec<Event>, torn_tail: bool, bad_lines: Vec<usize> }` (a malformed line in the middle is skipped and reported, never fatal).
  - `RecordedInput { sample_rate: u32, device: String, channel: u16, line_offset_s: f64, snapshot: Snapshot }` (everything known when capture starts).
  - `fn begin_take(dir: &Path, line_id: &str, input: &RecordedInput) -> Result<PartialTake>`: writes `takes/<line>/.<uuid>.json.partial` with `input` and returns the WAV path `takes/<line>/.<uuid>.wav.partial` (a uuid avoids needing the lock while recording). `fn commit_take(dir: &Path, partial: PartialTake) -> Result<TakeRef>`: reads the finished WAV's sample count, then under the lock allocates the next free number (max of existing `NNN.wav` and ledger takes + 1), renames the metadata to `NNN.json` and the WAV to `NNN.wav`, appends `Recorded`. `TakeRef { line: String, take: u32 }`.
  - `fn write_take_wav(path: &Path, samples: &[i32], sample_rate: u32) -> Result<()>` and `fn read_take_wav(path: &Path) -> Result<(Vec<i32>, u32)>` (24-bit mono, `hound`).
  - `fn repair(dir: &Path) -> Result<RepairReport>`: adopt `NNN.wav` files without a `Recorded` event from their `NNN.json` (`recovered: true`); report those without `NNN.json` and all `.partial` files; never delete.
  - `fn fold(project: &Project, ledger: &Ledger) -> State`; `State { lines: Vec<LineState>, orphans: Vec<TakeRef>, partials: Vec<PathBuf>, unadopted: Vec<PathBuf>, problems: Vec<Problem> }`; `LineState { id, takes: Vec<TakeState>, chosen: Option<(u32, String)>, proposed: Option<u32> }`; `TakeState { take: u32, file: String, effective: Option<Verdict>, verdict_source: Option<String>, stale_check: bool, speech: Option<(f64, f64)> }`. Effective verdict: latest `Judged` whose snapshot equals the current line's snapshot (text, start, resolved end), else the latest matching `Checked`, else `None` (unchecked); a non-matching judgment or check sets `stale_check`. Proposed: the ranking from the design (`keep`, then `listen` and unchecked equally, then `retake`; then fewer word diffs; then smaller overrun; then higher score; then newest). Chosen: latest `Chosen`.

- [ ] **Step 1: Write failing tests** `event_json_matches_design` (serialize a `Recorded` and compare keys to the design), `append_after_torn_tail_starts_new_line`, `read_skips_bad_middle_line`, `commit_take_numbers_sequentially` (three takes → 1, 2, 3; files exist; three `recorded` events), `repair_adopts_unlogged_wav` (from its `NNN.json`), `repair_reports_wav_without_metadata`, `repair_keeps_partials`, `fold_judged_overrides_checked`, `judged_stale_after_text_edit`, `unchecked_ranks_with_listen`, `check_stale_after_slot_change`, `fold_latest_choice_wins`, `ranking_prefers_keep_then_fewer_diffs_then_newest`, `orphan_after_id_change`.
- [ ] **Step 2: Run** `cargo test -p takewise-core` → FAIL.
- [ ] **Step 3: Implement** as specified (`uuid` crate with `v4` for partial names; `jiff::Timestamp::now()` for `at`).
- [ ] **Step 4: Run** the tests → PASS; `mise run check` → PASS.
- [ ] **Step 5: Commit** `feat(core): add the take ledger, take files and state folding`.

### Task 4: Speech detection and export

**Files:**
- Create: `crates/takewise-core/src/speech.rs`, `crates/takewise-core/src/export.rs`
- Test: unit tests in both modules (synthesize audio in tests: silence plus low noise, with 440 Hz bursts as "speech")

**Interfaces:**
- Consumes: `State`, `fold`, `read_take_wav`, `Project`, `slot_end` (Tasks 1–3).
- Produces:
  - `fn analyze(samples: &[i32], sample_rate: u32, preroll_s: f64) -> Analysis`; `Analysis { speech: Option<(f64, f64)>, peak_dbfs: f64, clipped_samples: u64, noise_floor_dbfs: f64 }`. Speech: 10 ms RMS frames; noise floor = 20th percentile of frame dB within the pre-roll (whole take if shorter); speech frames exceed floor + 12 dB; speech start/end = first/last speech frame after merging gaps under 300 ms and discarding runs under 80 ms; times are seconds from the take's first sample. Clipped = samples with |x| at or above −0.1 dBFS of 24-bit full scale.
  - `pub struct ExportOptions { pub use_proposed: bool }`; `fn export(project: &Project, state: &State, opts: &ExportOptions) -> Result<ExportReport>` writing `export/narration.wav` (atomically) and `export/manifest.json`; `ExportReport { lines: Vec<ManifestLine>, refused: Vec<Refusal> }`. Refusals (no file written): `no-choice` (line ids; also lines without takes even with `use_proposed`), `no-speech` (a used take where `analyze` finds none), `overlap` (both ids), `missing-file`. Speech comes from running `analyze` on each used take's WAV at export time. With `use_proposed`, `chosen_by` is `"proposed"`. Placement: source range = speech ± 0.150 s clipped to the take; destination start sample `round((line.start + line_offset_s) * 48000) + src_start`; crop to `[0, round(film_info.duration_s * 48000))`; 10 ms linear fades in and out of each placed range; overlap is checked on destination ranges after cropping.
  - `ManifestLine { id, text, take_file, chosen_by, source_samples: [u64; 2], dest_samples: [u64; 2], speech_film_s: [f64; 2] }`, manifest top level `{ "v": 1, "film": FilmInfo + path, "sample_rate": 48000, "lines": [...] }`. Word times join in milestone 3.

- [ ] **Step 1: Write failing tests** `detects_burst_bounds` (0.5 s noise, burst 1.0–2.2 s, noise → speech within ±20 ms of 1.0/2.2 relative to the take), `silent_take_has_no_speech`, `counts_clipping`, `export_places_sample_exactly` (a take with `line_offset_s = -2.0` on a line at 10.0, burst at 2.0 s into the take → burst onset at destination sample 480000 ±1 frame), `export_follows_moved_line` (move the line's start to 12.0 via `set_line_field`; onset moves to 576000), `export_crops_before_film_zero`, `export_refuses_overlap`, `export_refuses_missing_choice`, `export_use_proposed`, `export_length_equals_film`.
- [ ] **Step 2: Run** `cargo test -p takewise-core speech export` → FAIL.
- [ ] **Step 3: Implement** as specified.
- [ ] **Step 4: Run** the tests → PASS; `mise run check` → PASS.
- [ ] **Step 5: Commit** `feat(core): detect speech and export the narration`.

### Task 5: The `takewise` CLI

**Files:**
- Modify: `crates/takewise/Cargo.toml`, `crates/takewise/src/main.rs`
- Create: `crates/takewise/src/cli.rs`, `crates/takewise/src/output.rs`, `crates/takewise/src/film.rs`
- Test: `crates/takewise/tests/cli.rs` (assert_cmd, temp dirs)
- Modify: `README.md` (CLI usage and the Resolve recipe from the design)

**Interfaces:**
- Consumes: everything from Tasks 1–4.
- Produces: the commands from the design's CLI table, except that `record` prints "recording arrives in milestone 2" and exits 2, `check` exits 2 with "checks arrive in milestone 3", and `doctor` checks only `whisper-cli` on PATH, the model file in `$TAKEWISE_MODELS` or `~/.local/share/takewise/models`, `ffprobe` on PATH, and (with a project) that the film exists and matches `film_info`.
  - `film.rs`: `fn probe(path: &Path) -> Result<FilmInfo>` via `ffprobe -v error -show_entries format=duration -of json` plus file metadata. `status --refresh-film` rewrites `film_info` under the lock.
  - `status` text: one line per script line (`id`, takes count, chosen, proposed, effective verdict of the chosen or proposed take, `stale`), then problems, orphans, partials, torn tail, film changed. `--json` prints the `State` plus problems. `--strict` exits 1 when any line lacks a chosen take or any problem exists. `--repair` runs `repair` first.
  - `choose` validates the take exists; `judge` validates verdict and `score` in 0..=1; both append through the core.

- [ ] **Step 1: Write failing tests** `init_status_roundtrip_json`, `script_set_then_status`, `choose_then_export_writes_wav_and_manifest` (create takes through `begin_take`/`write_take_wav`/`commit_take` from the core in the test), `export_without_choice_exits_1_with_line_ids`, `judge_rejects_bad_score_exit_2`, `status_strict_exit_codes`, `unknown_project_exit_2`, `record_and_check_are_stubs_exit_2`.
- [ ] **Step 2: Run** `cargo test -p takewise` → FAIL.
- [ ] **Step 3: Implement** with clap derive; errors print to stderr (or as `{"ok":false,"error":{"code","message"}}` with `--json`); success JSON is `{"ok":true,"data":…}`.
- [ ] **Step 4: Run** `mise run check` → PASS. Then a manual smoke run: `takewise init /tmp/tw-smoke.takewise --film ~/Projects/kindred-promo/renders/kindred-promo-v2.mp4 && takewise status /tmp/tw-smoke.takewise --json`, then remove the temp project.
- [ ] **Step 5: Commit** `feat(cli): add the takewise command line` and `docs: document the CLI and the Resolve handoff` (README) as separate commits.
