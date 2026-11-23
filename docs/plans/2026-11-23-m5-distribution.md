# Milestone 5: Distribution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Someone can install Takewise with one Homebrew command, get the speech model with one click or command, and an agent can run the whole loop from a short guide.

**Architecture:** A model installer in `takewise-core` (HTTP download with resume and SHA-256), surfaced in the CLI (`takewise models install|status`) and the window (button with progress). Packaging follows Focal's `scripts/bundle`, `scripts/release` and its Homebrew cask template; release steps that publish stay manual.

**Tech Stack:** Rust 1.99.0; an HTTP client already in the dependency tree if any, else `ureq` (rustls) with minimal features; `sha2` (present).

**Spec:** `docs/design.md` "Distribution". Reference: `~/Projects/focal/{scripts,packaging,RELEASE.md}` and the hub's Mac app signing memory (Developer ID, the `keywink-notary` notary profile).

## Global Constraints

- Never publish (no `gh release create`, no tap push, no notarization) without Niklas's go-ahead in the parent session; build and verify locally only.
- The model URL and SHA-256 are pinned constants; a mismatch deletes the partial file and fails with a clear message.
- Downloads resume with HTTP `Range` from a `.part` file; progress reports bytes and percent.
- The CLI symlink installer never overwrites a file that is not a symlink to a Takewise bundle.

## Review Focus

1. **Network drops mid-download** → the next run resumes from the `.part` size. (Task 1 test against a local HTTP fixture server that cuts the connection.)
2. **Corrupted download** → SHA-256 mismatch is reported, the file is not installed. (Task 1 test.)
3. **No write access to `~/.local/bin`** → the menu item explains and suggests the Homebrew route. (Task 3 test.)
4. **Microphone permission denied** → `doctor` and the window show the System Settings path. (Task 3 test with a fake permission state.)
5. **Agent guide drift** → every command in `docs/agents.md` is executed by a test against a scratch project (except `record`). (Task 4 test.)

---

### Task 1: Model installer

**Files:** create `crates/takewise-core/src/models.rs`; modify `crates/takewise/src/cli.rs` (`models install`, `models status`), `doctor`.
- [ ] Pin `ggml-large-v3-turbo.bin` from `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin` with its SHA-256 (compute it from the already-downloaded file at `~/.local/share/takewise/models/ggml-large-v3-turbo.bin` and cross-check against the Hugging Face file listing). Tests with a local HTTP fixture: `downloads_and_verifies`, `resumes_after_cut`, `rejects_bad_hash`, `status_reports_missing_and_present`. Commit `feat: install the speech model`.

### Task 2: Window model prompt

**Files:** `crates/takewise/src/window/*`.
- [ ] When the model is missing, a banner "Install the speech model (1.5 GB)" with a button and a progress bar; checks run without words until done, then queued takes are checked. Tests (fake installer): `banner_when_model_missing`, `progress_then_checks_resume`. Commit `feat(app): offer to install the speech model`.

### Task 3: Packaging

**Files:** `packaging/Info.plist`, `packaging/takewise.rb` (cask template), `scripts/bundle`, `scripts/release`, `RELEASE.md`, `crates/takewise/src/window/menus.rs` (Install Command Line Tool), permission explanations.
- [ ] Mirror Focal: versioned bundle, Developer ID signing with hardened runtime and the audio-input entitlement, notarization step behind `--notarize`, a cask with `depends_on formula: ["whisper-cpp", "ffmpeg"]` and a `binary` stanza. Menu item links `~/.local/bin/takewise`. Tests: `cli_link_refuses_foreign_file`, `cli_link_creates_symlink`, `mic_denied_message`. Local verification: `scripts/release` without `--notarize` builds and `codesign --verify --deep --strict` passes. Commit `build: package Takewise for Homebrew`.

### Task 4: Docs for people and agents

**Files:** `README.md`, create `docs/agents.md`, a test `crates/takewise/tests/agents_doc.rs`.
- [ ] README: install, first session (setup check → record → choose → export), keys, the Resolve recipe, agent use. `docs/agents.md`: an agent's full loop with exact commands and JSON shapes (`init`, write `script.toml`, `status --json`, `check`, `judge`, `choose`, `export`, Resolve placement via the manifest). The test extracts the fenced shell commands from `docs/agents.md` and runs them against a scratch project with fake tools. Commit `docs: guide people and agents through Takewise`.

### Task 5: End-to-end rehearsal (with Niklas)

- [ ] Parent session coordinates: clean install from the locally built cask (or `scripts/bundle`), model install, setup check on the RØDECaster, narrate the Kindred promo, an interrupted take (quit mid-take, reopen, take recovered), checks, clean export, import into Resolve with markers, frame-aligned spot check. Notes in `docs/rehearsal-2026-11.md`.
