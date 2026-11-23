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
- Waveform editing, mixing, multiple voices or tracks. (Export cleanup, below, is a fixed processing profile, not an effects rack.)
- Splicing single words into a take. Retakes replace whole lines; long lines are split into two.
- Judging delivery with an audio model inside Takewise. Agents do that from outside and record it with `takewise judge`.
- Retiming the film to the voice. The manifest gives agents the measured timings to do that.
- Driving Resolve from Takewise. Agents place the export in Resolve with their own Resolve tools.
- Hearing the previous line's take during pre-roll (later).
- Loudness control beyond the fixed `clean` export profile (one static gain to −16 LUFS, peaks at −1 dBTP).
- Windows and Linux. Apple Silicon macOS only.

## Anti-goals

- Becoming a DAW. Takewise ends at chosen takes plus timings.
- Anything choosing a take on its own. Checks and judgments flag and rank; only the narrator (Enter) or an explicit `takewise choose` commits a choice.
- Monitoring the narrator's voice without being asked. Monitoring is off until the narrator turns it on (changed 2026-11-23, see "Setup and sound").
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
  export/narration.wav  # or .flac, see "Setup and sound"
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
- **Takes.** Recording streams to `takes/<line>/.<uuid>.wav.partial` from a writer thread fed directly by the capture, with its metadata (everything the `recorded` event needs) already in `takes/<line>/.<uuid>.json.partial` when the film is scheduled. The writer holds an exclusive advisory lock (`flock`) on the partial WAV while recording and rewrites the WAV header and syncs at least once per second, so the file is a valid WAV up to the last flush. Every captured sample is also kept in memory until the take is committed: when the metadata or the WAV cannot be written, the window shows the error, keeps recording, and writes the take from memory on stop (retrying until it succeeds). On stop, the WAV is finalized and fsynced, the metadata rewritten with the line's position at that moment, then under the lock the next free number is allocated, the metadata renamed to `NNN.json`, the WAV to `NNN.wav`, and the `recorded` event appended; the WAV stays locked until then. When finishing the WAV fails, or the take could not be begun on disk, the WAV is written again from memory only while its lock is held (kept from the recording, else taken on the new file first), and the lock is held until the commit succeeds or the window exits, so a repair never adopts a take the window still owns. Take numbers are allocated only under the lock.
- **Ledger appends** are one `write` of one complete line with `O_APPEND`, under the lock; when the file does not end in a newline (a torn last line from a crash), the writer first appends one, so the new event starts on its own line. Readers skip any line that does not parse and `status` reports it as a notice.
- **Recovery.** A `NNN.wav` without a `recorded` event (crash between rename and append) is adopted from its `NNN.json` with a `recorded` event marked `recovered: true` the next time the app or `takewise status --repair` runs; without its `NNN.json` it is only reported. The metadata names its partial WAV, so a `NNN.json` whose WAV is still `.<uuid>.wav.partial` (crash between the two renames) is completed the same way; a `NNN.json` with no audio at all is only reported. A `.<uuid>.json.partial` whose `.<uuid>.wav.partial` holds audio and is not locked (a crash mid-take) is adopted the same way: the WAV's lengths are fixed from its size on disk, it is numbered, and its `recorded` event is marked `recovered: true`; a locked partial belongs to a live recording and is never touched. Any other leftover `.partial` is reported and kept, never deleted.

### The ledger

`takes.jsonl` is append-only. Each line is one event: `{"v": 1, "at": RFC 3339, "kind": …, "line": id, "take": n, …}`.

- `recorded`: `file`, `sample_rate`, `samples`, `device`, `channel`, `line_offset_s` (film time of the take's first sample minus the line's start when the take is stopped and committed; negative because capture begins with pre-roll), a `snapshot` of the line (`text` as it was when the take started, the text read; `start` and `end` at that moment), so a line moved with **M** during the take keeps the take at the film time it was recorded at, and a text edited during the take shows the take as recorded against old text, and optionally `dropouts`: how many gaps the capture had (buffers arriving later than the previous one ended, by more than 1 ms), each filled with silence so later audio keeps its timing. Takes recorded before it existed, and takes recovered after a crash, have no `dropouts`; readers treat it as unknown. Optionally `film_shift_ms`: the take's audio is contiguous and placed by its first sample, so when the film stalled or changed rate after the take started, the largest distance between where the film really was and where the take places it (until the film's end), recorded only when above 5 ms; the window then shows "film stalled during the take; placement may be off by N ms". Absent on takes without a shift and on takes recorded before it existed; readers ignore it when absent.
- `checked`: `snapshot` checked against (text and slot), transcript with word times, word differences, `speech` (start and end in source seconds, from the energy detector), `overrun_s`, `peak_dbfs`, `clipped_samples`, `noise_floor_dbfs`, `verdict` (`keep`, `retake`, `listen`), `reasons` (list of `{code, message}`), `checker` (whisper model and Takewise version).
- `judged`: an outside judgment: `snapshot` judged against, `by` (free text, for example `claude-opus-5-5`), `verdict`, optional `score` 0–1, `reason`.
- `chosen`: `by` (`narrator` or the agent's name).

Snapshots store the resolved slot end, so moving a neighbour also changes them. Folding: a take's effective verdict is its latest `judged` event whose snapshot matches the current line (text exactly, start and end within 1 µs), otherwise its latest matching `checked` event, otherwise none (`unchecked`). A judgment or check against old text or an old slot is shown as stale; checks are re-run, judgments are not. A line's choice is its latest `chosen` event of a recorded take; a choice of a take without a parseable `recorded` event is ignored and reported as a `choice-without-take` notice, so the line counts as unchosen. No other event kinds in v1.

## Timing model

Takes belong to their line, not to absolute film time. A take's position is `line.start + line_offset_s`, so when the narrator presses **M** or an agent moves a line's `start`, its takes move with it; overrun checks are re-run against the new slot.

Capture starts at the beginning of pre-roll (so the noise floor before speech is recorded) and stops at Space or `tail_s` after the slot's end; when the film ends first (the last line's slot runs to the film's end), it stops `tail_s` of host time after the film reached its end. Space starts capture first; once its first buffer arrives, the film is scheduled to start at least 300 ms ahead (the players need the lead), so a slow input never eats into the pre-roll. From the scheduled start on, the window reads the player's timebase back every frame and records each departure from the mapping in effect (a late start, a stall, a rate change) in the take's timebase history, which places the samples: the first read-back replaces the scheduled mapping for the whole take, count-in included, so a late start moves the take, and the take's first sample is placed with it when the take is committed; the prompter and the automatic stop follow the film as it really plays. The loopback calibration (`takewise calibrate`) runs this same start routine, so it measures the window's path. When a line starts less than `preroll_s` into the film, the film holds its first frame during the count-in and starts playing at film time zero; the capture's first samples then have negative film time and are cropped at export.

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
- **Record (Space):** the film starts `preroll_s` before the line's start; a 3-2-1 count-in shows on screen; recording runs until Space again or `tail_s` after the slot ends. Film audio plays at `film_gain_db` in the headphones; the mic is monitored only when the narrator has turned monitoring on.
- **After the take:** the verdict shows next to the take as soon as the checks finish, until then "checking…", with the reason that explains it below. Checks run one at a time on a worker thread, after every commit and whenever a reload finds takes without a check against their line as it is now (an edited text or a moved slot). When whisper-cli cannot be set up, takes still get their sound and timing checks, with `listen` and the reason. The top-ranked take gets a hollow star (proposed); a chosen take gets a filled one.
- **Enter** chooses the selected take, **R** records another, **P** plays the line in context (pre-roll, take, tail) with the selected take locked to the film, **↑/↓** move between lines and **←/→** between takes.
- **M** sets the current line's start to the playhead while the film plays, clamped between the previous line's start and the line's own slot end. **E** edits the line's text in place; the draft belongs to the line and the `script.toml` version it was begun on, so a script changed meanwhile refuses it, and a line removed meanwhile is never replaced by another. Both write `script.toml` as described above.
- When the input device is missing or not running at 48 kHz, the screen says so, names the available devices, and does not record. On the RØDECaster Duo the mic appears in the two-channel devices "Main Stereo" and "Chat"; which channel carries the mic depends on RØDE Central's routing, so the level meter is how the narrator confirms the choice.

## Checks

All local, run after each take and by `takewise check`. Recording and choosing work without them; a failed check never blocks the narrator.

- **Speech and timing (primary, milestone 1 because export needs it):** an energy detector on the take finds speech start and end relative to the noise floor measured in the pre-roll. Overrun is speech end past the slot end; early start is speech start before the line's start.
- **Words:** transcribe with whisper.cpp (`whisper-cli`, Homebrew `whisper-cpp`) *without* the script as a prompt, so the model does not hear what it expects. Normalize case, punctuation and contractions, and turn numbers into values on both sides ("1,500", "fifteen hundred" and "one thousand five hundred" are all 1500; years said in two parts, decades, ordinals, decimals and currency likewise; a decimal compares by value, so "3.50%" is "three point five percent"; "three dollars and fifty cents" is $3.50; cents not named stay a number of their own, so "five dollars twenty years ago" matches "$5 20 years ago" and "seven pounds eight ounces" "7 pounds 8 ounces", while the alignment still matches "three dollars fifty" against "$3.50"; a sign is kept, so "-5", "minus five" and "negative five" are −5, never 5). A number never runs across punctuation ("pick twenty, one at a time" is 20 and one, not 21), and "first" and "second" are ordinals only when attached ("twenty-first"), never in "a sixty second timer"; a number split in two on one side still matches its joined form on the other ("sixty second" and "62nd", "60-second" and "sixty-second"), a clock time with a colon on either side matches "eleven thirty", "11 30" or whisper's "11.30" on the other (without a colon, two numbers are never a time), and "alright" matches "all right". "Three fifty" for $3.50 stays a possible mishearing to listen to, since it may be 350. Then set aside the annotations whisper writes for sounds (`[BLANK_AUDIO]`, `(coughs)`, `[Music]`, `[background music playing]` over several words), then align the words with the line text and report missing, extra and substituted words with times, named as written in the script or said in the transcript ("second", not "2nd"). Two words on one side against their concatenation on the other ("every day" and "everyday", "every one" and "everyone") are one substitution, so a possible mishearing to listen to, since they often mean different things; two words joined by a hyphen are one word spelled two ways and match ("voice-over" and "voiceover", "e-mail" and "email"). A script word with an apostrophe accepts what it sounds like ("link's" a heard "links" or "link is", "family's" "families", "Niklas's" "Niklas"); a plain script word does not, so "the light is below" heard as "the lights below" is a difference. The exact output format of the installed `whisper-cli` is verified before relying on it.
- **Sound:** peak dBFS, clipped samples (at or above −0.1 dBFS), noise floor.
- **Verdict, first matching rule wins:**
  1. No speech detected → `retake` ("no speech").
  2. Clipping → `retake`.
  3. One or more missing or extra words → `retake`, naming them.
  4. Overrun beyond 0.25 s or early start beyond 0.3 s → `retake`, with the amount. Early start is measured from the energy speech start, unless at least 120 ms of silence lies between it and the first transcribed word: then the sound before is a breath or a noise before the words (seen on the real Kindred take, line 4: a breath 0.52 s early), and the speech starts where the sound resumes after the last such gap, not at the first word's DTW time, which lands late. Without such a gap the energy start is the first word's onset, however late DTW places the word (0.08 to 0.20 s after the real onset on the Kindred lines).
  5. Transcription failed or unavailable → `listen` ("words not checked"). This includes a tool that does not finish in time (`ffmpeg` 30 s, `whisper-cli` 120 s), which is stopped, and output with word times missing, out of order or out of range. Such a check does not count as current: `takewise check` and the window's queue run it again.
  6. Only substitutions (likely mishearings of names or numbers) → `listen`, naming them.
  7. A sound whisper annotates inside the speech, such as `(coughs)` or `[Music]` → `listen` (`non-speech-sound`), naming it; an annotation outside the speech (`[BLANK_AUDIO]` in the pre-roll) is dropped.
  8. Otherwise → `keep`.
  Thresholds are tuned on real takes (the Kindred narration recorded on 2026-11-22), not only on synthetic `say` fixtures.
- **Re-checks:** when only a line's slot moved since a take's check (the same text), checking it again recomputes the verdict from that check's words and a fresh energy analysis, without running whisper; the sound finding of rule 7 carries over. The `checked` event's `checker` names the whisper model and how the audio is prepared before transcription (`pre=none` for now; a high-pass filter would be, say, `pre=hp80+norm`); a check by another model or preprocessing is transcribed again, as is a take whose current check is re-run explicitly.
- **Ranking** among a line's takes, using effective verdicts: takes whose file is missing rank last and are never proposed; then `keep` before `listen` and `unchecked` (equal) before `retake`; then fewer word differences; then smaller overrun; then outside `score`; then newest. The top take is proposed, never chosen automatically.

## CLI

One binary, `takewise`. Every command takes the project directory and supports `--json`. Exit codes: 0 success, 1 a finding (for example `export` refusing, or `status --strict` with problems), 2 usage or setup errors.

| Command | Does |
| --- | --- |
| `takewise init <dir> --film <path>` | Create a project with an empty script and the film's info. |
| `takewise record <dir>` | Open the recording window. |
| `takewise status <dir> [--strict] [--repair] [--refresh-film]` | Lines, takes, effective verdicts, proposed and chosen takes, validation problems, stale checks, orphans, film changes. Findings are `problems` (validation, missing chosen take files, film missing or changed) or `notices` (crash leftovers such as torn or bad ledger lines, `.partial` and unadoptable files; choices of takes without a `recorded` event; stale chosen takes; chosen takes whose speech overlaps, `speech-overlap`; missing files of other takes). `--strict` fails on problems and lines without a chosen take, never on notices. |
| `takewise script set <dir> <line> --text … / --start … / --end …` | Edit one field of one line safely. |
| `takewise check <dir> [--line ID] [--take N] [--stale-only]` | Run checks; appends `checked` events. Without `--line`, every take without a check against its line as it is now; `--line` (and `--take`) re-run those takes even when checked; `--stale-only` keeps takes whose latest check was made against older text or an older slot. A `retake` verdict exits 0; whisper-cli, `ffmpeg` or the model missing exits 2. |
| `takewise judge <dir> <line> <take> --verdict … --reason … --by … [--score …]` | Add an outside judgment. |
| `takewise choose <dir> <line> <take> [--by …]` | Choose a take; refuses one whose file is missing (exit 1, `missing-file`). |
| `takewise export <dir> [--use-proposed] [--format …] [--bit-depth …] [--sample-rate …] [--processing raw\|clean]` | Write `export/narration.wav` or `.flac` and `export/manifest.json`. |
| `takewise calibrate <dir> [--write] [--output <device>]` | Run the loopback latency test; `--write` stores the offset and the device pair. |
| `takewise doctor [<dir>]` | Check whisper-cli, the model and `ffmpeg` (transcribing 1 s of silence to prove they work together), microphone access, input devices and their sample rates, the output device. |

## Export and Resolve handoff

The narration file (`narration.wav` or `narration.flac`) is mono, starting at film time zero and exactly as long as the film. `[export]` sets its format, sample rate and bit depth for both processing profiles; a project without `[export]` gets 48 kHz 24-bit WAV with `raw` processing. Export removes a narration file of the other format, so only one narration file sits beside the manifest.

For each line, the chosen take is used; a line without one makes `export` fail with the line ids, unless `--use-proposed` uses the proposed take (the manifest records `by: "proposed"`), which it refuses with `no-acceptable-take` when that take's effective verdict is `retake`; a line with no takes at all always fails. A used take in which no speech is detected also fails export, naming the line. A used take recorded against different text than the line has now is exported, with a `recorded-against-old-text` warning in the manifest (and a notice in `status`). From the take, the source range is its speech, extended by 150 ms on either side and clipped to the take. A source sample `i` lands at destination sample `round((line.start + line_offset_s) × 48000) + i`; trimming never shifts the speech. Ranges are cropped to `[0, film length)`; a take whose speech (without the margin) lies entirely outside that range fails export with `speech-outside-film`, and one whose speech lies partly outside is exported with a `speech-cropped` warning in the manifest that gives the unclamped speech times (the line's `speech_film_s` is clamped to the film). When the speech (without the margins) of two used takes overlaps, export fails with `overlap` and names both lines, so the narrator can choose another take or move a start; `status` shows the same for chosen takes beforehand as a `speech-overlap` notice. When only their margins overlap, the gap between the two speeches is split at its midpoint, and each take keeps its margin up to there. Each placed range gets 10 ms fades.

Export holds the project lock while it reads and folds the project and writes both files to temp names; it then removes the old manifest, renames the WAV into place, and renames the manifest last, so an interrupted export never leaves an old manifest beside a new WAV. A refused export writes nothing and removes an existing `manifest.json`, so no stale pair looks valid (the export is regenerable; takes are never touched).

`manifest.json` lists the film (path, duration, size, modified), the WAV it describes (`narration`: file, size, SHA-256) and per line: id, text, take file, `by` of the choice, source sample range, destination sample range, speech start and end in film seconds, and `words_film_s`: the transcribed words (`{text, start, end}`, film seconds) from the take's latest check against the line as it is now, omitted when there is none.

For Resolve, an agent imports the narration file onto a new audio track at the timeline start, or places the individual takes (always the raw originals) at the manifest's positions, and adds a marker per line. The README documents this recipe; Takewise itself has no Resolve code. The milestone that adds export verifies the narration file against the loopback beeps and imports it into Resolve once.

## Architecture

A Cargo workspace:

- `takewise-core`: the project format, lock and write protocol, ledger and folding, script edits, audio analysis, word alignment, verdict rules, ranking and export. No GUI, no unsafe code, fully tested.
- `takewise-media`: the macOS bridges, the only crate allowed `unsafe` (each block with a `SAFETY` comment): film playback with AVPlayer, video frames for GPUI, take playback locked to the film, audio capture from a chosen device and channel, and the clock mapping.
- `takewise`: the binary, CLI subcommands and the GPUI recording window.

The media path below was proven by the milestone 0 spike on 2026-11-22 (decision "Play, capture and align media through AVPlayer, GPUI surfaces and cpal").

**Playback.** AVPlayer plays the film. `AVPlayerItemVideoOutput` delivers NV12 full-range `CVPixelBuffer`s (Metal-compatible, IOSurface-backed), polled once per frame and drawn with GPUI's `surface()` element; text draws on top of it. A second AVPlayer started at the same host time plays a take locked to the film (within 0.01 ms in the spike).

**Capture.** cpal opens an input stream on the named device and takes one channel; optional monitoring taps this stream (see "Setup and sound") and never opens a second capture. Each buffer's film time is `timebase(callback_host_time + i / sample_rate − L_in) + latency_offset_ms`, where `L_in` is the device's input latency plus safety offset plus stream latency from CoreAudio (`objc2-core-audio`). cpal's own capture timestamp is not used, because it leaves out the stream latency. AVPlayer's clock already accounts for output latency, so it is not subtracted. Changes of the player timebase (seeks, rate changes) are kept as a history, so a buffer is mapped with the timebase that was valid when it was captured. For a start scheduled at a later host time, the player's own timebase stays stopped until that moment, so the history takes the scheduled mapping (film time at the start host time, rate 1) from the call on; once playing, the player's timebase matches it within 0.02 ms. Before the first take after opening, the film plays briefly muted, so a cold output device does not shift the first take.

**Calibration.** One loopback measures only the total of input and output latency, so `latency_offset_ms` is calibrated per device pair with the loopback test. In the spike, beeps landed 31.5 ms late (standard deviation 0.01 ms) on the MacBook's speakers and mic, at film time 0.1 s and after seeks; CoreAudio's reported latencies alone left −19.5 ms, a calibrated offset under 1 ms. `takewise calibrate --write` stores the offset only when every detected beep landed within 20 ms of the mean error; otherwise it refuses (exit 1, `inconsistent-beeps`) and leaves `latency_offset_ms` as it is.

**Microphone permission.** A bare binary inherits the permission of the app that launched it, and capture hangs while that app's prompt is unanswered. `takewise record` therefore launches a signed `Takewise.app` (with `NSMicrophoneUsageDescription`) so the permission belongs to Takewise; release builds are Developer ID signed, since ad hoc permission grants reset on every rebuild. The other CLI commands stay a plain binary.

**Transcription.** Shell out to `whisper-cli` with JSON output and word timestamps rather than linking whisper.cpp, so builds stay simple. `takewise doctor` reports a missing binary or model.

## Setup and sound

Added 2026-11-23 at Niklas's request and revised after council review (Claude Fable 5.2: 6/10, GPT Sol 6.2: 7/10). The aim is outcomes good enough to use without post-production, while staying easy: the app guides the narrator to a good setup instead of expecting audio knowledge.

### Settings schema

New tables in `project.toml`; a project without them behaves as before (no monitoring, `raw` export):

```toml
[output]
device = "RODECaster Duo Main Stereo"   # where film audio and monitoring play

[monitor]
enabled = false
volume_db = -6.0

[calibration]                            # which device pair `input.latency_offset_ms` was measured on
input = "RODECaster Duo Main Stereo"
output = "RODECaster Duo Main Stereo"
buffer_frames = 512                      # the capture buffer during the measurement
date = "2026-11-23"

[export]
format = "wav"        # wav or flac
bit_depth = 24        # 16 or 24
sample_rate = 48000   # 48000 or 44100
processing = "clean"  # raw or clean; new projects default to clean

[setup]               # written by the setup check
date = "2026-11-23"
input = "RODECaster Duo Main Stereo"
channel = 1
output = "RODECaster Duo Main Stereo"
room_floor_dbfs = -68.0
speech_to_noise_db = 46.0
```

`[calibration]` also records the capture buffer size; when the input, the output or the buffer size differs from it, or a project has an offset but no `[calibration]` (measured before this section), `status` and the window warn that the latency calibration is stale. `status` likewise warns when the devices differ from `[setup]` or no setup check has run. The last device choices are also saved as defaults for new projects in `~/Library/Application Support/Takewise/settings.toml` (`TAKEWISE_SETTINGS` names another file); `init` starts a project with them. Device and route changes take effect between takes, never during one.

### Recording screen additions (milestone 4a)

**Audio panel** (toggled with **A**, usable while the film plays): input device, input channel and output device as pickers. Every channel of the selected input shows its own live meter, so the channel that carries the mic is obvious. A missing device is explained in plain words ("RODECaster Duo is not connected. Connect it or pick another input."). An input not at 48 kHz offers a "Switch to 48 kHz" button (sets the device's nominal rate through CoreAudio). Film volume and monitor volume are sliders and also keys that match the typed character, so they work on QWERTY and QWERTZ alike: `-` and `+` for the film, `,` and `.` for the monitor. They can change during a take.

**Monitoring** (**H**, also a visible toggle in the panel). Off by default. **H** works between takes only, since starting an output stream mid-take risks a dropout. A saved `enabled = true` resumes monitoring on open only when the devices are the ones it was enabled on, with the toggle visibly on; otherwise it starts off. When on, the existing capture callback also writes into a ring buffer that an output stream plays at `monitor.volume_db`; monitoring never opens a second capture and never changes the buffer size of the capture stream, which is fixed per session (128 frames, for every take whether monitored or not) so calibration stays valid. In v1 monitoring requires the input and output to be the same device (a shared clock, such as the RØDECaster's mic and headphone outputs); with different devices the toggle explains why it is unavailable. The panel shows the estimated mic-to-ear delay (input latency + buffer + output latency, labeled as an estimate) and warns above 15 ms, which rules out the built-in mic (about 50 ms). Monitoring is refused while the output is the built-in speakers (feedback). Because CoreAudio cannot tell whether an interface monitors in hardware, the panel always shows a hint: "If your interface has direct monitoring, use it instead and leave this off, or you will hear yourself twice." A monitoring failure mutes only the monitor; recording continues.

**Monitoring spike, the gate for 4a.** On the RØDECaster with wired headphones, with AVPlayer running: zero dropouts over five minutes of capture at 128 frames; loopback placement within ±20 ms and unchanged (within 2 ms) between the device's default buffer and 128 frames, and between monitoring off and on; the measured mic-to-ear delay. If the device refuses 128 frames or any criterion fails, capture keeps the device's default buffer and 4a ships the panel, meters and scrolling with only the direct-monitoring hint, without software monitoring.

**Meters.** The input meter shows peak with a 1.5 s hold and a shaded target band from −18 to −6 dBFS. A "near clipping" light turns red at −1 dBFS and stays red until **K** or the next take; a take whose samples reached −0.1 dBFS is marked "clipped" in its row (the verdict rule's threshold).

**Long lines.** Up to three lines of text show at full size. A longer line scrolls smoothly so that the words under the pace fill stay on the middle row; the first and last rows hold still, and a retake starts from the top again.

### Setup check (milestone 4b)

**C**, offered when a project is first opened and when the input or output device changes. Steps:

1. *Devices:* input, channel and output, as in the panel. Bluetooth output and the built-in speakers are flagged ("film sound from speakers will leak into your takes; use wired headphones").
2. *Voice:* the narrator reads a test sentence shown on screen. Reports peak and average speech level and any clipping, and suggests a gain change in dB on the interface so peaks land near −10 dBFS; after the narrator changes the gain, the sentence is read again until the level fits, so step 3 compares against the final gain. Hints, not verdicts: plosives (bursts of energy below 80 Hz, "turn the mic slightly off-axis or add a pop filter") and boom (much more energy below 200 Hz than in 200 Hz–4 kHz, "if you sound boomy, move a little further from the mic"; tuned on Niklas's voice at known distances).
3. *Room:* five seconds of silence. Reports the room's noise floor and the speech-to-noise ratio against step 2's speech level: 40 dB or more is good, 30–40 dB acceptable, below 30 dB noisy, with tips. Mains hum is flagged when narrow peaks at 50 or 60 Hz *and* at least two harmonics stand clearly above neighbouring frequencies for the whole recording. The recording is kept as room tone in `takes/.room/<timestamp>.wav` with the devices and gain level it was recorded at. Noise reduction uses the newest room tone recorded on the same devices on the same day; when there is none, `clean` skips noise reduction and the window suggests running the room step again.
4. *Latency (optional):* the loopback calibration, offered when `[calibration]` does not match the current device pair.

Results are saved in `[setup]`.

**Noise per take.** `checked` gains `speech_level_dbfs` (RMS over the energy speech range). A new verdict rule between rules 7 and 8 (after the sound rule, before `keep`): a take whose speech-to-noise ratio (speech level against its own pre-roll floor) is below 30 dB gives `listen` ("background noise only 24 dB below your voice"). An absolute floor is not used, because it depends on gain.

**Cleaner audio for the checks.** Before transcription, the take is converted with a high-pass at 80 Hz and normalized, which helps Whisper. The take file is never changed. The `checker` string's preprocessing tag changes from `pre=none` to `pre=hp80+norm`, so takes checked before are transcribed again.

### Export cleanup and formats (milestone 4b)

Takes are always recorded as 48 kHz, 24-bit WAV, the lossless original, and are never modified. Export follows `[export]`, overridable with `takewise export --format … --bit-depth … --sample-rate … --processing …`. The file is `export/narration.wav` or `export/narration.flac`.

`clean` works in this order, all at 48 kHz until the last step:

1. Each used take, whole, through a high-pass at 80 Hz and gentle noise reduction (`afftdn`, fixed reduction of 10 dB, noise floor from the room tone chosen as described in the setup check's room step). Noise reduction is skipped when there is no room tone or the room tone is digital silence (a gated interface). A click fixture proves the filters add no delay, or the delay is measured and compensated, so `clean` places speech exactly where `raw` does.
2. Placement, fades and cropping in Rust, as for `raw`.
3. Loudness: measure integrated loudness and true peak of the assembled narration (`ebur128`), then apply one static gain: the smaller of the gain to reach −16 LUFS and the gain that keeps true peak at −1 dBTP. No dynamic normalization.
4. Resampling to 44.1 kHz (once, for the whole narration) and dither when writing 16-bit.
5. Verification: integrated loudness and true peak are measured again on the final encoded file and recorded in the manifest. If resampling or dither pushed the true peak above −1 dBTP, the gain is lowered by the excess and steps 3–5 run once more; the manifest records the final values.

`raw` skips steps 1 and 3 and needs no `ffmpeg` unless resampling or FLAC is requested; `doctor` checks `ffmpeg`. The manifest states `sample_rate` of the export file and `source_sample_rate` (48000) of the takes: source ranges stay in take samples at 48 kHz, destination ranges are in export samples (`round(n × rate / 48000)`), and film seconds are authoritative.

## Distribution

Added 2026-11-23 after council round 5.

- **Install:** a Homebrew cask in Niklas's tap (`niklas-heer/tap/takewise`) installs the Developer ID signed and notarized `Takewise.app` and links its `takewise` binary onto PATH (`binary` stanza). The cask depends on the `whisper-cpp` and `ffmpeg` formulae. Without Homebrew, the app's menu offers "Install Command Line Tool", which links `~/.local/bin/takewise` to the binary inside the bundle (as Focal does).
- **Without the tools:** recording, choosing and `raw` WAV export at 48 kHz work without `whisper-cli` and `ffmpeg`; checks then give `listen` with `words-not-checked`, and `doctor` and the window say how to install them.
- **The speech model:** not bundled (1.5 GB). On first launch, and whenever it is missing, the window shows "Install the speech model (1.5 GB)"; `takewise models install` does the same from the command line. Both download `ggml-large-v3-turbo.bin` from the whisper.cpp model repository on Hugging Face into `~/.local/share/takewise/models`, show progress, verify its SHA-256 against a value pinned in the source, and resume an interrupted download. Until it is installed, checks run without words.
- **Microphone permission** belongs to `Takewise.app`; the first take triggers the macOS prompt. `doctor` and the window explain how to grant it in System Settings when it was denied.
- **Docs:** the README covers install, a first session, the keys, the Resolve recipe and agent use; `AGENTS.md` in the repo covers contributors. A short agent guide (`docs/agents.md`) shows an agent's full loop: `init`, writing `script.toml`, `status --json`, `check`, `judge`, `choose`, `export`, and placing the result in Resolve.

## Milestones

0. **Media spike (throwaway):** a GPUI window plays the film with a text overlay; capture records from a chosen device and channel with host timestamps; a take plays back locked to the film; the loopback test places beeps within ±20 ms at film zero and after seeks; microphone permission for a bare binary versus a bundle is settled. Decides surface versus layer, the capture API and how latency is accounted for.
1. **Core and CLI:** format, validation, lock and write protocol, ledger and folding, the energy speech detector, `init` (film info through `ffprobe`), `status`, `script set`, `choose`, `judge`, `export`, and `doctor` limited to tools, model and film (devices arrive with milestone 2); tests with fixtures.
2. **Recording window:** the screen above on the spike's media path: record, play in context, choose, mark start, edit text, level meter. After this milestone Niklas can narrate a film; agents can judge takes from the folder.
3. **Checks:** whisper-cli integration, alignment, verdict rules, ranking, `check`; thresholds tuned on real takes; verdicts in the window.
4. **Setup and sound** (section "Setup and sound"): 4a the audio panel, monitoring, meters and scrolling; 4b the setup check, the noise rule, export cleanup and formats. 4a starts with a short spike proving monitoring on the RØDECaster with wired headphones.
5. **Ship** (section "Distribution").

## Constraints

- Stable Rust 1.99.0 pinned in `rust-toolchain.toml` (gpui-pre 0.3.12, which gpui-kit 0.10.0 pins, needs it), edition 2024, Focal's lint baseline, `unsafe` denied outside `takewise-media`.
- mise for tasks (`build`, `test`, `check`, `run`, `clean`), vrdx decision records in `decisions/`.
- MIT license.

## Left to the builder

The exact word-alignment algorithm, number normalization, energy-detector parameters, the whisper model directory, and the GPUI layout details.
