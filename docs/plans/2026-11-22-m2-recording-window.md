# Milestone 2: Recording Window Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Niklas can open a project, see the film with the line to read, record takes per line, play a take in context, choose takes, set starts and fix wording, and export the narration, all without touching Resolve.

**Architecture:** The proven spike code (`spike/`, gitignored, read-only reference with its `README.md`) becomes the `takewise-media` crate: AVPlayer with an NV12 video output for GPUI's `surface()`, a second AVPlayer for takes, cpal capture with CoreAudio latencies. Timing math and the recording session's rules live in `takewise-core` as pure, tested functions. The GPUI window in the `takewise` binary only wires these together. `takewise record` launches a signed `Takewise.app` so the microphone permission belongs to Takewise.

**Tech Stack:** Rust 1.99.0, gpui-kit `=0.10.0` (pins gpui-pre `=0.3.12`), cpal 0.18, objc2 0.6 with objc2-av-foundation / core-media / core-video / core-audio / core-foundation 0.3, hound.

**Spec:** `docs/design.md` (sections "Timing model", "The recording screen", "Architecture"), decision "Play, capture and align media through AVPlayer, GPUI surfaces and cpal" in `decisions/`.

## Global Constraints

- `rust-toolchain.toml` moves to `1.99.0`; `mise run check` passes on it.
- `unsafe` only in `crates/takewise-media`, which sets `#![allow(unsafe_code)]` at the crate root with a `SAFETY` comment on every `unsafe` block; everything else keeps `unsafe_code = "deny"`.
- The microphone is never monitored: no output stream ever carries input samples.
- The window never holds the project lock while recording; all writes go through `takewise-core` (`begin_take`, `write_take_wav`, `commit_take`, `choose_take`, `set_line_field` with the expected `ScriptVersion`).
- Film time of capture sample `i` in a buffer: `timebase(callback_host_time + i / sample_rate − L_in) + latency_offset_ms / 1000`, `L_in` = device latency + safety offset + stream latency. Never cpal's own capture timestamp.
- Keys exactly as the design: Space record/stop, Enter choose, R retake, P play in context, M mark start, E edit, ↑/↓ lines, ←/→ takes.
- Bundle identifier `com.niklasheer.takewise`; `NSMicrophoneUsageDescription` = "Takewise records your narration from the microphone you choose."
- Build with `CARGO_PROFILE_DEV_DEBUG=0`; check `df -h ~` before building; `mise run clean` at the end.

## Review Focus

1. **A seek or line change while a buffer is in flight** → buffers are mapped with the timebase valid when they were captured, never the new one. (Task 2 test `maps_with_timebase_valid_at_capture`.)
2. **Recording a line that starts at 0.0 s** → the film holds frame 0 during the count-in, capture starts at count-in, `line_offset_s` is negative and export crops it. (Task 2 test `first_line_holds_first_frame`.)
3. **`script.toml` changed by an agent while the window is open** → the window reloads; an edit made against the old version is refused with a visible message and nothing is overwritten. (Task 4 test `stale_edit_is_refused_and_reported`.)
4. **The configured input device is unplugged or not at 48 kHz** → the window shows the problem and the available devices, and Space does nothing. (Task 2 test `record_blocked_without_device` on the session; Task 4 shows it.)
5. **Quitting or closing the window mid-take** → the partial WAV is finalized and committed if it has audio, so the take is not lost. (Task 4 test `close_mid_take_commits`.)

---

### Task 1: Toolchain, the media crate and device listing

**Files:**
- Modify: `rust-toolchain.toml` (1.99.0), root `Cargo.toml` (shared objc2 versions in `[workspace.dependencies]`)
- Create: `crates/takewise-media/Cargo.toml`, `src/lib.rs`, `src/devices.rs`, `src/latency.rs`, `src/capture.rs`, `src/player.rs`, `src/video.rs`
- Modify: `crates/takewise/src/cli.rs` (`doctor` lists input devices with sample rate and channel count and warns when the default output is Bluetooth)
- Test: `crates/takewise-media/tests/smoke.rs` (all `#[ignore]`, run by hand: `cargo test -p takewise-media -- --ignored`)

**Interfaces:**
- Produces (`takewise_media`):
  - `fn input_devices() -> Result<Vec<DeviceInfo>>`, `fn default_output() -> Result<DeviceInfo>`; `DeviceInfo { name: String, sample_rate: u32, channels: u16, transport: Transport }`, `Transport { BuiltIn, Usb, Bluetooth, Other }`.
  - `fn input_latency_s(device: &str) -> Result<f64>` (device + safety offset + stream latency).
  - `struct Capture` with `Capture::start(device: &str, channel: u16, sink: Sender<CaptureBuffer>) -> Result<Capture>` and `Capture::stop(self)`; `CaptureBuffer { host_time_s: f64, samples: Vec<i32> }` (24-bit range, one channel, `host_time_s` from the callback's host time, before latency).
  - `struct FilmPlayer` (AVPlayer): `open(path: &Path) -> Result<FilmPlayer>`, `play_at(film_s: f64, host_time_s: f64)`, `pause()`, `seek(film_s: f64)`, `set_gain_db(db: f32)`, `set_muted(bool)`, `current_film_s() -> f64`, `timebase() -> Timebase` (rate and anchor pair read from the player's timebase), `latest_frame() -> Option<CVPixelBuffer>` (NV12 full range, Metal-compatible, IOSurface-backed `AVPlayerItemVideoOutput`, polled per frame).
  - `struct TakePlayer` (second AVPlayer): `open(path: &Path)`, `play_at(take_s: f64, host_time_s: f64)`, `stop()`.
  - `fn host_time_now_s() -> f64`.

- [ ] **Step 1:** Bump the toolchain; run `mise run check` on the existing code → PASS (fix only what 1.99.0 newly flags).
- [ ] **Step 2: Write the ignored smoke tests** `lists_input_devices`, `plays_film_frames` (open the Kindred film, play 2 s, at least 40 frames), `captures_one_channel` (2 s from the default input; buffers arrive with increasing `host_time_s`).
- [ ] **Step 3: Implement** by porting `spike/src/{audio,latency,player}.rs` and the video-output part of `spike/src/window.rs` into the API above, dropping the spike's CLI glue.
- [ ] **Step 4: Run** `mise run check` → PASS and `cargo test -p takewise-media -- --ignored` → PASS (needs microphone permission for the terminal host; if it hangs on the prompt, report it and continue).
- [ ] **Step 5: Commit** `build: move to Rust 1.99.0` and `feat(media): add playback, capture and device bridges` (two commits).

### Task 2: Timing and the recording session in the core

**Files:**
- Create: `crates/takewise-core/src/timing.rs`, `crates/takewise-core/src/session.rs`
- Test: unit tests in both modules

**Interfaces:**
- Consumes: `Project`, `Script::slot_end`, `RecordedInput`, `Snapshot` (milestone 1).
- Produces:
  - `struct Timebase { rate: f64, film_anchor_s: f64, host_anchor_s: f64 }` with `film_at(host_s: f64) -> f64`.
  - `struct TimebaseHistory` with `record(at_host_s: f64, tb: Timebase)` and `film_at(host_s: f64) -> Option<f64>` (uses the timebase valid at `host_s`; `None` before the first).
  - `fn sample_film_time(history: &TimebaseHistory, buffer_host_s: f64, index: usize, sample_rate: u32, input_latency_s: f64, offset_ms: f64) -> Option<f64>`.
  - `struct RecordPlan { film_from_s: f64, hold_first_frame_s: f64, capture_stop_film_s: f64 }` and `fn plan_take(line_start: f64, slot_end: f64, preroll_s: f64, tail_s: f64) -> RecordPlan` (`film_from_s = max(0, start − preroll)`; `hold_first_frame_s = max(0, preroll − start)`; stop at `slot_end + tail_s`).
  - `fn line_offset_s(first_sample_film_s: f64, line_start: f64) -> f64`.
  - `fn pace_fill(text: &str, slot_start: f64, slot_end: f64, now_film_s: f64) -> usize` (characters to highlight, by character count, clamped to the text).
  - `fn clamp_mark(script: &Script, idx: usize, film_s: f64, film_duration: f64) -> f64` (between the previous line's start + 0.001 and the line's own slot end − 0.001).
  - `enum SessionState { Idle, CountIn { until_host_s: f64 }, Recording { line: String, partial: PartialTake }, Blocked(String) }` and `fn can_record(input_ok: Result<(), String>) -> SessionState` returning `Blocked` with the message when the device is missing or not 48 kHz.

- [ ] **Step 1: Write failing tests** `maps_with_timebase_valid_at_capture` (seek recorded at host 10.0; a buffer at host 9.99 maps with the old timebase), `sample_film_time_applies_latency_and_offset` (exact values: buffer host 5.0, index 480 at 48 kHz, latency 0.051, offset −31.5 ms, rate 1, anchor film 0 at host 0 → 5.0 + 0.01 − 0.051 − 0.0315), `first_line_holds_first_frame` (start 0.0, preroll 2.0 → film_from 0, hold 2.0), `plan_take_regular_line`, `pace_fill_bounds` (before slot 0, after slot full length, halfway ≈ half), `clamp_mark_between_neighbours`, `record_blocked_without_device`.
- [ ] **Step 2: Run** `cargo test -p takewise-core timing session` → FAIL.
- [ ] **Step 3: Implement** as specified.
- [ ] **Step 4: Run** → PASS; `mise run check` → PASS.
- [ ] **Step 5: Commit** `feat(core): add timing maps and the recording session rules`.

### Task 3: The app bundle and `takewise record`

**Files:**
- Create: `packaging/Info.plist`, `scripts/bundle` (shell, like Focal's `scripts/bundle`)
- Modify: `mise.toml` (`bundle` task: build release and assemble `build/Takewise.app`), `crates/takewise/src/cli.rs` (`record`), `README.md`
- Test: `crates/takewise/tests/cli.rs` (`record_without_bundle_explains`)

**Interfaces:**
- Produces: `scripts/bundle` builds `build/Takewise.app` containing the `takewise` binary, signs with `$TAKEWISE_SIGN_IDENTITY` (Developer ID) when set, else ad hoc. `takewise record <dir>` validates the project, then runs `open -n -W <bundle> --args window <abs dir>`; the bundle is found next to the binary's `../..` when the binary runs inside the bundle, else at `$TAKEWISE_APP`, else `/Applications/Takewise.app`, else `build/Takewise.app` relative to the repo; missing → exit 2 with "run `mise run bundle`". The hidden subcommand `window <dir>` opens the GPUI window (Task 4) and is what runs inside the bundle.

- [ ] **Step 1: Write the failing test** `record_without_bundle_explains` (with `TAKEWISE_APP` pointing nowhere and no other bundle found, exit 2 and the hint).
- [ ] **Step 2: Implement** the plist (identifier and usage string from Global Constraints, `LSMinimumSystemVersion` 14.0), the script and the subcommand.
- [ ] **Step 3: Run** `mise run check` → PASS; `mise run bundle` → `build/Takewise.app` exists and `codesign -dv build/Takewise.app` shows identifier `com.niklasheer.takewise`.
- [ ] **Step 4: Commit** `feat(cli): launch recording in a signed app bundle`.

### Task 4: The recording window

**Files:**
- Create: `crates/takewise/src/window/mod.rs`, `window/video.rs`, `window/prompter.rs`, `window/lines.rs`, `window/takes.rs`, `window/recorder.rs`, `window/watch.rs`
- Test: `crates/takewise/src/window/tests.rs` (GPUI test support as in Focal's `ui_tests.rs`, media replaced by a fake behind a small trait)

**Interfaces:**
- Consumes: `takewise_media::{FilmPlayer, TakePlayer, Capture, input_latency_s, input_devices}`; core `timing`, `session`, `begin_take`, `write_take_wav`, `commit_take`, `choose_take`, `set_line_field`, `script_version`, `fold`, `read`.
- Produces: the screen from the design's sketch. `trait Media` with the calls the window uses, implemented by a real adapter over `takewise_media` and a fake for tests.
  - **video.rs:** draws `FilmPlayer::latest_frame()` with `surface()` every frame, 16:9, letterboxed.
  - **prompter.rs:** current line large, next line dimmed, slot bar, pace fill (`pace_fill`), level meter from the last capture buffer's peak, count-in 3-2-1, the `Blocked` message.
  - **lines.rs / takes.rs:** line list with take count and chosen mark; takes of the current line with effective verdict (or "unchecked"), hollow star for proposed, filled for chosen.
  - **recorder.rs:** Space: `plan_take`, warm-up (play the film muted for 0.3 s before the first take after opening), `begin_take` with the snapshot, start capture, play film from `film_from_s` (holding frame 0 for `hold_first_frame_s`), feed buffers into a WAV writer and a `TimebaseHistory` filled from the player's timebase on every play and seek; first sample's film time via `sample_film_time` gives `line_offset_s`; stop on Space or at `capture_stop_film_s`, then `commit_take`. P: play the film from `film_from_s` and the selected take with `TakePlayer::play_at` at the matching host time. Enter: `choose_take` with `by = "narrator"`.
  - **watch.rs:** watches `script.toml` and `takes.jsonl` (FSEvents through the `notify` crate) and reloads the fold; E and M call `set_line_field` with the version the window loaded and show `ScriptChanged` as a message.

- [ ] **Step 1: Write failing tests** with the fake media: `space_records_and_commits_a_take` (ledger gains one `recorded` with the expected `line_offset_s`), `enter_chooses_selected_take`, `mark_start_clamps`, `stale_edit_is_refused_and_reported`, `close_mid_take_commits`, `blocked_device_ignores_space`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** the modules.
- [ ] **Step 4: Run** `mise run check` → PASS; `mise run bundle`; open the Kindred project (`takewise init` a scratch project on `/Users/nheer/Projects/github.com/niklas-heer/kindred-promo/renders/kindred-promo-v2.mp4` with the eight lines from the Kindred narration) with `takewise record`; record one take on the built-in mic, choose it, export; screenshot the window for the report. Delete the scratch project afterwards.
- [ ] **Step 5: Commit** `feat(app): add the recording window` (split into a few commits along the modules if it helps review).

### Task 5: Calibration

**Files:**
- Create: `crates/takewise/src/calibrate.rs`
- Modify: `crates/takewise/src/cli.rs` (`calibrate <dir> [--write]`, runs inside the bundle like `record`), `README.md`

**Interfaces:**
- Produces: `takewise calibrate <dir>` plays a generated beep film (ten 1 kHz beeps, 20 ms, at known film times including 0.1 s, made with `ffmpeg` into the system temp dir) through the default output, captures the configured input with the same mapping as recording (with `latency_offset_ms = 0`), detects beep onsets (rising edge above noise floor + 20 dB, ignoring onsets more than 100 ms from any expected beep), and prints mean and standard deviation of placement error; `--write` stores `−mean` in `latency_offset_ms` through a format-preserving edit of `project.toml` under the lock. Fewer than 6 beeps detected → exit 1 with "hold the headphones to the mic and raise the volume".

- [ ] **Step 1: Write failing tests** for the pure parts: `detects_beep_onsets` (synthesized beeps with noise), `ignores_far_onsets`, `write_preserves_project_toml_comments`.
- [ ] **Step 2: Implement.**
- [ ] **Step 3: Run** `mise run check` → PASS; run `takewise calibrate` once on the built-in speakers and mic and include the numbers in the report.
- [ ] **Step 4: Commit** `feat(cli): calibrate latency with a loopback test`.
