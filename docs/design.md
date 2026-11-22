# Takewise v1 design

Status: revised after council rounds 1 (Claude Fable 5.2: 6/10, GPT Sol 6.2: 7/10) and 2 (8.5/10, 8/10), 2026-11-22.

## Why

Narrating the 51-second Kindred promo in DaVinci Resolve took longer than the narration itself. The track would not arm until the mic was patched in a non-scriptable dialog, Resolve played the voice back with a delay ("super irritating"), the script lived in marker notes nobody could read while speaking, and an agent then had to hunt down the takes and transcribe them to find out whether the words were right.

Takewise is a small macOS app for exactly that job: speak a script over a finished film, line by line, retake what went wrong, and hand the chosen takes to an agent and to Resolve. The main win is the agent interface. Agents write the script, read every take's verdict, add their own judgments, and place the result in Resolve; the human gets one focused screen for speaking.

## Goals

- Record narration over a film with the line to read shown large, timed to the picture.
- Record any number of takes per line; never lose a take.
- After every take, say within seconds whether it is good enough: wrong or missing words, overrun of the line's slot, clipping or silence, each with a reason.
- Rank the takes of a line and propose one; the narrator commits a choice with one key.
- Let the narrator fix a line's wording in place and set a line's start by pressing a key while the film plays.
- Expose everything to agents through a plain folder format and a CLI with JSON output, including a way for an agent to add its own judgment of a take.
- Export one narration WAV aligned to the film's first frame, plus a manifest of exact sample positions.

## Non-goals for v1

- A script editor beyond fixing a line's text and setting its start. Agents write `script.toml`.
- Waveform editing, effects, mixing, multiple voices or tracks, a settings screen.
- Splicing single words into a take. Retakes replace whole lines; long lines are split into two.
- Judging delivery with an audio model inside Takewise. Agents do that from outside and record it with `takewise judge`.
- Retiming the film to the voice. The manifest gives agents the measured timings to do that.
- Driving Resolve from Takewise. Agents place the export in Resolve with their own Resolve tools.
- Hearing the previous line's take during pre-roll (later).
- Loudness normalization or integrated loudness.
- Windows and Linux. Apple Silicon macOS only.

## Anti-goals

- Becoming a DAW. Takewise ends at chosen takes plus timings.
- Anything choosing a take on its own. Checks and judgments flag and rank; only the narrator (Enter) or an explicit `takewise choose` commits a choice.
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
  takes/<line-id>/001.json  # the take's recording metadata, written before the WAV is published
  takes.jsonl         # append-only ledger
  .lock               # advisory lock held while writing
  export/narration.wav
  export/manifest.json
```

`project.toml`:

```toml
format = 1
film = "../renders/kindred-promo-v2.mp4"   # relative to the project dir, or absolute
sample_rate = 48000

[film_info]                                 # written by init (ffprobe), refreshed by `takewise status --refresh-film`
duration_s = 51.5
size = 12345678
modified = "2026-11-22T20:05:11Z"

[input]
device = "RODECaster Duo Main Stereo"      # matched by name; `takewise doctor` lists devices
channel = 1                                 # 1-based input channel, recorded as mono
latency_offset_ms = 0                       # manual correction from the loopback test

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
id = "hook"                 # stable slug [a-z0-9-], unique; takes and ledger refer to it
start = 0.0                 # seconds of film time where the line should begin
end = 5.0                   # end of the slot; optional, defaults to the next line's start or the film's end
text = "I put our whole family tree together. Everyone looked at it once, and never came back."
note = "warm, a little wry"  # optional direction for the narrator
```

Validation (`status` reports, the app refuses to record until fixed): ids unique and slug-safe, starts in increasing order, `end > start`, everything within the film's duration. Changing a line's `id` orphans its takes, which `status` reports.

### Writing files safely

The app, the CLI and agents editing by hand may all touch the folder.

- **One lock.** Every Takewise writer takes an exclusive advisory lock on `.lock` (`flock`) for the duration of a write: a script edit, a take allocation, a ledger append. Writes are short; the lock is never held while recording.
- **`script.toml` edits** are format-preserving (`toml_edit`): re-read the file under the lock, change one field of one line by id, write a temp file, rename over. The app watches the file and reloads; an edit made in the app against a version that changed on disk is rejected with a message instead of overwriting it. Agents may edit the file directly while the app is closed. Only `takewise script set` (below) is protected against concurrent edits; a direct edit while the app is open can be overwritten.
- **Takes.** Recording writes to `takes/<line>/.<uuid>.wav.partial`, with its metadata (everything the `recorded` event needs) already in `takes/<line>/.<uuid>.json.partial` when capture starts. On stop, the WAV is finalized and fsynced, then under the lock the next free number is allocated, the metadata renamed to `NNN.json`, the WAV to `NNN.wav`, and the `recorded` event appended. Take numbers are allocated only under the lock.
- **Ledger appends** are one `write` of one complete line with `O_APPEND`, under the lock; when the file does not end in a newline (a torn last line from a crash), the writer first appends one, so the new event starts on its own line. Readers skip any line that does not parse and `status` reports it.
- **Recovery.** A `NNN.wav` without a `recorded` event (crash between rename and append) is adopted from its `NNN.json` with a `recorded` event marked `recovered: true` the next time the app or `takewise status --repair` runs; without its `NNN.json` it is only reported. A leftover `.partial` is reported and kept, never deleted.

### The ledger

`takes.jsonl` is append-only. Each line is one event: `{"v": 1, "at": RFC 3339, "kind": …, "line": id, "take": n, …}`.

- `recorded`: `file`, `sample_rate`, `samples`, `device`, `channel`, `line_offset_s` (film time of the take's first sample minus the line's start, at record time; negative because capture begins with pre-roll), and a `snapshot` of the line at record time (`text`, `start`, `end`).
- `checked`: `snapshot` checked against (text and slot), transcript with word times, word differences, `speech` (start and end in source seconds, from the energy detector), `overrun_s`, `peak_dbfs`, `clipped_samples`, `noise_floor_dbfs`, `verdict` (`keep`, `retake`, `listen`), `reasons` (list of `{code, message}`), `checker` (whisper model and Takewise version).
- `judged`: an outside judgment: `snapshot` judged against, `by` (free text, for example `claude-opus-5-5`), `verdict`, optional `score` 0–1, `reason`.
- `chosen`: `by` (`narrator` or the agent's name).

Snapshots store the resolved slot end, so moving a neighbour also changes them. Folding: a take's effective verdict is its latest `judged` event whose snapshot matches the current line, otherwise its latest matching `checked` event, otherwise none (`unchecked`). A judgment or check against old text or an old slot is shown as stale; checks are re-run, judgments are not. A line's choice is its latest `chosen` event. No other event kinds in v1.

## Timing model

Takes belong to their line, not to absolute film time. A take's position is `line.start + line_offset_s`, so when the narrator presses **M** or an agent moves a line's `start`, its takes move with it; overrun checks are re-run against the new slot.

Capture starts at the beginning of pre-roll (so the noise floor before speech is recorded) and stops at Space or `tail_s` after the slot's end. When a line starts less than `preroll_s` into the film, the film holds its first frame during the count-in and starts playing at film time zero; the capture's first samples then have negative film time and are cropped at export.

The clock mapping is the spike's job to prove. The model: capture buffers carry host timestamps; the player's timebase maps host time to film time. The film time a sample belongs to is the film time *heard by the narrator* when the sound was made, so the mapping must account for both input latency (mic to timestamp) and output latency (player to headphones). The spike establishes which of these the capture timestamp and the player clock already include, so neither is subtracted twice. `latency_offset_ms` is a manual correction set from a loopback test: a film with beeps at known times played through headphones held to the mic; the recorded beeps must land at their film times within ±20 ms, including at film time zero and after repeated seeks. A clap test is not sufficient. Bluetooth headphones are not supported for recording; `doctor` warns when the output device is Bluetooth.

## The recording screen

```text
+--------------------------------------------------+----------------+
|                                                  | 1 hook     ✓ 3 |
|                  film (16:9)                     | 2 today    ! 2 |
|                                                  | 3 question  –  |
+--------------------------------------------------+ 4 quiz      –  |
|  0:05.2  ▓▓▓▓▓▓▓▓▓▓░░░░░░░  slot 3.0 s   ▮▮▮░ mic | ...            |
|                                                  |                |
|  So I built Kindred around a page worth          | takes of 2:    |
|  opening again.                                  |  1 retake: 'a' |
|                                                  |    missing     |
|  next: One question a week, and every answer…    |  2 keep  ☆     |
+--------------------------------------------------+----------------+
 Space record/stop · Enter choose · R retake · P play in context · M mark start · E edit
```

- The current line is large; the next line is dimmed below it. A bar shows the slot running out, and the words fill from left to right at an even pace across the slot (karaoke-style pace guide, by character count; Niklas's original idea, kept because it is small). A level meter shows the input.
- **Record (Space):** the film starts `preroll_s` before the line's start; a 3-2-1 count-in shows on screen; recording runs until Space again or `tail_s` after the slot ends. Film audio plays at `film_gain_db` in the headphones; the mic is never monitored.
- **After the take:** the verdict shows next to the take as soon as the checks finish, until then "checking…". The top-ranked take gets a hollow star (proposed); a chosen take gets a filled one.
- **Enter** chooses the selected take, **R** records another, **P** plays the line in context (pre-roll, take, tail) with the selected take locked to the film, **↑/↓** move between lines and **←/→** between takes.
- **M** sets the current line's start to the playhead while the film plays, clamped between the previous line's start and the line's own slot end. **E** edits the line's text in place. Both write `script.toml` as described above.
- When the input device is missing or not running at 48 kHz, the screen says so, names the available devices, and does not record.

## Checks

All local, run after each take and by `takewise check`. Recording and choosing work without them; a failed check never blocks the narrator.

- **Speech and timing (primary, milestone 1 because export needs it):** an energy detector on the take finds speech start and end relative to the noise floor measured in the pre-roll. Overrun is speech end past the slot end; early start is speech start before the line's start.
- **Words:** transcribe with whisper.cpp (`whisper-cli`, Homebrew `whisper-cpp`) *without* the script as a prompt, so the model does not hear what it expects. Normalize case, punctuation and simple numbers, align the words with the line text and report missing, extra and substituted words with times. The exact output format of the installed `whisper-cli` is verified before relying on it.
- **Sound:** peak dBFS, clipped samples (at or above −0.1 dBFS), noise floor.
- **Verdict, first matching rule wins:**
  1. No speech detected → `retake` ("no speech").
  2. Clipping → `retake`.
  3. One or more missing or extra words → `retake`, naming them.
  4. Overrun beyond 0.25 s or early start beyond 0.3 s → `retake`, with the amount.
  5. Transcription failed or unavailable → `listen` ("words not checked").
  6. Only substitutions (likely mishearings of names or numbers) → `listen`, naming them.
  7. Otherwise → `keep`.
  Thresholds are tuned on real takes (the Kindred narration recorded on 2026-11-22), not only on synthetic `say` fixtures.
- **Ranking** among a line's takes, using effective verdicts: `keep` before `listen` and `unchecked` (equal) before `retake`; then fewer word differences; then smaller overrun; then outside `score`; then newest. The top take is proposed, never chosen automatically.

## CLI

One binary, `takewise`. Every command takes the project directory and supports `--json`. Exit codes: 0 success, 1 a finding (for example `export` refusing, or `status --strict` with problems), 2 usage or setup errors.

| Command | Does |
| --- | --- |
| `takewise init <dir> --film <path>` | Create a project with an empty script and the film's info. |
| `takewise record <dir>` | Open the recording window. |
| `takewise status <dir> [--strict] [--repair] [--refresh-film]` | Lines, takes, effective verdicts, proposed and chosen takes, validation problems, stale checks, orphans, film changes. |
| `takewise script set <dir> <line> --text … / --start … / --end …` | Edit one field of one line safely. |
| `takewise check <dir> [--line ID] [--take N]` | Run checks; appends `checked` events. |
| `takewise judge <dir> <line> <take> --verdict … --reason … --by … [--score …]` | Add an outside judgment. |
| `takewise choose <dir> <line> <take> [--by …]` | Choose a take. |
| `takewise export <dir> [--use-proposed]` | Write `export/narration.wav` and `export/manifest.json`. |
| `takewise doctor [<dir>]` | Check whisper-cli, the model, microphone access, input devices and their sample rates, the output device. |

## Export and Resolve handoff

`narration.wav` is 48 kHz mono 24-bit PCM, starting at film time zero and exactly as long as the film.

For each line, the chosen take is used; a line without one makes `export` fail with the line ids, unless `--use-proposed` uses the proposed take (the manifest records `by: "proposed"`); a line with no takes at all always fails. A used take in which no speech is detected also fails export, naming the line. From the take, the source range is its speech, extended by 150 ms on either side and clipped to the take. A source sample `i` lands at destination sample `round((line.start + line_offset_s) × 48000) + i`; trimming never shifts the speech. Ranges are cropped to `[0, film length)`. When two lines' destination ranges overlap, export fails and names both lines, so the narrator can choose another take or move a start. Each placed range gets 10 ms fades.

`manifest.json` lists the film (path, duration, size, modified) and per line: id, text, take file, `by` of the choice, source sample range, destination sample range, speech start and end in film seconds, and, once milestone 3 adds transcription, word times in film seconds.

For Resolve, an agent imports `narration.wav` onto a new audio track at the timeline start, or places the individual takes at the manifest's positions, and adds a marker per line. The README documents this recipe; Takewise itself has no Resolve code. The milestone that adds export verifies the WAV against the loopback beeps and imports it into Resolve once.

## Architecture

A Cargo workspace:

- `takewise-core`: the project format, lock and write protocol, ledger and folding, script edits, audio analysis, word alignment, verdict rules, ranking and export. No GUI, no unsafe code, fully tested.
- `takewise-media`: the macOS bridges, the only crate allowed `unsafe` (each block with a `SAFETY` comment): film playback with AVPlayer, video frames for GPUI, take playback locked to the film, audio capture from a chosen device and channel, and the clock mapping.
- `takewise`: the binary, CLI subcommands and the GPUI recording window.

**Playback.** AVPlayer plays the film. Frames reach GPUI through `AVPlayerItemVideoOutput` as `CVPixelBuffer`s drawn with GPUI's surface element if the pinned GPUI exposes it, otherwise through an `AVPlayerLayer` placed under GPUI's view. The spike decides.

**Microphone permission.** macOS grants the microphone to the app that owns the process. A bare binary started from a terminal or an agent harness inherits that host's permission, which is fragile. The spike decides whether `takewise record` must launch a bundled `Takewise.app` (with `NSMicrophoneUsageDescription`) so the permission belongs to Takewise.

**Transcription.** Shell out to `whisper-cli` with JSON output and word timestamps rather than linking whisper.cpp, so builds stay simple. `takewise doctor` reports a missing binary or model.

## Milestones

0. **Media spike (throwaway):** a GPUI window plays the film with a text overlay; capture records from a chosen device and channel with host timestamps; a take plays back locked to the film; the loopback test places beeps within ±20 ms at film zero and after seeks; microphone permission for a bare binary versus a bundle is settled. Decides surface versus layer, the capture API and how latency is accounted for.
1. **Core and CLI:** format, validation, lock and write protocol, ledger and folding, the energy speech detector, `init` (film info through `ffprobe`), `status`, `script set`, `choose`, `judge`, `export`, and `doctor` limited to tools, model and film (devices arrive with milestone 2); tests with fixtures.
2. **Recording window:** the screen above on the spike's media path: record, play in context, choose, mark start, edit text, level meter. After this milestone Niklas can narrate a film; agents can judge takes from the folder.
3. **Checks:** whisper-cli integration, alignment, verdict rules, ranking, `check`; thresholds tuned on real takes; verdicts in the window.
4. **Ship:** app bundle, README with the Resolve recipe, agent instructions.

## Constraints

- Stable Rust pinned like Focal (`rust-toolchain.toml`), edition 2024, Focal's lint baseline, `unsafe` denied outside `takewise-media`.
- mise for tasks (`build`, `test`, `check`, `run`, `clean`), vrdx decision records in `decisions/`.
- MIT license.

## Left to the builder

The exact word-alignment algorithm, number normalization, energy-detector parameters, the whisper model directory, and the GPUI layout details.
