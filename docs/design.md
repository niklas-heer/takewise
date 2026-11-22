# Takewise v1 design

Status: draft for council review, 2026-11-22.

## Why

Narrating the 51-second Kindred promo in DaVinci Resolve took longer than the narration itself. The track would not arm until the mic was patched in a non-scriptable dialog, Resolve played the voice back with a delay ("super irritating"), the script lived in marker notes nobody could read while speaking, and an agent then had to hunt down the takes and transcribe them to find out whether the words were right.

Takewise is a small macOS app for exactly that job: speak a script over a finished film, line by line, retake what went wrong, and hand the chosen takes to an agent and to Resolve. The main win is the agent interface. Agents write the script, read every take's verdict, and place the result in Resolve; the human gets one focused screen for speaking.

## Goals

- Record narration over a film with the line to read shown large, timed to the picture.
- Record any number of takes per line; never lose a take.
- After every take, say within seconds whether it is good enough: wrong or missing words, overrun of the line's slot, clipping or noise, each with a reason.
- Rank the takes of a line and propose one; the narrator overrules with one key.
- Let the narrator fix a line's wording in place and set a line's start by pressing a key while the film plays.
- Expose everything to agents through a plain folder format and a CLI with JSON output.
- Export one narration WAV aligned to the film's first frame, plus a manifest of timings.

## Non-goals for v1

- A script editor beyond fixing a line's text and setting its start. Agents write `script.toml`.
- Waveform editing, effects, mixing, multiple voices or tracks.
- Splicing single words into a take. Retakes replace whole lines; long lines are split into two.
- Judging delivery (energy, warmth) with an audio-understanding model. The ledger accepts judgments from outside (`takewise judge`), so an agent can add this later without a format change.
- Retiming the film to the voice. The manifest gives agents the measured timings to do that.
- Driving Resolve from Takewise. Agents place the export in Resolve with their own Resolve tools.
- Windows and Linux. Apple Silicon macOS only.

## Anti-goals

- Becoming a DAW. Takewise ends at chosen takes plus timings.
- The AI rejecting or deleting takes on its own. It flags and ranks; the narrator decides.
- Software monitoring of the narrator's own voice. Takewise never plays the mic input back.
- Needing Resolve, a network connection or a cloud account to record and check.

## Approaches

| Approach | Trade-off |
| --- | --- |
| Swift app (AppKit) with AVFoundation | Easiest media path (AVPlayerView, AVAudioEngine). A second language beside the Rust CLI; Niklas has had trouble with SwiftUI and AppKit is verbose. |
| **Rust with GPUI and gpui-kit, AVFoundation through objc2** (chosen) | One language for app, CLI and checks; same stack as Focal, which Niklas already ships. Video inside a GPUI window is unproven, so a spike comes first. |
| Tauri or a web page | Fast UI, but browser capture adds processing and unclear latency, and playback sync is harder to trust. |

Niklas chose GPUI with gpui-kit on 2026-11-22, with the middle ground for script editing (agent drafts, app fixes text and sets starts).

## Project folder

A project is a directory, by convention `<name>.takewise/`:

```text
kindred-promo.takewise/
  project.toml        # film, audio settings, check settings
  script.toml         # the lines, edited by agents and by the app
  takes/<line-id>/001.wav
  takes.jsonl         # append-only ledger
  export/narration.wav
  export/manifest.json
```

`project.toml`:

```toml
format = 1
film = "../renders/kindred-promo-v2.mp4"   # relative to the project dir, or absolute
sample_rate = 48000

[input]
device = "RODECaster Duo Main Stereo"      # matched by name
channel = 1                                 # 1-based input channel, mono
latency_offset_ms = 0                       # added to measured latency; set by calibration

[record]
preroll_s = 2.0
tail_s = 1.5
film_gain_db = -12.0                        # film audio level in the headphones while recording

[check]
whisper_model = "ggml-large-v3-turbo.bin"   # resolved in the models directory
language = "en"
```

`script.toml`:

```toml
[[line]]
id = "hook"                 # stable slug; takes and ledger refer to it
start = 0.0                 # seconds of film time where the line should begin
end = 5.0                   # end of the slot; optional, defaults to the next line's start
text = "I put our whole family tree together. Everyone looked at it once, and never came back."
note = "warm, a little wry"  # optional direction for the narrator
```

Times are seconds as decimals. Lines are kept in film order. Renaming a line's text keeps its takes; changing its `id` orphans them, which `takewise status` reports.

`takes.jsonl` is append-only; each line is one event with `at` (RFC 3339), `line`, `take` and a `kind`:

- `recorded`: file, `film_start` (film time of the first sample after latency correction), duration, device, channel.
- `checked`: transcript with word times, word diff against the line text as it was at check time, speech start and end, overrun, peak and clipping, noise floor, `verdict` (`keep`, `retake`, `listen`) and `reasons`.
- `judged`: an outside judgment (`by`, `verdict`, `score` 0–1, `reason`), for example from an agent listening with an audio model.
- `chosen` and `unchosen`: the narrator's or an agent's pick.

The current state is a fold over the ledger. Nothing is rewritten in place, so a crash mid-take loses at most that take.

## The recording screen

```text
+--------------------------------------------------+----------------+
|                                                  | 1 hook     ✓ 3 |
|                  film (16:9)                     | 2 today    ! 2 |
|                                                  | 3 question  –  |
+--------------------------------------------------+ 4 quiz      –  |
|  0:05.2  ▓▓▓▓▓▓▓▓▓▓░░░░░░░  slot 3.0 s           | ...            |
|                                                  |                |
|  So I built Kindred around a page worth          | takes of 2:    |
|  opening again.                                  |  1 retake: 'a' |
|                                                  |    missing     |
|  next: One question a week, and every answer…    |  2 keep  ★     |
+--------------------------------------------------+----------------+
 Space record/stop · Enter keep · R retake · P play in context · M mark start · E edit
```

- The current line is large; the next line is dimmed below it. A bar shows the slot running out; the words fill from left to right at an even pace across the slot (a karaoke-style pace guide, by character count).
- **Record (Space):** the film starts `preroll_s` before the line's start, with the previous line's chosen take audible; a 3-2-1 count-in shows on screen; recording runs until Space again or `tail_s` after the slot ends. Film audio plays at `film_gain_db` in the headphones; the mic is never monitored.
- **After the take:** checks run and the verdict shows next to the take within a few seconds: `keep`, `retake` with reasons (missing "a", 0.6 s over the slot), or `listen` when the checks are unsure. The best take is starred.
- **Enter** chooses the selected take, **R** records another, **P** plays the line in context (pre-roll, take, tail) with the chosen or selected take, **↑/↓** move between lines.
- **M** sets the current line's start to the playhead while the film plays (the "timing from the video"). **E** edits the line's text in place. Both write `script.toml`.
- Settings: input device and channel with a live level meter, chosen once and stored in `project.toml`.

## Checks

All local, run after each take and by `takewise check`:

- **Words:** transcribe the take with whisper.cpp (`whisper-cli`, a Homebrew dependency) *without* the script as a prompt, so the model does not hear what it expects. Normalize case, punctuation and numbers, then align words against the line text and report missing, extra and substituted words with times.
- **Timing:** speech start and end from word times, cross-checked with an energy threshold. Report the start offset against the slot start and any overrun past the slot end.
- **Sound:** peak dBFS, clipped samples, integrated loudness and the noise floor before speech starts.
- **Verdict:** deterministic rules in the core, each producing a reason. Word errors or clipping give `retake`; overrun beyond a tolerance gives `retake`; low transcription confidence on a differing word gives `listen`; otherwise `keep`.
- **Ranking:** among a line's takes, prefer no word errors, then fitting the slot, then no clipping, then outside judgments, then the newest. The top take is proposed; it is chosen automatically only when the line has no choice yet.

## CLI

One binary, `takewise`. Every command takes the project directory and supports `--json`.

| Command | Does |
| --- | --- |
| `takewise init <dir> --film <path>` | Create a project with an empty script. |
| `takewise record <dir>` | Open the recording window. |
| `takewise status <dir>` | Lines, takes, verdicts, chosen takes, orphans. |
| `takewise check <dir> [--line ID] [--take N]` | Re-run checks; appends `checked` events. |
| `takewise judge <dir> <line> <take> --verdict … --reason … --by …` | Add an outside judgment. |
| `takewise choose <dir> <line> <take>` | Choose a take. |
| `takewise export <dir>` | Write `export/narration.wav` and `export/manifest.json`. |
| `takewise doctor` | Check whisper-cli, the model, microphone access and the input device. |

Exit codes: 0 success, 1 a finding (for example `status` with lines lacking a chosen take when `--strict`), 2 usage or setup errors.

## Export and Resolve handoff

`narration.wav` is 48 kHz mono 24-bit, starting at film time zero and as long as the film. Each chosen take is placed at its `film_start`, trimmed to its speech plus 150 ms on either side, with 10 ms fades. `manifest.json` lists each line with the take file, its film in and out, speech in and out, and word times.

For Resolve, an agent imports `narration.wav` onto a new audio track at the timeline start, or places the individual takes at the manifest's positions, and adds a marker per line. The README documents this recipe; Takewise itself has no Resolve code.

## Architecture

A Cargo workspace:

- `takewise-core`: the project format, ledger, script editing, word alignment, verdict rules, ranking, export and audio analysis. No GUI, no unsafe code, fully tested.
- `takewise-media`: the macOS bridges, the only crate allowed `unsafe`: film playback with AVPlayer, video frames for GPUI, audio capture from a chosen device and channel, and the clock mapping between them.
- `takewise`: the binary, CLI subcommands and the GPUI recording window.

**Playback and sync.** AVPlayer plays the film. Frames reach GPUI through `AVPlayerItemVideoOutput` as `CVPixelBuffer`s drawn with GPUI's surface element if the pinned GPUI exposes it, otherwise through an `AVPlayerLayer` placed under GPUI's view. Capture opens the chosen device and channel and timestamps buffers in host time. Film time maps to host time through the player's clock, so each take's `film_start` is the host time of its first sample converted to film time, minus measured input latency, plus `latency_offset_ms`.

**Transcription.** Shell out to `whisper-cli` with JSON output and word timestamps rather than linking whisper.cpp, so builds stay simple. `takewise doctor` reports a missing binary or model.

## Milestones

0. **Media spike (throwaway):** a GPUI window plays a film in sync; capture records from a chosen device and channel to WAV with host timestamps; a clap test shows the film-to-take offset within ±20 ms. Decides surface versus layer and the capture API.
1. **Core and CLI:** format, ledger, status, choose, judge, export, audio analysis, alignment and verdict rules, tested with fixtures (macOS `say` produces speech fixtures).
2. **Checks:** whisper-cli integration, `check`, `doctor`.
3. **Recording window:** the screen above on the spike's media path.
4. **Ship:** app bundle, README with the Resolve recipe, agent instructions.

## Constraints

- Stable Rust pinned like Focal (`rust-toolchain.toml`), edition 2024, Focal's lint baseline, `unsafe` denied outside `takewise-media`.
- mise for tasks (`build`, `test`, `check`, `run`, `clean`), vrdx decision records in `decisions/`.
- MIT license.

## Left to the builder

Exact word-alignment algorithm, number normalization, loudness measurement method, verdict thresholds (start with ±0.3 s start offset, 0.25 s overrun tolerance, −1 dBFS clipping), the whisper model directory, and the GPUI layout details.
