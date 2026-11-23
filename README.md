# Takewise

A macOS app for recording voiceover narration over a finished film.

Takewise shows the script as a teleprompter timed to the picture, records several takes per line, and judges each take against the script: wrong words, overrun timing and sound problems. The chosen takes, with their timings, are written to a plain folder that agents and DaVinci Resolve can pick up.

Status: milestone 5 of [the design](docs/design.md): everything up to distribution works; the first public release is still to come.

## Install

```sh
brew install --cask niklas-heer/tap/takewise   # Takewise.app, the takewise command, whisper-cpp and ffmpeg
takewise models install                        # the speech model (1.5 GB), once
takewise doctor                                # check the tools, the model, the microphone and the devices
```

Takewise needs an Apple Silicon Mac with macOS 14 or later. The cask installs `Takewise.app`, links `takewise` onto your `PATH`, and pulls in `whisper-cpp` (for the word checks) and `ffmpeg` (film info, audio for the checks, `clean` export, FLAC and 44.1 kHz).

The speech model (`ggml-large-v3-turbo.bin`) is not bundled. `takewise models install` downloads it from whisper.cpp's repository on Hugging Face into `~/.local/share/takewise/models` (or `$TAKEWISE_MODELS`), resumes an interrupted download, and installs it only when its SHA-256 matches the one pinned in Takewise. The recording window offers the same with a button while the model is missing. Until it is installed, takes are checked without words (`listen`, "words not checked") and checked again once it is there.

The first take asks for the microphone. If you said no, the window says so and does not record: turn Takewise on in System Settings › Privacy & Security › Microphone and reopen it; `takewise doctor` reports the same.

Recording, choosing and `raw` WAV export at 48 kHz work without `whisper-cli` and `ffmpeg`.

### From source

With Rust (pinned in `rust-toolchain.toml`) and [mise](https://mise.jdx.dev): `mise run build`, `mise run run -- <command>`, `mise run check`. Recording runs inside `Takewise.app`, so the microphone permission belongs to Takewise rather than to the terminal that started it: build the bundle with `mise run bundle` (a release build in `build/Takewise.app`, signed ad hoc, so macOS asks for the microphone again after each rebuild; set `TAKEWISE_SIGN_IDENTITY` to a Developer ID to keep the grant). `takewise record` looks for the bundle it runs in, then `$TAKEWISE_APP`, then `/Applications/Takewise.app`, then the repository's `build/Takewise.app`. Without Homebrew, **Takewise › Install Command Line Tool…** in the app links `~/.local/bin/takewise` to the binary inside the bundle; it never replaces a file that is not a link to a Takewise bundle. Releases are described in [RELEASE.md](RELEASE.md).

## A first session

1. **Start a project** from the finished film and write the script, by hand or with an agent ([the agent guide](docs/agents.md)):

   ```sh
   takewise init promo.takewise --film renders/promo.mp4
   $EDITOR promo.takewise/script.toml
   ```

   Each line is a `[[line]]` table with an `id`, the `start` in seconds of film time, an optional `end`, the `text` and an optional `note`; see [Project folder](docs/design.md#project-folder).

2. **Open the recording window** with `takewise record promo.takewise`. Press **A** to pick the microphone's input and channel and the headphones' output, then **C** for [the setup check](#the-setup-check): your level, the room's noise and the latency. Run `takewise calibrate promo.takewise --write` once per device pair when it asks.

3. **Record.** Space starts a take of the current line: the film plays from a little before the line, counts in, and the line fills at an even pace across its slot. Space again stops. Every take is kept and checked: words, timing and sound, with a verdict (`keep`, `listen`, `retake`) in the take list. Record more takes with R, listen with P, and press Enter on the take you want.

4. **Export** with `takewise export promo.takewise`: `export/narration.wav`, as long as the film, and `export/manifest.json`. Place it in your edit as described [below](#placing-the-narration-in-davinci-resolve).

### Keys

| Key | Does |
| --- | --- |
| Space | Record a take of the current line, or stop it |
| Enter | Choose the selected take |
| R | Record another take |
| P | Play the line in context, the take locked to the film |
| M | Set the line's start to the playhead while the film plays |
| E | Edit the line's text (Enter saves, Esc cancels) |
| ↑ / ↓ | Previous or next line |
| ← / → | Previous or next take |
| A | The audio panel: input, channel, output, volumes, meters |
| C | The setup check |
| − / + | Film quieter or louder |
| , / . | Monitor quieter or louder |
| H | Monitoring (not available yet) |
| K | Clear the clipping light |
| I | Install the speech model, while it is missing |

## Agents

Every state the window shows is readable with `--json`, and the folder is plain TOML, JSONL and WAV, so an agent can run everything but the recording itself: write the script, read the checks, judge and choose takes, export, and place the narration in Resolve. [docs/agents.md](docs/agents.md) walks through that loop with the exact commands and JSON shapes; a test runs every command in it.

## Command line

Every command takes the project directory. With `--json` it prints one JSON object on stdout: `{"ok":true,"data":…}`, or `{"ok":false,"error":{"code":…,"message":…}}` with the data that led to it. Exit codes: 0 success, 1 a finding (an export that refuses, `status --strict` with problems, a failed `doctor` check), 2 a usage or setup error.

```sh
takewise init promo.takewise --film renders/promo.mp4   # measure the film, create an empty script
$EDITOR promo.takewise/script.toml                      # agents write the [[line]] tables
takewise status promo.takewise                          # lines, takes, verdicts, proposed and chosen takes, problems
takewise script set promo.takewise hook --start 1.5     # change one field of one line, keeping comments
takewise judge promo.takewise hook 2 --verdict keep --reason "clear, warm" --by claude-opus-5-5 --score 0.8
takewise choose promo.takewise hook 2 --by claude-opus-5-5
takewise export promo.takewise                          # export/narration.wav (or .flac) and export/manifest.json
takewise record promo.takewise                          # open the recording window in Takewise.app
takewise calibrate promo.takewise --write               # loopback test: store latency_offset_ms
takewise doctor promo.takewise                          # tools, the model, the microphone, the film, input devices and the output
takewise models status                                  # whether the speech model is installed
takewise models install                                 # download and verify it (1.5 GB), resuming
```

- `status --strict` exits 1 while a line has no chosen take or any problem exists; `--repair` adopts takes a crash left without a ledger event; `--refresh-film` re-measures a film that changed. Notices, which do not fail `--strict`, include `calibration-stale` (the input, the output or the capture buffer differs from the ones the latency was measured on) and `no-setup` (no setup check has run yet, or the devices changed since; press C in the recording window).
- `script set` takes exactly one of `--text`, `--start`, `--end`, `--note`, `--clear-end` and `--clear-note`. Pass `--expect <script_version>` from `status --json` to refuse the edit if the script changed since you read it.
- `export` uses each line's chosen take and refuses, naming the lines, when one has none, when a take has no detectable speech, or when two lines overlap. `--use-proposed` fills unchosen lines with the proposed take. `[export]` in `project.toml` sets the file, and `--format wav|flac`, `--bit-depth 16|24`, `--sample-rate 48000|44100` and `--processing raw|clean` override it for one export. `raw` places the takes as recorded. `clean`, the default for projects made by `init`, high-passes each take at 80 Hz, reduces its noise by 10 dB when a room tone from the setup check matches it (the manifest says per line when and why it did not), and brings the narration to −16 LUFS integrated with one gain and a transparent look-ahead peak limiter that keeps the true peak below −1 dBTP; the limiter never takes more than 6 dB, so very dynamic narration can stay below the target (`loudness_target_reached: false`). The manifest records the measured loudness and true peak of the final file. Takes are never changed. 16-bit files are dithered. `ffmpeg` is needed for `clean`, 44.1 kHz and FLAC; `raw` WAV at 48 kHz works without it. A project without `[export]` exports 48 kHz 24-bit WAV, `raw`. Only one narration file sits beside the manifest: switching the format removes the other.
- `record` refuses a script with validation problems (exit 1) and exits 2 when no `Takewise.app` is found. In the window: Space records or stops a take of the current line (pre-roll, a count-in, the line filling at an even pace across its slot), Enter chooses the selected take, R records another, P plays the line in context with the take locked to the film, M sets the line's start to the playhead while the film plays, E edits the line's text, ↑/↓ move between lines and ←/→ between takes. A line longer than three rows scrolls with the pace guide, keeping the current words on the middle row. The audio keys are described [below](#sound-the-audio-panel-and-meters), and C opens [the setup check](#the-setup-check). The window records only from the project's `[input] device` at 48 kHz and says why when it cannot. It reloads when `script.toml` or `takes.jsonl` change, and refuses an edit made against a script that changed on disk. Closing it mid-take keeps the take.
- `calibrate` plays ten short beeps (made with `ffmpeg`) with seeks through the default output, or `--output <device>`, while the project's input records them in Takewise.app; hold the headphones to the microphone. It prints where the beeps landed (mean and standard deviation in ms) and with `--write` stores the offset that cancels the mean in `[input] latency_offset_ms`, with the input, output and capture buffer size it was measured on in `[calibration]`, keeping `project.toml`'s comments. Without `--output` it plays through the project's `[output] device`, else the system output. Fewer than six detected beeps exits 1. Calibrate again whenever `status` or the audio panel says the calibration is stale.
- `check` runs the checks (energy speech detection, whisper-cli words, sound, timing, background noise) on every take without a current check and appends a `checked` event each; `--line` and `--take` re-run those takes even when checked, `--stale-only` only those checked against older text or an older slot. Whisper hears a copy high-passed at 60 Hz and peak-normalized; the take is never changed. A missing whisper-cli, ffmpeg or model exits 2 with how to install it.
- `doctor` lists the input devices with their sample rates and channels, checks the project's input device (chosen, connected, at 48 kHz), and warns when the output is Bluetooth.

The project folder is plain TOML, JSONL and WAV; see [Project folder](docs/design.md#project-folder) in the design.

## Sound: the audio panel and meters

Press **A** in the recording window for the audio panel. It also opens by itself when the project's input is missing, not chosen, or not running at 48 kHz, and says so in plain words; recording stays off until that is fixed.

- **Input, channel and output.** Pick the input device and the output the film plays through. Every channel of the input has its own live meter: speak, and the channel that moves carries your microphone (on the RØDECaster Duo that depends on RØDE Central's routing); click it to record from it. An input at another rate offers **Switch to 48 kHz**, which sets the device's rate as Audio MIDI Setup would, or says why the device refused. Device changes apply between takes; during a take the pickers are disabled. The choices are saved in `project.toml` (`[input]`, `[output]`) and as the defaults for new projects in `~/Library/Application Support/Takewise/settings.toml`.
- **Volume.** Film volume and monitor volume are sliders and keys that match the character you type, so they work on QWERTY and QWERTZ alike: `-` and `+` for the film, `,` and `.` for the monitor, 1 dB per press (film −60 to 0 dB, monitor −60 to +6 dB). They work during a take too, and are saved to `[record] film_gain_db` and `[monitor] volume_db` between takes.
- **Meters.** The input meter holds its peak for 1.5 s over a shaded target band from −18 to −6 dBFS. The near-clipping light turns red at −1 dBFS and stays red until **K** or the next take. A take whose samples reached −0.1 dBFS says "clipped" in its row.
- **Monitoring (H)** is not available yet: it waits for a test on a real interface with wired headphones, which proves it adds no dropouts and does not move the takes. Until then the panel's switch stays off with that reason, and H says so. If your interface has direct (hardware) monitoring, use it to hear yourself; when software monitoring arrives, leave it off with direct monitoring on, or you will hear yourself twice. Monitoring will never change what is recorded.

## The setup check

Press **C** in the recording window. The window offers it, with a line above the keys, when no setup check has run on the project or the devices changed since; it never opens by itself and never during a take. It walks through four steps (Enter goes on, Esc closes):

1. **Devices:** the input, channel and output (change them in the audio panel, A). Speakers and Bluetooth output are flagged: use wired headphones.
2. **Voice:** press Space and read the sentence on screen. You get the peak and the average speech level, any clipping, and the gain change on your interface that puts the peaks near −10 dBFS; read again until it says the level fits. Hints for popped consonants (a pop filter, or the mic slightly off-axis) and a boomy sound (a little further from the mic) never block anything.
3. **Room:** press Space and stay quiet for five seconds. You get the room's noise floor and how far your voice stands above it (40 dB or more good, 30–40 acceptable, below 30 noisy, with tips), and mains hum when it is there. The recording is kept as room tone in `takes/.room/`, which `clean` export uses for noise reduction on takes recorded on the same input, channel and day at the same gain.
4. **Latency:** only when the latency calibration does not match the devices; it says to run `takewise calibrate --write`.

The results go to `[setup]` in `project.toml`. Every take's check also compares its speech with the noise in its own pre-roll: less than 30 dB apart gives `listen` with `background-noise`.

## Placing the narration in DaVinci Resolve

The narration file (`export/narration.wav` or `.flac`, mono, at the manifest's `sample_rate` and `bit_depth`) starts at the film's first frame and is exactly as long as the film. Takewise has no Resolve code; an agent with Resolve tools places it:

1. Import the narration file named in the manifest's `narration.file` into the media pool.
2. Add a new mono audio track to the film's timeline and place it at the timeline's start (the film's first frame), so sample 0 lines up with frame 0.
3. For each entry in `export/manifest.json` under `lines`, add a timeline marker at `speech_film_s[0]` with the line's `id` as its name and `text` as its note.

To place takes individually instead, put each line's `take_file` (always the raw original at 48 kHz) on the track at `dest_samples[0] / sample_rate` seconds and trim it to `source_samples` (take samples at `source_sample_rate`, 48 kHz); the positions in the manifest are exact sample offsets.

## License

MIT
