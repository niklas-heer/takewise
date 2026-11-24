# Milestone 6: passages and a remembered setup

Status: revised after council round 1 (Claude Fable 5.2: 6.5/10, GPT Sol 6.2: 6/10), 2026-11-24.

## Why

Niklas is narrating a 13-minute vacation film whose narration has long passages in which sentences run into each other. The promo films so far had one sentence per line, recorded one line at a time. Two things got in the way:

1. **Flow.** Each line is a separate take with its own count-in, so the sentences of a passage are read in isolation and lose the intonation that joins them. The export also replaces what lies between two lines with silence: each take is trimmed to its speech plus 150 ms.
2. **Repeated setup.** Each new project asked for the setup check and the loopback latency test again, on the same devices and on the same day. `settings.toml` already remembers the input, channel and output for new projects. The latency calibration and the setup check's results live only in each project.

### Does the current system hold up for 13 minutes?

Read against the code of 0.1.3:

- **Long lines.** A line can be a paragraph: the prompter scrolls a line longer than three rows, the checks compare the whole text, and `whisper-cli` has 120 s per take. The cost: one wrong word means re-reading the whole paragraph, and the sentences inside it are not tied to their shots.
- **Many lines.** 13 minutes is 80 to 120 lines. The ledger, the status and the export scale linearly.
- **The line list** scrolls on its own but does not follow the selection (`window/lines.rs`).
- **The export** of 13 minutes holds the whole narration in memory (48 kHz `f64`) and makes up to five loudness passes. Measured on a 13-minute synthetic film with a release build of the core export: `raw` takes 1.25 s with a peak of 1.07 GB of memory, `clean` about 18 s with 1.77 GB. A debug build of the CLI measured 30 to 128 s and 1.65 to 1.78 GB for a `clean` export. That is acceptable on this Mac.

So the system works for long films, except for flow and the line list. This milestone lets sentence lines be recorded as one continuous passage, and still be picked up line by line.

## A. Passages

### What a passage is

A passage is a run of consecutive lines in which each line's slot ends less than `[record] passage_gap_s` before the next line starts. The default is 1.0 s. In other words, `next.start − slot_end(k) < passage_gap_s`, where `slot_end` is the line's `end`, or else the next line's start.

The existing scripts set `end` on every line and leave gaps of 0.1 to 0.7 s between sentences. Their deliberate pauses for picture or music are 1.4 s or more. So the rule groups sentences and splits at real pauses without any change to the scripts.

The line list shows the grouping with a thin bar beside the lines of each passage. `status --json` gives each line its `passage` (the index of its first line's id).

### Recording one: Shift+Space

**Shift+Space** records from the current line through the end of its passage. **Space** and **R** still record the current line alone. Space or Shift+Space stops any take.

How the take runs:

- **One capture and one film playback**, with the same clock, latency offset and timebase history as a line take. Pre-roll and count-in come only before the first line.
- **It stops on its own** `tail_s` after the last line's slot ends, as a line take stops, or on a capture failure.
- **Lines are snapshotted when the take starts.** **M** (mark start) is disabled during a passage take. This is new: today M works during a line take, and it still does there.
- **The prompter follows the film.** It tracks the active line by the snapshotted line id: line *k+1* becomes current when the film reaches its start, with its text, pace fill, note and next line. Its scroll resets at each switch. The line list marks the line being read.

### On disk, and never losing it

The capture streams into `takes/.passages/.<uuid>.wav.partial`, with the same writer, lock, flush and in-memory copy as a line take. Its sidecar `.<uuid>.json.partial` is written once the film is scheduled and the first sample is placed. The sidecar has its own schema (a `kind: "passage"` field) so it can never be read as a take's metadata. It holds:

- the passage's line ids and their snapshots;
- the device, channel and latency;
- the film time of the first sample;
- dropouts and film shift.

On stop, the order is:

1. Finish the WAV.
2. Compute the cuts (below) and write them into the sidecar.
3. Rename both to `takes/.passages/<uuid>.wav` and `<uuid>.json`. **This is the commit point.** From here on the audio is a published file.
4. `takewise_core::passage::split(dir, uuid)` derives one take per line. It skips any piece whose `(passage id, index)` already has a `recorded` event, so it can be called again safely. A piece left behind by a crash between renaming its `NNN.wav` and appending its event is adopted by the usual repair, then recognized by its `passage` metadata.

The window calls `split` right after the rename, on its worker thread, not in the frame. `status --repair` calls it for every published passage that still lacks pieces.

**Repair.** A passage partial that is not locked (a crash mid-take) is finalized from its size on disk, as take partials are. When its sidecar has the first sample's film time, its cuts are computed and it is published and split. Without the film time, it is reported, with its path in `status --json`, and kept. `scan` skips dot-directories under `takes/` (`.passages`, `.room`). That also fixes 0.1.3, where a stray `.json.partial` in such a directory could make repair fail.

### Cutting the passage into pieces

Each line whose start the capture reached gets one piece. The pieces are contiguous and cover the passage exactly. Boundary *b* lies between line *k* and line *k+1*. Times are converted to sample positions through the passage's first-sample film time, the same mapping that places the takes.

1. **The search window.** It runs from the midpoint of line *k*'s slot to the midpoint of line *k+1*'s slot, clamped to the captured range. So two boundaries never search the same audio.
2. **Pauses** are runs of 10 ms frames below the energy detector's speech threshold, which comes from the noise floor of the passage's pre-roll.
3. **Choosing the cut:**
   - The pause that contains the cue gap (from `slot_end(k)` to `start(k+1)`), or else the cue *B* = `start(k+1)`, if it lasts at least 150 ms.
   - Else the longest pause of at least 150 ms within ±1 s of *B*.
   - Else the nearest pause of at least 80 ms within the window.
   - Else *B* itself, marked `pause: false`.
   - Cut at the middle of the chosen pause, clamped inside the window. Cuts are then strictly increasing by construction.
4. **A trailing piece without speech** (the take was stopped right after a line began) joins the piece before it. That line gets no take. This keeps a line that was never spoken apart from one that was cut off. A line cut off mid-speech gets a piece, and its words check flags it.

Each cut is stored in the sidecar as `{ sample, cue_film_s, pause: bool }`.

### Pieces are ordinary takes

Each piece is written as an ordinary `takes/<line>/NNN.wav`, with `NNN.json` and a `recorded` event. Its `line_offset_s` comes from the piece's first sample. Both `RecordedInput` and `Recorded` gain an optional `passage` object:

- `id`
- `index` and `count`
- `from_sample` and `to_sample` in the passage file
- `preroll_floor_dbfs`

The folded `TakeState` and `status --json` show it. An older Takewise reads these takes as ordinary takes and ignores `passage`.

### Checks on a piece

- **The noise floor** comes from `passage.preroll_floor_dbfs`, through a floor override in `analyze`, not from the piece's own short pre-roll. The same floor is used for the speech threshold, the noise rule and room-tone matching, in `run_check`, `export` and `clean`.
- **The timing rules move to the passage's edges.**
  - Overrun is checked only on the last piece. Early start is checked only on the first piece.
  - At an interior boundary, the piece after it gets `listen` with `passage-drift` ("started 1.4 s before its cue") when its speech onset is more than 1.0 s from its line's start, in either direction.
  - A cut with `pause: false` gives both neighbouring pieces `listen` with `passage-boundary` ("no pause found at the boundary; the words may be split between lines").
- Words and sound are checked per piece as before. Ranking is unchanged, and nothing is chosen automatically.

### Hearing and choosing a passage

- **Shift+P** plays the selected take's passage in context: the passage file, locked to the film from its own first sample. This is how the narrator hears the flow. **P** still plays one piece in context.
  - When a take's first sample comes after the film's start point (any piece but the first), the take is scheduled at its own host time instead of at a negative offset.
- **Shift+Enter** chooses that passage's piece for every line of the passage that has one.
  - All `chosen` events are appended in one write, under the project lock, after checking that every piece's take exists.
  - `takewise choose <dir> --passage <id>` does the same from the CLI.
  - The window says how many lines it chose. Lines without a piece keep their choice.

### Export: a passage stays continuous

Before trimming, the export groups consecutive lines into a **seam run** when all of these hold:

- their used takes are pieces *i*, *i+1*, … of one passage;
- each piece's `to_sample` equals the next piece's `from_sample`;
- each piece's own placement (`line.start + line_offset_s`) equals the run's anchor (the first piece's destination plus the source offset) within one sample. When a line moved after recording, this fails, and that boundary is spliced as it is today.

A seam run is rendered as one source range of the passage file, placed at the run's anchor:

- It is trimmed and faded only at its outer edges, with today's rules.
- In `clean`, it is cleaned as one file, so the filters do not restart at a cut.
- Overlap and margin rules apply between runs and other takes as today.

So a passage chosen whole exports exactly as it was recorded. The manifest keeps per-line ranges and marks `seam_with: "<next line id>"`.

## B. A remembered setup

Everything here lives in `~/Library/Application Support/Takewise/remembered.json`, beside `settings.toml`. It is changed only by one locked read-modify-write, because `save_defaults_to` rewrites `settings.toml` whole.

**Latency.**

- **What is remembered:** every stored measurement (`takewise calibrate --write`, or the setup check's latency step) is saved per exact route: input name, output name, and capture buffer frames, with the offset and the date. A newer measurement replaces an older one for the same route.
- **Existing measurements:** opening a project whose `[calibration]` fits its route also remembers it, if nothing is remembered for that route yet. So the first launch of 0.2.0 already knows the WH-1000XM5 measurement.
- **When it is applied:** when the window opens and whenever the route changes. With the devices and buffer resolved live, if the project has no calibration that fits, and one is remembered for exactly that route, it is copied into `[input] latency_offset_ms` and `[calibration]`, with `from = "remembered"`.
  - The match stays exact, so an offset is never applied to other devices or another buffer size.
  - A project's own fitting measurement always wins.
  - `init` copies nothing, since it knows neither the resolved output nor the buffer size.
- **What it says:** the panel and `status` show "latency from an earlier measurement on these devices (date)".
- **Bluetooth:** remembered offsets apply to Bluetooth outputs too. Niklas's three projects measured −29, −29 and −28 ms on the WH-1000XM5. For a Bluetooth output the panel adds: "Bluetooth delay can change after reconnecting; press C to measure again (15 s) if takes sound late." `doctor` keeps warning that wired headphones are better.

**Setup check.**

- **What is remembered:** after each setup check, the latest result for its exact input, channel and output. That is the `[setup]` table and a copy of its room tone (WAV and sidecar, in `rooms/` beside `remembered.json`). Room tones from earlier days are pruned.
- **When it is used:** a project with no `[setup]` for the route in use, or none from today (local day), adopts a remembered same-day result. It copies the room tone into its own `takes/.room/` and writes `[setup]` with `from = "remembered"`.
  - `clean` and the setup offer then work unchanged, from the project's own files.
  - A take whose pre-roll floor differs from the room tone by more than 6 dB still skips noise reduction, and the window still suggests running the room step again.

## C. The line list follows the current line

When the current line changes (arrows, a passage take moving to its next line, a reload), the list scrolls just enough to show it.

## Out of scope

- Editing or re-splitting cuts, and forced alignment with whisper to find them.
- Inline cues inside a line, and punch-in within a line.
- Monitoring, exporting from the window, and jumping to a clicked row.

## Tests

**Core, with synthetic audio and fixture projects:**

- Cuts:
  - a pause around the cue;
  - a stop closure (60 ms) beside a real pause;
  - no pause;
  - two short lines;
  - early and late narrators;
  - a stop right after a boundary;
  - the pieces cover the passage exactly.
- `split` is idempotent, including after a crash between renaming a piece and appending its event.
- Repair finishes a crashed passage partial, and skips dot-directories.
- Piece checks:
  - the floor override;
  - interior timing (`passage-drift` and `passage-boundary`; no overrun or early start inside).
- Export:
  - a whole passage exports sample-identical to its file, in `raw`, and continuous in `clean`;
  - a moved line breaks only its own seam;
  - mixed passage and line takes;
  - the manifest's `seam_with`.
- `choose --passage`: all or nothing, and lines without pieces left alone.
- Remembered:
  - calibration saved, applied on the exact route only, not on another buffer, and a project's own measurement wins;
  - an existing project calibration becomes remembered;
  - setup adopted on the same day only, with the room tone copied in;
  - `settings.toml` is untouched by all of it.

**Window, with the simulated media in `window/tests.rs`:**

- Shift+Space across three lines gives one passage file and three takes.
- The prompter switches lines at each start and resets its scroll.
- Space stops mid-passage.
- A passage ends at a gap of 1 s or more.
- Shift+P plays the passage.
- Shift+Enter chooses across the passage.
- The line list follows the selection.

**Real use.** Record a 60 to 90 s passage on the RØDECaster, including one spoken-through cue. Listen to it with Shift+P, replace one line with a single-line take, and export in `clean`. The export time and peak memory on a 13-minute film are measured (see "Does the current system hold up for 13 minutes?").

## Release

0.2.0. The new fields (`passage`, `seam_with`, `[record] passage_gap_s`) are additive, and the format stays `1`. README, `docs/reference.md`, `docs/agents.md` (with the passage rule and `choose --passage`) and `docs/design.md` are updated. Released as `RELEASE.md` describes: signed, notarized, a GitHub release, and a cask PR in the tap.
