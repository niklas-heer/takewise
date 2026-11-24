# AGENTS.md

Guidance for coding agents working in this repository.

## Project

Takewise is a macOS app for recording voiceover narration over a finished film: a teleprompter timed to the picture, takes per line, automatic checks of every take, and a folder format plus CLI that agents use to write the script, read verdicts and export the result. Read [README.md](README.md) for the goal and [docs/design.md](docs/design.md) for scope, the folder format, the CLI and milestones. Implementation plans live in `docs/plans/`. An agent that uses Takewise for narration, rather than developing it, follows [docs/agents.md](docs/agents.md); releases follow [RELEASE.md](RELEASE.md).

## Invariants

- **Never lose a take.** Takes are only ever added, and nothing changes or deletes a take or a ledger event. `takes.jsonl` is an append-only event history: its content only grows, though the file itself can be replaced. A single event is appended in place with `O_APPEND`. A batch that must arrive whole (`choose --passage`) is published by writing a synced copy that keeps every earlier byte, followed by the batch, and renaming it over the ledger, so a live reader must reopen the path to see a batch: a file kept open across it is the old one.
- **Flag, don't decide.** Checks and judgments propose and rank; only the narrator or an explicit `choose` picks a take. Nothing rejects a take on its own.
- **Monitoring only on request.** The app plays the microphone back only when the narrator turns monitoring on, and monitoring never changes what is recorded.
- **Agents are first-class.** Every state the app shows is readable through the CLI with `--json`; files stay plain TOML, JSONL and WAV.

## Stack

- Rust (pinned in `rust-toolchain.toml`), a Cargo workspace under `crates/`:
  - `takewise-core`: the UI-free model: folder format, ledger, script edits, checks, verdicts, ranking, export. Put logic here and test it.
  - `takewise-media`: macOS bridges (AVFoundation, CoreAudio) through `objc2`. The only crate allowed `unsafe`, with a `SAFETY` comment on every block.
  - `takewise`: the binary: CLI subcommands and the GPUI recording window (through `gpui-kit`, pinned exactly).
- Commands: `mise run build`, `mise run run -- <args>`, `mise run test`, `mise run check` (fmt, Clippy with `-D warnings`, also on the `screenshots` feature, tests), `mise run clean`, `mise run screenshots` (regenerates `docs/images/` for the README; see [docs/reference.md](docs/reference.md#regenerating-the-screenshots)).
- Runtime tools: `whisper-cli` (Homebrew `whisper-cpp`) for transcription; `ffmpeg` for film info, audio for the checks, the `clean` export, resampling and FLAC (`raw` WAV export at 48 kHz works without it).
- Debug builds are large; run `mise run clean` when done.

## Conventions

- Conventional commits, such as `feat(core): …` or `fix(cli): …`.
- Record lasting choices as vrdx decision records in `decisions/` (install with `brew install niklas-heer/tap/vrdx`): run `vrdx guide` first, create with `vrdx new "<title>" --body-file <file>`, and check with `vrdx validate`. Use `accepted` only for choices the maintainer made.
- Update `docs/design.md` when the design changes.
