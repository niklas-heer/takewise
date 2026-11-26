# Takewise for agents

Takewise splits narration work between a person and an agent. The narrator reads the lines in the recording window; you, the agent, write the script, read the checks of every take, judge and choose takes when the narrator hands that over, export the narration and, with your own editor tools, place it in the film's edit. Everything goes through the `takewise` command and plain files, so you never need the window.

This guide walks one project through the whole loop. A test (`crates/takewise/tests/agents_doc.rs`) runs its commands in order against a scratch project, recording fixture takes in place of the narrator's `takewise record` (a passage of both lines, then one more take of each line; the passage id shown below stands for that passage's), and checks that every JSON example below has the shape of the real output (every key it shows is there), so they stay true.

## Rules

- Add `--json` to any command: it prints one JSON object, `{"ok":true,"data":…}` or `{"ok":false,"error":{"code":…,"message":…},"data":…}`.
- Exit codes: 0 success, 1 a finding (`data` says what: an export that refuses, `status --strict` with open lines, a failed `doctor` check), 2 a usage or setup error (`error.code` says which, such as `missing-model` or `script-changed`).
- Never edit `takes.jsonl` or anything under `takes/`. Takes are only ever added; choices and judgments are new events written by `choose` and `judge`. If you read `takes.jsonl` yourself, open it anew each time instead of following an open file: `choose --passage` replaces the file with a longer copy (every earlier byte kept), which a file held open does not show.
- Write `script.toml` freely before recording. Once takes exist, change one field at a time with `takewise script set … --expect <script_version>`, so an edit made against a script the narrator changed meanwhile is refused instead of lost.
- Flag, don't decide: checks and judgments rank takes; only `choose` (yours or the narrator's) picks one.
- Work through the `takewise` command, never the app's windows: `takewise record <dir>` opens the recording window for the narrator.

## 1. Check the setup

```sh
takewise models install --json
takewise doctor --json
```

`models install` downloads the speech model (1.5 GB) once, resuming an interrupted download, and verifies it; it reports `already-installed` when it is there. `doctor` checks `whisper-cli`, the model, `ffmpeg`, the microphone permission and the audio devices; each failed check in `data.checks` says how to fix it.

## 2. Create the project and write the script

```sh
takewise init promo.takewise --film renders/promo.mp4 --json
```

`init` measures the film with `ffprobe` and writes `project.toml` and an empty `script.toml`. To time the lines you need your own view of the cut: Takewise doesn't show you the film, so pull frames with `ffmpeg` (for example one every second, or at scene changes) or read the editor's timeline with your own tools. Rough starts are fine; the narrator can move a line's start with M while the film plays (during a take or P). Write the lines: an `id` (lowercase letters, digits and `-`, unique), the `start` in seconds of film time, an optional `end` (otherwise the slot runs to the next line's start or the film's end), the `text` and an optional `note` for the narrator.

<!-- file: promo.takewise/script.toml -->
```toml
[[line]]
id = "hook"
start = 2.0
text = "Hello there."
note = "warm, unhurried"

[[line]]
id = "close"
start = 6.0
end = 9.5
text = "See you at the reunion."
```

```sh
takewise status promo.takewise --json
```

`status` validates the script (starts in order, slots inside the film) and lists `problems`; fix them before the narrator records. Keep `script_version` for later edits.

Each line's `passage` names the passage it belongs to, by the id of the passage's first line. A passage is a run of consecutive lines that the narrator can read in one go: a line and the next belong together while the next one starts less than `[record] passage_gap_s` (1.0 s in `project.toml`) after the slot of the one before ends. Here `hook` has no `end`, so its slot runs to `close`, and both lines are one passage. To split a passage, leave a pause of a second or more between two lines (set the earlier line's `end`); to join sentences, keep their slots closer than that.

<!-- output of: takewise status promo.takewise --json -->
```json
{
  "ok": true,
  "data": {
    "script_version": "4f1c…",
    "film": { "path": "/…/renders/promo.mp4", "missing": false, "changed": false },
    "lines": [
      { "id": "hook", "text": "Hello there.", "start": 2.0, "end": 6.0, "passage": "hook", "takes": [], "chosen": null, "proposed": null }
    ],
    "problems": [],
    "notices": []
  }
}
```

`problems` block an export; `notices` inform. One notice to act on with the narrator is `rescued-audio`: the window could not save a take or passage into the project (its disk was full or gone) and kept the audio in the folder the message names, with the steps to bring it back ([reference](reference.md#rescued-audio)). One folder can hold several recordings: restore all of them before the folder is deleted, and never delete a WAV the message lists as having no JSON file without the narrator.

## 3. The narrator records

```sh
takewise record promo.takewise
```

This opens the recording window in `Takewise.app`; the narrator records one or more takes per line and may choose some. A passage can be read in one go (Shift+Space in the window): it is kept whole in `takes/.passages/<id>.wav` and cut, in the pauses between the lines, into one take per line, so everything below works on its takes as on any other. Wait for them. The window checks each take as it is recorded; you can follow along with `status`, which reloads from the files every time.

## 4. Read the checks

```sh
takewise check promo.takewise --json
```

`check` checks every take without a current check (the window usually has already) and prints what it found. A take gets a verdict, `keep`, `listen` or `retake`, with reasons: no speech, clipping, missing, extra or misheard words (named, with times), an overrun of the line's slot, an early start, background noise. The words come from `whisper-cli` without the script as a prompt, so it does not hear what it expects. A `retake` verdict is a finding about the take, not an error: the command still exits 0. Times here (`transcript`, `word_diffs`, `speech`) are seconds from the take's first sample; `overrun_s` is how far the speech ends past the line's slot (negative: before it).

<!-- output of: takewise check promo.takewise --json -->
```json
{
  "ok": true,
  "data": {
    "checks": [
      {
        "line": "hook",
        "take": 1,
        "verdict": "keep",
        "reasons": [],
        "transcript": [{ "text": "Hello", "start": 2.1, "end": 2.4 }, { "text": "there.", "start": 2.5, "end": 2.8 }],
        "word_diffs": [],
        "speech": [2.0, 3.0],
        "overrun_s": -3.0,
        "peak_dbfs": -6.4,
        "checker": "takewise 0.2.0 whisper ggml-large-v3-turbo.bin pre=hp60+norm"
      },
      {
        "line": "close",
        "take": 2,
        "verdict": "retake",
        "reasons": [
          { "code": "missing-words", "message": "missing “see”, “you”, “at”" },
          { "code": "possible-mishearing", "message": "heard “hello” for “the”, “there” for “reunion”" }
        ],
        "word_diffs": [{ "kind": "missing", "expected": "see", "heard": null, "at": 2.1 }]
      }
    ]
  }
}
```

```sh
takewise status promo.takewise --json
```

In `status`, each take has its `effective` verdict (the check's, or the latest judgment's), the `reason` behind it, its `speech` (start and end in seconds of the take), and whether it was recorded against text the line no longer has. A take cut from a passage has a `passage` object: the passage's `id`, which piece it is (`index` of `count`), its sample range in the passage file (`from_sample`, `to_sample`) and the noise floor it is checked with; a take recorded line by line has none. Each line has the `proposed` take (the best ranked: `keep` before `listen` and unchecked before `retake`, then fewer word differences, then less overrun, then your `score`, then newest) and the `chosen` one.

<!-- output of: takewise status promo.takewise --json -->
```json
{
  "ok": true,
  "data": {
    "lines": [
      {
        "id": "hook",
        "takes": [
          {
            "take": 1,
            "file": "takes/hook/001.wav",
            "effective": "keep",
            "verdict_source": "check",
            "reason": null,
            "speech": [2.0, 3.0],
            "stale_check": false,
            "recorded_against_old_text": false,
            "passage": {
              "id": "0d9c5a0e-6a0c-4e52-9d33-3c1f5a2b7e10",
              "index": 0,
              "count": 2,
              "from_sample": 0,
              "to_sample": 240000,
              "preroll_floor_dbfs": -144.0
            }
          }
        ],
        "chosen": null,
        "proposed": 2
      }
    ]
  }
}
```

## 5. Judge and choose

Listen to the takes yourself if you can (the WAVs are in `takes/<line>/`), and add a judgment when you disagree with a check or want to rank takes. A judgment overrides the check's verdict for ranking; the `score` (0 to 1) breaks ties between takes with the same verdict.

```sh
takewise judge promo.takewise close 2 --verdict listen --reason "words differ: check the ending" --by claude-opus-5-5 --score 0.4 --json
takewise choose promo.takewise --passage 0d9c5a0e-6a0c-4e52-9d33-3c1f5a2b7e10 --by claude-opus-5-5 --json
```

`choose --passage` takes a passage's `id` (from a take's `passage.id` in `status`) and chooses that passage's take for every line it has one of, so the lines stay in the flow they were read in. It is all or nothing: when one of the takes' files is missing it chooses none (exit 1, `missing-file`). Lines the passage has no take of (the narrator stopped before them) are listed in `skipped` and keep their choice.

<!-- output of: takewise choose promo.takewise --passage 0d9c5a0e-6a0c-4e52-9d33-3c1f5a2b7e10 --by claude-opus-5-5 --json -->
```json
{
  "ok": true,
  "data": {
    "passage": "0d9c5a0e-6a0c-4e52-9d33-3c1f5a2b7e10",
    "by": "claude-opus-5-5",
    "chosen": [{ "line": "hook", "take": 1 }, { "line": "close", "take": 1 }],
    "skipped": []
  }
}
```

A single line can still get another take: here `close` was read again on its own, and that take replaces the passage's.

```sh
takewise choose promo.takewise close 2 --by claude-opus-5-5 --json
takewise status promo.takewise --strict --json
```

`choose` refuses a take whose file is missing. `status --strict` exits 0 once every line has a chosen take and no problem is left; otherwise 1.

The check of a take cut from a passage reads its timing by its place in the passage. Only the last piece can overrun and only the first can start early; in between, `passage-drift` (`listen`) says a line's words began more than a second from its cue, and `passage-boundary` (`listen`) says no pause was found where two lines were cut apart, so a word may sit in the wrong take: listen to both neighbours, or prefer other takes for those lines.

## 6. Export

```sh
takewise export promo.takewise --json
```

`export` writes `export/narration.wav` (or `.flac`): one mono file exactly as long as the film, starting at its first frame, with each line's chosen take placed at the line's current start plus the offset the take was recorded at, trimmed to its detected speech. The project's `[export]` settings choose the format and processing; new projects export `clean` (high-pass, noise reduction from the setup check's room tone, −16 LUFS, a common level for spoken word, with true peaks below −1 dBTP). It refuses (exit 1) with the line ids when a line has no chosen take, a take has no speech, or two lines' speech overlaps; `--use-proposed` fills unchosen lines with their proposed take, and still refuses a line whose proposed take is marked `retake` (`no-acceptable-take`). It also refuses when the film is missing (`film-missing`) or changed since the project measured it (`film-changed`). When the film was rendered again, point the project at the new render instead of starting a new project: `takewise film promo.takewise --film <new render> --json`, or without `--film` for a render written over the old file (as `status --refresh-film` does). It keeps the script, the takes, the choices and the setup, records the film it replaced (`status` shows it as `film.previous`), and warns, without refusing, in `data.warnings`: `film-duration-changed` (the message gives the old and new duration) and `slot-past-film-end` for each `line` whose slot now runs past the film's end. Check the line timings against the new cut, fix them with `script set`, then export again. A recording window open on the project keeps the film it opened and asks to be opened again. `film-probe-timeout` means `ffprobe` stalled reading the film and was stopped; export again. `state-changed` means the script or ledger changed twice while it rendered; export again. The command reports the files, the settings and loudness it used, the lines and any warnings:

<!-- output of: takewise export promo.takewise --json -->
```json
{
  "ok": true,
  "data": {
    "narration": "promo.takewise/export/narration.wav",
    "manifest": "promo.takewise/export/manifest.json",
    "settings": { "processing": "clean", "format": "wav", "sample_rate": 48000, "bit_depth": 24, "loudness_lufs": -16.0 },
    "lines": [{ "id": "hook", "take_file": "takes/hook/001.wav", "speech_film_s": [2.0, 3.0] }],
    "warnings": []
  }
}
```

Warnings mean the file was written but is worth a look; the same list is the manifest's top-level `warnings`. Each has a `code`, a `message` and, unless it is about the whole narration, the `line`:

- `recorded-against-old-text`: the take was recorded before the line's text changed.
- `speech-cropped`: part of the take's speech lies outside the film.
- `tight-gap`: two lines' speech is less than 20 ms apart.
- `passage-piece-without-speech`: the line's take is a piece of a passage that holds no speech, exported as one stretch with its neighbours (`seam_with`): the narrator read the line early or late, so its words are in the piece before or after it. The narration has them as they were read; the line's `speech_film_s` is an empty range. Have the narrator listen to the passage, and re-record the line if its words are missing. When the piece lies at an outer edge of the stretch and the passage's piece on that side is not exported (its line uses another take, such as a line read again on its own), the message instead says the line may be missing from the export and names that piece's take file: a line read early is in the piece before, a line read late in the piece after, and in neither case in the narration. Listen to the named file, then re-record the line or choose that take for its line. A piece without speech at the edge of its stretch is trimmed away whole, so its `source_samples` and `dest_samples` are empty ranges (inside its file and the narration, as every range is).
- `noise-reduction-skipped` (`clean`): no room tone matches the take (none on its input, channel and day, or the gain changed), or the take or the room tone is digital silence. Run the setup check's room step, then export again.
- `loudness-target-missed` (`clean`, no `line`): the limiter's 6 dB cap kept the narration below −16 LUFS (`loudness_target_reached: false`).

`export/manifest.json` describes the narration file for whatever places it:

```sh
cat promo.takewise/export/manifest.json
```

<!-- output of: cat promo.takewise/export/manifest.json -->
```json
{
  "film": { "path": "/…/renders/promo.mp4", "duration_s": 51.5 },
  "narration": { "file": "narration.wav", "size": 7416068, "sha256": "4aa9…" },
  "processing": "clean",
  "sample_rate": 48000,
  "source_sample_rate": 48000,
  "bit_depth": 24,
  "loudness_lufs": -16.0,
  "true_peak_dbtp": -10.2,
  "lines": [
    {
      "id": "hook",
      "text": "Hello there.",
      "take_file": "takes/hook/001.wav",
      "chosen_by": "claude-opus-5-5",
      "source_samples": [88800, 151200],
      "dest_samples": [88800, 151200],
      "speech_film_s": [2.0, 3.0],
      "words_film_s": [{ "text": "Hello", "start": 2.1, "end": 2.4 }]
    }
  ]
}
```

## 7. Place it in the edit

Takewise has no editor-specific code; place the export with your own editor tools. In any editor, the narration file goes on a mono track at the start of the film's timeline, so its sample 0 lines up with the film's first frame. In DaVinci Resolve, for example:

1. Import the file named by the manifest's `narration.file` (in `export/`) into the media pool.
2. Add a mono audio track to the film's timeline and place the narration at the timeline's start, so its sample 0 lines up with the film's first frame.
3. For each entry in `lines`, add a timeline marker at `speech_film_s[0]` seconds, named after the line's `id`, with its `text` as the note.

To place takes one by one instead, put each line's `take_file` (the raw original at `source_sample_rate`) at `dest_samples[0] / sample_rate` seconds and trim it to `source_samples`. The positions are exact sample offsets. Skip a line whose `source_samples` is an empty range: nothing of its take is in the narration (see `passage-piece-without-speech`).

When the narrator re-records a line later, run the loop from step 4 again: `check`, read `status`, `choose`, `export`.
