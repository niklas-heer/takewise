# Milestone 4b: Setup Check, Noise Rule and Clean Export Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A guided setup check gets the narrator to a good level and a quiet enough room, every take is judged for background noise, Whisper hears a cleaned copy, and `clean` export delivers narration at −16 LUFS / −1 dBTP in the chosen format, placed exactly where `raw` places it.

**Architecture:** Analysis (levels, plosives, boom, hum, speech-to-noise) and the export pipeline's decisions live in `takewise-core` as pure functions over samples; `ffmpeg` does filtering, loudness measurement, resampling, dither and FLAC encoding through a small runner with timeouts (reuse `whisper::find_tool` and the child-process helper). The setup check is a GPUI view over the core analysis, tested with the fake `Media`.

**Tech Stack:** Rust 1.99.0, existing crates; `ffmpeg` filters `highpass`, `afftdn`, `ebur128`, `aresample` (soxr), `aformat`; FLAC through ffmpeg.

**Spec:** `docs/design.md` "Setup and sound" → "Setup check (milestone 4b)", "Noise per take", "Cleaner audio for the checks", "Export cleanup and formats"; "Export and Resolve handoff".

## Global Constraints

- Takes are never modified; every processed file is a new file, written atomically.
- `clean` places speech at the same samples as `raw` (verified by a click fixture to ±1 sample at 48 kHz).
- Loudness: one static gain, min(gain to −16 LUFS, gain to −1 dBTP); no `loudnorm` dynamic mode. Final file re-measured; if true peak > −1 dBTP, lower gain by the excess and redo once.
- A project without `[export]` exports 48 kHz 24-bit WAV `raw`, as before. Exactly one narration file exists beside the manifest.
- `raw` 48 kHz WAV export works without `ffmpeg`.
- Hints (plosives, boom, hum) never block anything; only the noise rule changes verdicts (`listen`).

## Review Focus

1. **A gated interface (pre-roll and room tone are digital silence)** → speech-to-noise is reported as "silent between words (gated)", noise reduction is skipped, the noise rule does not fire. (Task 1 test `gated_input_is_not_noisy`.)
2. **A narrator with a deep voice** → the boom hint uses a ratio threshold tuned so a normal male voice at 15 cm does not trigger it; it is a hint only. (Task 1 test `deep_voice_without_proximity_is_quiet`, synthetic.)
3. **A take whose pre-roll floor differs by more than 6 dB from every room tone** → that take skips noise reduction, the manifest says why. (Task 4 test `mismatched_room_tone_skips_nr`.)
4. **Export at 44.1 kHz 16-bit FLAC** → destination sample ranges follow `round(n × 44100 / 48000)`, the file decodes to the expected length, true peak ≤ −1 dBTP after dither. (Task 5 tests.)
5. **Switching format between exports** → the other narration file is removed; the manifest names the current file and its hash. (Task 5 test `format_switch_leaves_one_file`.)

---

### Task 1: Core analysis for setup and noise

**Files:** create `crates/takewise-core/src/setup.rs`; modify `check.rs`, `ledger.rs` (`Checked.speech_level_dbfs`, optional), `speech.rs` (reuse frames).

**Interfaces:** `fn voice_report(samples: &[i32], rate: u32) -> VoiceReport { peak_dbfs, speech_rms_dbfs, clipped: u64, gain_change_db: f64 /* to put peaks at −10 dBFS */, plosives: u32, boom_ratio_db: f64, hints: Vec<Hint> }`; `fn room_report(samples, rate, speech_rms_dbfs) -> RoomReport { floor_dbfs, gated: bool, snr_db: Option<f64>, rating: Good|Acceptable|Noisy, hum: Option<HumReport { fundamental_hz: 50|60, harmonics: Vec<f64> }>, tips: Vec<String> }`. Plosive: 10 ms frames whose energy below 80 Hz exceeds the 200 Hz–4 kHz energy by 6 dB and the frame is within the speech. Boom: band energy ratio < 200 Hz vs 200 Hz–4 kHz over speech, hint when > +3 dB (tunable constant). Hum: FFT over the whole room recording; peak at 50 or 60 Hz or its 2nd/3rd harmonic ≥ 10 dB above the median of ±10 Hz neighbours, present in ≥ 2 harmonics. Noise rule in `build_checked`: SNR = speech RMS − pre-roll floor < 30 dB → `listen` `background-noise` (skipped when the pre-roll is gated).
- [x] Tests: `gain_suggestion_targets_minus_ten`, `detects_plosive_bursts`, `deep_voice_without_proximity_is_quiet`, `detects_mains_hum_with_harmonics`, `broadband_noise_is_not_hum`, `gated_input_is_not_noisy`, `noise_rule_listens_below_30_db`, `noise_rule_order_after_sound_rule`. Commit `feat(core): analyze voice, room and per-take noise`.

### Task 2: Cleaner audio for the checks

**Files:** `crates/takewise-core/src/whisper.rs` (conversion step), `run_check.rs` (checker tag).

- [x] The ffmpeg conversion before whisper adds `highpass=f=80` and peak normalization to −3 dBFS (`loudnorm` not used); the checker tag becomes `pre=hp80+norm`, so older checks re-run. Re-verify Niklas's eight lines stay keep (read-only, as in the M3 plan task 5). Tests: `conversion_args_include_highpass`, `checker_tag_is_hp80_norm`. Commit `feat(core): give whisper a cleaned copy of each take`.

### Task 3: The setup check window

**Files:** create `crates/takewise/src/window/setup.rs`; modify `window/mod.rs` (**C**, auto-offer), `window/media.rs` (record a short clip without a take), `crates/takewise-core/src/project.rs` (`write_setup`, room tone files `takes/.room/<timestamp>.wav` with a sidecar JSON of devices and floor).

- [x] Steps exactly as the design: devices (flags speakers/Bluetooth) → voice (read the sentence "The quick brown fox jumps over the lazy dog, then naps by the warm fire." with a 5 s capture, report, repeat until peaks are within −14..−6 dBFS or the narrator continues) → room (5 s silence, report, keep room tone) → latency (offer calibrate when stale). Offered on first open and when devices differ from `[setup]`; never during a take. Tests (fake media + synthetic samples): `setup_offered_on_first_open`, `setup_not_offered_when_current`, `voice_step_repeats_until_level_fits`, `room_tone_saved_with_devices`, `setup_writes_project_toml`. Commit `feat(app): guide the narrator through a setup check`.

### Task 4: Clean processing of takes

**Files:** create `crates/takewise-core/src/clean.rs`, a `ffmpeg` runner `crates/takewise-core/src/ffmpeg.rs` (timeouts, nice, lossy output; reuse the child helper).

**Interfaces:** `fn choose_room_tone(dir, recorded: &Recorded, take_floor_dbfs: f64) -> Option<RoomTone>` (same devices, same date, floor within 6 dB, not gated; newest wins); `fn clean_take(src: &Path, room: Option<&RoomTone>, out: &Path) -> Result<CleanInfo>` (`highpass=f=80`, then `afftdn=nr=10:nf=<room floor>` when a room tone applies; 48 kHz 32-bit float intermediate).
- [x] Tests: `click_lands_on_same_sample_after_clean` (ffmpeg-gated test, skipped with a message if ffmpeg is missing), `mismatched_room_tone_skips_nr`, `gated_room_tone_skips_nr`, `newest_matching_room_tone_wins`. Commit `feat(core): clean takes with a high-pass and room-tone noise reduction`.

### Task 5: Loudness, formats and the export pipeline

**Files:** modify `crates/takewise-core/src/export.rs`, `project.rs` (`ExportConfig`), `crates/takewise/src/cli.rs` (flags), `README.md` (export section).

- [x] Pipeline as the design (clean takes → place in Rust at 48 kHz float → measure `ebur128` integrated and true peak → one static gain → resample once with soxr if 44.1 kHz → dither with `aformat` triangular when 16-bit → encode WAV or FLAC → re-measure, correct once). Manifest: `processing`, `sample_rate`, `source_sample_rate`, `bit_depth`, `format`, `loudness_lufs`, `true_peak_dbtp`, per-line `noise_reduction` note, destination ranges at the export rate. Tests: `raw_default_unchanged`, `clean_hits_minus_16_lufs_within_half_lu`, `true_peak_never_above_minus_one`, `export_44k_16bit_flac_ranges_and_length`, `format_switch_leaves_one_file`, `raw_wav_needs_no_ffmpeg`, CLI `export_flags_override_config`. Commit `feat: clean export with loudness, formats and sample rates`.

### Task 6: Verification and docs

- [x] `mise run check` → PASS. Run `clean` export on a scratch project built from Niklas's eight hand-cut lines placed at the Kindred starts (read-only inputs, scratch output deleted after), report achieved LUFS/true peak and spot-check timing against `raw` with the click method. README and design updated where behaviour was settled. Commit `docs: describe the setup check and clean export`.
