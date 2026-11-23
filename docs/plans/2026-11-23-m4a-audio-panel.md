# Milestone 4a: Audio Panel, Meters, Scrolling and Monitoring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The narrator chooses input, channel and output in the window, sees which channel carries the mic, changes film and monitor volume during a session, sees near-clipping, reads long lines that scroll with the pace fill, and can hear their own voice when the device pair allows it.

**Architecture:** Settings schema and stale-calibration rules in `takewise-core`; device control (channel meters, output selection, buffer size, nominal sample rate, the monitor output stream) in `takewise-media`; the panel, meters and prompter scrolling in the GPUI window behind the existing `Media` trait so they stay testable with the fake. Software monitoring is gated by a hardware spike (Task 1); if the spike fails, Task 5 ships only the direct-monitoring hint.

**Tech Stack:** Rust 1.99.0, gpui-kit `=0.10.0` (its Select, Slider and Switch components), cpal 0.18, objc2-core-audio 0.3.

**Spec:** `docs/design.md`, section "Setup and sound" → "Settings schema", "Recording screen additions (milestone 4a)" and "Monitoring spike, the gate for 4a"; decision "Offer optional, low-latency monitoring".

## Global Constraints

- Capture uses a fixed buffer of 128 frames for every take when the spike passes; otherwise the device default. The choice is recorded in `[calibration]` as `buffer_frames`.
- Monitoring is off by default, toggled with **H** between takes only, refused on built-in speakers and when input and output are different devices, and never changes what is recorded. A monitor failure mutes only the monitor.
- Keys match typed characters: `-`/`+` film volume, `,`/`.` monitor volume (±1 dB steps, film clamped to −60..0 dB, monitor −60..+6 dB), **A** panel, **H** monitor, **K** clears the near-clipping light.
- Near-clipping light at −1 dBFS; "clipped" in a take row at −0.1 dBFS.
- Device and route changes apply between takes; during a take the pickers are disabled with a tooltip.
- A project without `[output]`, `[monitor]` or `[calibration]` keeps working exactly as before.
- Do not drive the GUI with synthetic keystrokes while Niklas uses the Mac; window behaviour is tested with the fake `Media`.

## Review Focus

1. **The configured input disappears between sessions** → the panel opens by itself with the device list and a plain-language message; Space stays blocked until a device is chosen. (Task 4 test `missing_device_opens_panel`.)
2. **A device that cannot switch to 48 kHz** → the "Switch to 48 kHz" button reports the CoreAudio error in words and recording stays blocked. (Task 2 test `nominal_rate_error_is_reported`.)
3. **Monitoring toggled, then the output device is changed to speakers** → monitoring turns off with a message, never feeds back. (Task 5 test `monitor_off_when_output_becomes_speakers`.)
4. **An old calibration after the buffer size changes** → status and the window say the calibration is stale. (Task 3 test `calibration_stale_on_buffer_change`.)
5. **A very long line (five rows)** → scrolling keeps the current words on the middle row and never jumps backwards during a take. (Task 6 test `scroll_is_monotonic`.)

---

### Task 1: Monitoring spike on the RØDECaster (with Niklas)

**Files:** `spike-monitor/` (gitignored, throwaway), a findings note `docs/monitoring-spike.md` (committed).

- [ ] **Step 1:** Build a small binary: capture the configured input at 128 frames with cpal, tap into a ring buffer, play it on the same device's output; play the Kindred film with AVPlayer at the same time; count dropouts over five minutes; run the loopback test (beeps via `takewise calibrate`'s routine) at the default buffer and at 128 frames, with the monitor off and on; estimate and, with the loopback, measure mic-to-ear delay.
- [ ] **Step 2:** Niklas runs it with wired headphones on the RØDECaster (the parent session coordinates this; do not run it unattended against his devices).
- [ ] **Step 3:** Record the numbers and the pass/fail against the design's criteria in `docs/monitoring-spike.md`; commit `docs: record the monitoring spike`.

### Task 2: Media: channel meters, output choice, buffer size, nominal rate

**Files:** `crates/takewise-media/src/{capture.rs,devices.rs,player.rs}`, new `src/monitor.rs` (empty stub until Task 5).

**Interfaces:**
- `Capture::start_with(device: &str, channel: u16, buffer_frames: Option<u32>, sink) -> Result<Capture>` (existing `start` calls it with `None`).
- `struct ChannelMeter` with `ChannelMeter::start(device: &str) -> Result<ChannelMeter>` and `levels() -> Vec<f32>` (peak dBFS per channel over the last 100 ms), for the panel while idle.
- `fn set_nominal_sample_rate(device: &str, rate: u32) -> Result<()>` (CoreAudio `kAudioDevicePropertyNominalSampleRate`).
- `fn output_devices() -> Result<Vec<DeviceInfo>>`; `FilmPlayer::set_output_device` exists already.
- [ ] Ignored hardware smoke tests `meters_every_channel`, `switches_nominal_rate` (skips if the device is already 48 kHz); unit test `nominal_rate_error_is_reported` on the error mapping. Commit `feat(media): add channel meters, output and rate control`.

### Task 3: Core: settings schema and staleness

**Files:** `crates/takewise-core/src/project.rs`, `state.rs`, `fsutil.rs` (settings file), `crates/takewise/src/cli.rs` (`status` text and JSON).

**Interfaces:** `OutputConfig { device: Option<String> }`, `MonitorConfig { enabled: bool, volume_db: f64 }` (default false, −6.0), `CalibrationInfo { input, output, buffer_frames: Option<u32>, date }`, `SetupInfo` (fields from the design, written in 4b; read now); `fn calibration_stale(config: &ProjectConfig, current: &Route) -> Option<String>`; `fn set_route(dir, input: Option<(&str, u16)>, output: Option<&str>) -> Result<()>` (format-preserving, under the lock); app defaults in `~/Library/Application Support/Takewise/settings.toml` via `fn load_defaults() / save_defaults(&Route)`.
- [ ] Tests `old_project_without_new_tables_opens`, `calibration_stale_on_device_change`, `calibration_stale_on_buffer_change`, `calibration_stale_without_record`, `set_route_preserves_comments`, `defaults_roundtrip`; `status` shows the stale-calibration and no-setup warnings. Commit `feat(core): store output, monitor and calibration settings`.

### Task 4: The audio panel and meters

**Files:** new `crates/takewise/src/window/panel.rs`, `window/meter.rs`; modify `window/mod.rs`, `window/media.rs` (trait gains channel levels, output list, rate switch), `window/prompter.rs` (meter moves to `meter.rs`).

- [ ] Panel (**A**): input, channel, output pickers; one live meter per input channel; film and monitor sliders; monitor switch (disabled with the reason when not allowed); the direct-monitoring hint; plain-language device problems; "Switch to 48 kHz" button. Main meter: 1.5 s peak hold, −18..−6 dBFS band, near-clipping light (**K** clears; cleared on the next take); "clipped" marker in take rows from the `checked` event's `clipped_samples` or, before the check, from the recorder's peak.
- [ ] Tests with the fake: `missing_device_opens_panel`, `pickers_disabled_during_take`, `route_change_applies_between_takes`, `volume_keys_match_characters` (`-`,`+`,`,`,`.` on both layouts), `near_clip_latches_until_k`, `clipped_take_row`. Commit per module: `feat(app): add the audio panel`, `feat(app): add peak hold and the near-clipping light`.

### Task 5: Monitoring (only if Task 1 passed)

**Files:** `crates/takewise-media/src/monitor.rs`, `window/recorder.rs` (tap), `window/panel.rs`.

- [ ] `struct Monitor` fed from the capture callback through a lock-free ring buffer (bounded, 4× buffer), output on the same device at 128 frames, volume in dB, silent on underrun; `estimated_delay_ms()` = input latency + 2 × buffer + output latency. **H** between takes; refused on speakers and different devices; resumes on open only for the same devices; warning above 15 ms.
- [ ] Tests (fake): `h_ignored_during_take`, `monitor_refused_on_speakers`, `monitor_refused_across_devices`, `monitor_off_when_output_becomes_speakers`, `monitor_resume_only_same_devices`, `monitor_failure_keeps_recording`. If Task 1 failed: implement only the hint and the disabled switch with the reason "not available on this setup". Commit `feat(app): monitor the narrator's voice on request`.

### Task 6: Scrolling long lines

**Files:** `window/prompter.rs`.

- [ ] Up to three rows at full size; longer lines scroll smoothly (eased, 200 ms) so the row holding the pace fill's current word stays in the middle; the first and last rows hold still; a retake starts from the top. Pure layout function `fn scroll_offset(row_of_current: usize, rows: usize, visible: usize) -> f32` tested: `scroll_is_monotonic`, `first_rows_hold_still`, `last_rows_hold_still`, `retake_resets_scroll`. Commit `feat(app): scroll long lines with the pace guide`.

### Task 7: Docs and verification

- [ ] README: the panel, keys, monitoring and its limits. `mise run check` → PASS; `mise run bundle`; a hand session with Niklas (panel, meters, monitoring on the RØDECaster, a five-row line) noted in the report. Commit `docs: describe the audio panel and monitoring`.
