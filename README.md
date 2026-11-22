# Takewise

A macOS app for recording voiceover narration over a finished film.

Takewise shows the script as a teleprompter timed to the picture, records several takes per line, and judges each take against the script: wrong words, overrun timing and sound problems. The chosen takes, with their timings, are written to a plain folder that agents and DaVinci Resolve can pick up.

Status: milestone 1 of [the design](docs/design.md). The project folder, the ledger, speech detection, export and the `takewise` command line work. The recording window arrives in milestone 2 and the word checks in milestone 3.

## Requirements

- Apple Silicon macOS and Rust (pinned in `rust-toolchain.toml`), with [mise](https://mise.jdx.dev) for the tasks.
- `ffprobe` from `brew install ffmpeg`, to measure the film.
- From milestone 3: `whisper-cli` from `brew install whisper-cpp` and a whisper model in `~/.local/share/takewise/models` (or `$TAKEWISE_MODELS`).

Build with `mise run build`; run with `mise run run -- <command>`.

## Command line

Every command takes the project directory. With `--json` it prints one JSON object on stdout: `{"ok":true,"data":…}`, or `{"ok":false,"error":{"code":…,"message":…}}` with the data that led to it. Exit codes: 0 success, 1 a finding (an export that refuses, `status --strict` with problems, a failed `doctor` check), 2 a usage or setup error.

```sh
takewise init promo.takewise --film renders/promo.mp4   # measure the film, create an empty script
$EDITOR promo.takewise/script.toml                      # agents write the [[line]] tables
takewise status promo.takewise                          # lines, takes, verdicts, proposed and chosen takes, problems
takewise script set promo.takewise hook --start 1.5     # change one field of one line, keeping comments
takewise judge promo.takewise hook 2 --verdict keep --reason "clear, warm" --by claude-opus-5-5 --score 0.8
takewise choose promo.takewise hook 2 --by claude-opus-5-5
takewise export promo.takewise                          # export/narration.wav and export/manifest.json
takewise doctor promo.takewise                          # whisper-cli, the model, ffprobe and the film
```

- `status --strict` exits 1 while a line has no chosen take or any problem exists; `--repair` adopts takes a crash left without a ledger event; `--refresh-film` re-measures a film that changed.
- `script set` takes exactly one of `--text`, `--start`, `--end`, `--note`, `--clear-end` and `--clear-note`. Pass `--expect <script_version>` from `status --json` to refuse the edit if the script changed since you read it.
- `export` uses each line's chosen take and refuses, naming the lines, when one has none, when a take has no detectable speech, or when two lines overlap. `--use-proposed` fills unchosen lines with the proposed take.
- `record` and `check` exit 2 until milestones 2 and 3.

The project folder is plain TOML, JSONL and WAV; see [Project folder](docs/design.md#project-folder) in the design.

## Placing the narration in DaVinci Resolve

`export/narration.wav` is 48 kHz mono 24-bit PCM, starts at the film's first frame and is exactly as long as the film. Takewise has no Resolve code; an agent with Resolve tools places it:

1. Import `export/narration.wav` into the media pool.
2. Add a new mono audio track to the film's timeline and place the WAV at the timeline's start (the film's first frame), so sample 0 lines up with frame 0.
3. For each entry in `export/manifest.json` under `lines`, add a timeline marker at `speech_film_s[0]` with the line's `id` as its name and `text` as its note.

To place takes individually instead, put each line's `take_file` on the track at `dest_samples[0] / 48000` seconds and trim it to `source_samples`; the positions in the manifest are exact sample offsets.

## License

MIT
