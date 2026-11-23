//! Transcription with whisper.cpp's `whisper-cli`.
//!
//! A take is converted to 16 kHz mono 16-bit PCM with `ffmpeg`, then
//! transcribed with
//! `whisper-cli -m <model> -f <wav> -l <language> -nfa -dtw <preset> -ojf -of <out> -np`.
//! The script is never passed as a prompt, so the model does not hear what
//! it expects.
//!
//! **Word times come from DTW, not from the token offsets.** Verified with
//! whisper.cpp 1.9.5 and `ggml-large-v3-turbo.bin` on `say` takes with 0.5,
//! 1.5 and 2.5 s of leading silence: the full JSON's token `offsets` (from
//! whisper's timestamp tokens, the same with or without `-ml 1 -sow`) put the
//! first word at 0.00 s in nearly every take with a second or more of
//! pre-roll, and drift by seconds after it, so early-start checks would be
//! wrong. The DTW times (`t_dtw`, in centiseconds) move exactly with the
//! pre-roll, stay in order, and land 0.1 to 0.4 s after each word's real
//! onset on those takes (0.08 to 0.20 s on real narration), so a measured
//! early start reads a little late, never early. DTW
//! needs the model's alignment-head preset (`-dtw large.v3.turbo`, from the
//! model file name) and is silently disabled with flash attention, so `-nfa`
//! is required; it costs about 0.2 s on a 6 s take. When a word token comes
//! back without a DTW time, the transcription fails (and the take gets a
//! `listen` verdict) rather than falling back to the offsets.
//!
//! A word is the run of tokens from one starting with a space; tokens without
//! letters or digits (punctuation) join the word's text but not its timing.
//! A word starts at its first token's DTW time and ends after the summed
//! duration of its tokens' offsets (durations are sound even where the
//! offsets' positions are not), or at the next word's start, whichever is
//! first. Confidence is the lowest token probability in the word.
//! Annotations such as `[BLANK_AUDIO]` or `(sighs)` become words of their
//! own; the checks flag one inside the speech and ignore the rest.
//!
//! Every failure is soft (a [`Transcript::Failed`]): a tool that does not
//! finish within its timeout is killed, token text is read as bytes so a
//! character split over two tokens joins up again (anything still invalid
//! becomes U+FFFD), and word times out of order or out of range are
//! rejected rather than trusted.

use std::fmt::Write as _;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::check::Transcript;
use crate::ledger::Word;
use crate::project::CheckConfig;
use crate::words::isolate_annotations;

/// Words this far outside the energy speech range are dropped as
/// hallucinations (whisper likes to hear "Thank you." in silence).
pub const SPEECH_MARGIN_S: f64 = 0.3;
/// The models directory used when `$TAKEWISE_MODELS` is not set, under `$HOME`.
pub const DEFAULT_MODELS_DIR: &str = ".local/share/takewise/models";
/// How long `ffmpeg` may take to convert a take before it is stopped.
pub const FFMPEG_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `whisper-cli` may take to transcribe a take before it is stopped.
pub const WHISPER_TIMEOUT: Duration = Duration::from_secs(120);
/// Runs the tools at lower priority, so a check never competes with
/// recording or the film for the CPU.
const NICE: &str = "/usr/bin/nice";
/// The `nice` increment the tools run with.
const TOOL_NICENESS: &str = "10";
/// Searched after `$PATH`, since an app started from Finder gets a minimal one.
const FALLBACK_TOOL_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];
/// whisper.cpp's DTW alignment-head presets.
const DTW_PRESETS: [&str; 12] = [
    "tiny",
    "tiny.en",
    "base",
    "base.en",
    "small",
    "small.en",
    "medium",
    "medium.en",
    "large.v1",
    "large.v2",
    "large.v3",
    "large.v3.turbo",
];

/// Turns a take into words. Implementations fail soft: a failure is a
/// [`Transcript::Failed`] with the reason, never a panic or an error.
pub trait Transcriber {
    fn transcribe(&self, wav: &Path) -> Transcript;
    /// The model's file name, for the `checked` event's `checker` field.
    fn model(&self) -> String;
}

/// Stands in when transcription cannot be set up (see [`SetupError`]):
/// every take's words go unchecked, with the reason, so the sound and timing
/// checks still run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    pub reason: String,
    /// The configured model's file name.
    pub model: String,
}

impl Transcriber for Unavailable {
    fn transcribe(&self, _: &Path) -> Transcript {
        Transcript::Failed(self.reason.clone())
    }
    fn model(&self) -> String {
        self.model.clone()
    }
}

/// Why transcription cannot be set up at all.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error(
        "{tool} not found on PATH or in /opt/homebrew/bin; install it with `brew install {formula}`"
    )]
    MissingTool {
        tool: &'static str,
        formula: &'static str,
    },
    #[error(
        "whisper model {} not found; download it into $TAKEWISE_MODELS or ~/{DEFAULT_MODELS_DIR}",
        .0.display()
    )]
    MissingModel(PathBuf),
    #[error(
        "no word-timing (DTW) preset for whisper model {0:?}; use a standard whisper.cpp model such as ggml-large-v3-turbo.bin"
    )]
    UnsupportedModel(String),
}

impl SetupError {
    /// A stable, kebab-case code for JSON output.
    pub fn code(&self) -> &'static str {
        match self {
            SetupError::MissingTool { .. } => "missing-tool",
            SetupError::MissingModel(_) => "missing-model",
            SetupError::UnsupportedModel(_) => "unsupported-model",
        }
    }
}

/// The real transcriber: `ffmpeg` and `whisper-cli` run as child processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhisperCli {
    pub whisper: PathBuf,
    pub ffmpeg: PathBuf,
    pub model: PathBuf,
    pub language: String,
    /// The DTW preset matching the model, such as `large.v3.turbo`.
    pub dtw_preset: String,
    /// [`FFMPEG_TIMEOUT`] unless changed.
    pub ffmpeg_timeout: Duration,
    /// [`WHISPER_TIMEOUT`] unless changed.
    pub whisper_timeout: Duration,
}

impl WhisperCli {
    /// Checks only that the model's file name has a DTW preset; the paths
    /// are not looked at until [`Transcriber::transcribe`].
    pub fn new(
        whisper: PathBuf,
        ffmpeg: PathBuf,
        model: PathBuf,
        language: &str,
    ) -> Result<WhisperCli, SetupError> {
        let name = model
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let preset = dtw_preset(&name).ok_or(SetupError::UnsupportedModel(name))?;
        Ok(WhisperCli {
            whisper,
            ffmpeg,
            model,
            language: language.to_owned(),
            dtw_preset: preset.to_owned(),
            ffmpeg_timeout: FFMPEG_TIMEOUT,
            whisper_timeout: WHISPER_TIMEOUT,
        })
    }

    /// Finds `whisper-cli` and `ffmpeg` on `$PATH` (then in
    /// `/opt/homebrew/bin` and `/usr/local/bin`) and the configured model in
    /// `$TAKEWISE_MODELS` or `~/.local/share/takewise/models`.
    pub fn discover(config: &CheckConfig) -> Result<WhisperCli, SetupError> {
        let path = std::env::var_os("PATH");
        Self::discover_in(config, &tool_dirs(path.as_deref()), &models_dir())
    }

    /// [`WhisperCli::discover`] with explicit search directories.
    pub fn discover_in(
        config: &CheckConfig,
        tool_dirs: &[PathBuf],
        models_dir: &Path,
    ) -> Result<WhisperCli, SetupError> {
        let find = |tool: &'static str, formula: &'static str| {
            tool_dirs
                .iter()
                .map(|dir| dir.join(tool))
                .find(|path| is_executable(path))
                .ok_or(SetupError::MissingTool { tool, formula })
        };
        let whisper = find("whisper-cli", "whisper-cpp")?;
        let ffmpeg = find("ffmpeg", "ffmpeg")?;
        let model = models_dir.join(&config.whisper_model);
        if !model.is_file() {
            return Err(SetupError::MissingModel(model));
        }
        WhisperCli::new(whisper, ffmpeg, model, &config.language)
    }

    /// The model's file name, for the `checker` field.
    pub fn model_name(&self) -> String {
        self.model
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn run(&self, wav: &Path) -> Result<Vec<Word>, String> {
        let tmp = tempfile::Builder::new()
            .prefix("takewise-whisper-")
            .tempdir()
            .map_err(|err| format!("could not create a temporary directory: {err}"))?;
        let pcm = tmp.path().join("take.wav");
        let out = tmp.path().join("take");
        run_tool(
            tool_command(&self.ffmpeg)
                .args(["-nostdin", "-y", "-loglevel", "error", "-i"])
                .arg(wav)
                .args(["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"])
                .arg(&pcm),
            &self.ffmpeg,
            self.ffmpeg_timeout,
        )?;
        run_tool(
            tool_command(&self.whisper)
                .arg("-m")
                .arg(&self.model)
                .arg("-f")
                .arg(&pcm)
                .args(["-l", &self.language, "-nfa", "-dtw", &self.dtw_preset])
                .arg("-ojf")
                .arg("-of")
                .arg(&out)
                .arg("-np"),
            &self.whisper,
            self.whisper_timeout,
        )?;
        let json_path = out.with_extension("json");
        let json = std::fs::read(&json_path).map_err(|err| {
            format!(
                "could not read whisper-cli output {}: {err}",
                json_path.display()
            )
        })?;
        parse_full_json(json)
    }
}

impl Transcriber for WhisperCli {
    fn transcribe(&self, wav: &Path) -> Transcript {
        match self.run(wav) {
            Ok(words) => Transcript::Words(words),
            Err(message) => Transcript::Failed(message),
        }
    }
    fn model(&self) -> String {
        self.model_name()
    }
}

/// Where tools are looked for: the directories of `path` (a `$PATH` value),
/// then `/opt/homebrew/bin` and `/usr/local/bin`, since an app started from
/// Finder gets only `/usr/bin:/bin:/usr/sbin:/sbin`.
pub fn tool_dirs(path: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path
        .map(|path| std::env::split_paths(path).collect())
        .unwrap_or_default();
    dirs.extend(FALLBACK_TOOL_DIRS.iter().map(PathBuf::from));
    dirs
}

/// The executable `name` in the first of [`tool_dirs`] of `$PATH` that has
/// it, as the checks find their tools.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    tool_dirs(std::env::var_os("PATH").as_deref())
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|path| is_executable(path))
}

/// `$TAKEWISE_MODELS`, or `~/.local/share/takewise/models`.
pub fn models_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TAKEWISE_MODELS") {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(DEFAULT_MODELS_DIR)
}

/// A regular file anyone may execute.
fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// A command running `tool` through `nice` (when the system has it), which
/// execs the tool, so a timeout still stops the tool itself.
fn tool_command(tool: &Path) -> Command {
    if !Path::new(NICE).is_file() {
        return Command::new(tool);
    }
    let mut command = Command::new(NICE);
    command.args(["-n", TOOL_NICENESS]).arg(tool);
    command
}

/// Runs a tool to completion; a failure to start it, a nonzero exit or
/// running longer than `timeout` (the tool is then killed) is an error naming
/// the tool and quoting the end of its stderr.
fn run_tool(command: &mut Command, tool: &Path, timeout: Duration) -> Result<(), String> {
    if !is_executable(tool) {
        return Err(format!(
            "could not run {}: not an executable file",
            tool.display()
        ));
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("could not run {}: {err}", tool.display()))?;
    // Read stderr on the side, so a chatty tool never blocks on a full pipe.
    let (sender, receiver) = mpsc::channel();
    if let Some(mut stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = stderr.read_to_end(&mut buffer);
            let _ = sender.send(buffer);
        });
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            waited => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(match waited {
                    Err(err) => format!("could not wait for {}: {err}", tool.display()),
                    Ok(_) => format!(
                        "{} timed out after {} s and was stopped",
                        tool.display(),
                        timeout.as_secs_f64()
                    ),
                });
            }
        }
    };
    if status.success() {
        return Ok(());
    }
    // A child the tool left behind may hold stderr open; do not wait for it.
    let stderr = receiver
        .recv_timeout(Duration::from_secs(1))
        .unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr);
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines
        .get(lines.len().saturating_sub(3)..)
        .unwrap_or_default();
    let mut message = format!("{} failed ({status})", tool.display());
    if !tail.is_empty() {
        let _ = write!(message, ": {}", tail.join(" / "));
    }
    Err(message)
}

/// The DTW preset for a model file name: `ggml-large-v3-turbo-q5_0.bin` is
/// `large.v3.turbo`. `None` for a name that is not a standard model.
pub fn dtw_preset(model_file: &str) -> Option<&'static str> {
    let name = model_file.strip_suffix(".bin").unwrap_or(model_file);
    let name = name.strip_prefix("ggml-").unwrap_or(name);
    // Drop a quantization suffix such as `-q5_0` or `-q8_0`.
    let name = match name.rfind("-q") {
        Some(at)
            if name
                .get(at + 2..)
                .is_some_and(|q| q.starts_with(|c: char| c.is_ascii_digit())) =>
        {
            name.get(..at).unwrap_or(name)
        }
        _ => name,
    };
    let dotted = name.replace('-', ".");
    DTW_PRESETS.iter().copied().find(|preset| *preset == dotted)
}

#[derive(Deserialize)]
struct FullJson {
    transcription: Vec<Segment>,
}

#[derive(Deserialize)]
struct Segment {
    tokens: Vec<Token>,
}

#[derive(Deserialize)]
struct Token {
    text: RawText,
    offsets: Offsets,
    p: f32,
    #[serde(default)]
    t_dtw: Option<i64>,
}

/// A JSON string's bytes, which need not be valid UTF-8: whisper may split a
/// character over two tokens.
struct RawText(Vec<u8>);

impl<'de> Deserialize<'de> for RawText {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<RawText, D::Error> {
        struct Bytes;
        impl serde::de::Visitor<'_> for Bytes {
            type Value = RawText;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string")
            }
            fn visit_bytes<E>(self, bytes: &[u8]) -> Result<RawText, E> {
                Ok(RawText(bytes.to_vec()))
            }
            fn visit_str<E>(self, text: &str) -> Result<RawText, E> {
                Ok(RawText(text.as_bytes().to_vec()))
            }
        }
        deserializer.deserialize_bytes(Bytes)
    }
}

#[derive(Deserialize)]
struct Offsets {
    from: i64,
    to: i64,
}

/// A word being assembled from tokens.
struct Partial {
    text: Vec<u8>,
    start_cs: i64,
    duration_ms: i64,
    confidence: f32,
}

/// Parses `whisper-cli -ojf -dtw …` output into words (see the module doc).
pub fn parse_full_json(json: impl AsRef<[u8]>) -> Result<Vec<Word>, String> {
    let json: FullJson = serde_json::from_slice(json.as_ref())
        .map_err(|err| format!("could not parse whisper-cli output: {err}"))?;
    let out_of_range = || "whisper-cli gave token times out of range".to_owned();
    let mut partials: Vec<Partial> = Vec::new();
    // Punctuation after a space (an opening quote) waits for the next word.
    let mut prefix: Vec<u8> = Vec::new();
    for token in json.transcription.iter().flat_map(|s| &s.tokens) {
        let bytes = token.text.0.as_slice();
        let text = String::from_utf8_lossy(bytes);
        let special = text.starts_with("[_") && text.ends_with(']');
        if special {
            continue;
        }
        let wordy = text.chars().any(char::is_alphanumeric);
        let spaced = text.starts_with(char::is_whitespace);
        if !wordy {
            match partials.last_mut() {
                Some(word) if !spaced && prefix.is_empty() => word.text.extend_from_slice(bytes),
                _ => prefix.extend_from_slice(bytes),
            }
            continue;
        }
        let Some(t_dtw) = token.t_dtw.filter(|&t| t >= 0) else {
            return Err(format!(
                "whisper-cli gave no word times (DTW) for {:?}; is it whisper.cpp 1.7 or later?",
                text.trim()
            ));
        };
        let duration_ms = token
            .offsets
            .to
            .checked_sub(token.offsets.from)
            .ok_or_else(out_of_range)?
            .max(0);
        match partials.last_mut() {
            Some(word) if !spaced && prefix.is_empty() => {
                word.text.extend_from_slice(bytes);
                word.duration_ms = word
                    .duration_ms
                    .checked_add(duration_ms)
                    .ok_or_else(out_of_range)?;
                word.confidence = word.confidence.min(token.p);
            }
            _ => {
                let mut text = std::mem::take(&mut prefix);
                text.extend_from_slice(bytes);
                partials.push(Partial {
                    text,
                    start_cs: t_dtw,
                    duration_ms,
                    confidence: token.p,
                });
            }
        }
    }
    let starts: Vec<f64> = partials.iter().map(|p| p.start_cs as f64 / 100.0).collect();
    let words = partials
        .iter()
        .zip(&starts)
        .enumerate()
        .map(|(i, (partial, &start))| {
            let mut end = start + partial.duration_ms as f64 / 1000.0;
            if let Some(&next) = starts.get(i + 1) {
                end = end.min(next).max(start);
            }
            Word {
                text: String::from_utf8_lossy(&partial.text).trim().to_owned(),
                start,
                end,
                confidence: Some(partial.confidence),
            }
        })
        .collect();
    let words = isolate_annotations(words);
    if let Some(pair) = words.windows(2).find(|pair| pair[1].start < pair[0].start) {
        return Err(format!(
            "whisper-cli gave word times out of order ({:.2} s after {:.2} s)",
            pair[1].start, pair[0].start
        ));
    }
    Ok(words)
}

/// Drops words entirely outside the energy speech range widened by
/// [`SPEECH_MARGIN_S`] on both sides; keeps everything without a range.
pub fn keep_within(words: Vec<Word>, speech: Option<(f64, f64)>) -> Vec<Word> {
    let Some((start, end)) = speech else {
        return words;
    };
    words
        .into_iter()
        .filter(|w| w.end >= start - SPEECH_MARGIN_S && w.start <= end + SPEECH_MARGIN_S)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::words::{align, normalize};

    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/whisper-jfk.json"
    );

    fn word(text: &str, start: f64, end: f64) -> Word {
        Word {
            text: text.to_owned(),
            start,
            end,
            confidence: None,
        }
    }

    /// Writes an executable shell script.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A fake ffmpeg that creates its last argument, the output file.
    fn fake_ffmpeg(dir: &Path) -> PathBuf {
        script(dir, "ffmpeg", r#"for a; do last=$a; done; : > "$last""#)
    }

    /// A fake whisper-cli that records its arguments in `args.txt` and runs
    /// `then` with `$out` set to its `-of` argument.
    fn fake_whisper(dir: &Path, then: &str) -> PathBuf {
        let args = dir.join("args.txt");
        script(
            dir,
            "whisper-cli",
            &format!(
                r#"echo "$@" > "{}"
while [ $# -gt 0 ]; do if [ "$1" = "-of" ]; then out=$2; fi; shift; done
{then}"#,
                args.display()
            ),
        )
    }

    fn cli(dir: &Path, whisper: PathBuf) -> WhisperCli {
        WhisperCli::new(
            whisper,
            fake_ffmpeg(dir),
            dir.join("ggml-large-v3-turbo.bin"),
            "en",
        )
        .unwrap()
    }

    fn failure(transcript: Transcript) -> String {
        match transcript {
            Transcript::Failed(message) => message,
            Transcript::Words(words) => panic!("expected a failure, got {words:?}"),
        }
    }

    #[test]
    fn parses_full_json_words() {
        let words = parse_full_json(fs::read(FIXTURE).unwrap()).unwrap();
        let text: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(
            text.join(" "),
            "And so, my fellow Americans, ask not what your country can do for you, \
             ask what you can do for your country."
        );
        // Starts are DTW times (centiseconds in the JSON), not segment offsets.
        assert!((words[0].start - 0.50).abs() < 1e-9, "{:?}", words[0]);
        assert!((words[1].start - 0.86).abs() < 1e-9, "{:?}", words[1]);
        for pair in words.windows(2) {
            assert!(pair[0].start <= pair[1].start, "{pair:?}");
            assert!(pair[0].end >= pair[0].start, "{pair:?}");
            assert!(pair[0].end <= pair[1].start + 1e-9, "{pair:?}");
        }
        let last = words.last().unwrap();
        assert!(last.end > last.start && last.end < 11.0, "{last:?}");
        assert!(
            words
                .iter()
                .all(|w| w.confidence.is_some_and(|p| (0.0..=1.0).contains(&p)))
        );
        // The transcript aligns with the speech it came from.
        let expected = normalize(
            "And so, my fellow Americans, ask not what your country can do for you; \
             ask what you can do for your country.",
        );
        assert_eq!(align(&expected, &words), []);
    }

    #[test]
    fn parses_words_split_over_tokens() {
        let json = r#"{"transcription": [{"tokens": [
            {"text": "[_BEG_]", "offsets": {"from": 0, "to": 0}, "p": 0.2, "t_dtw": -1},
            {"text": " Kind", "offsets": {"from": 0, "to": 460}, "p": 0.6, "t_dtw": 124},
            {"text": "red", "offsets": {"from": 460, "to": 800}, "p": 0.7, "t_dtw": 142},
            {"text": " open", "offsets": {"from": 2000, "to": 2290}, "p": 0.9, "t_dtw": 256},
            {"text": "-", "offsets": {"from": 2290, "to": 2380}, "p": 0.4, "t_dtw": 256},
            {"text": "source", "offsets": {"from": 2450, "to": 3120}, "p": 0.8, "t_dtw": 298},
            {"text": ".", "offsets": {"from": 3120, "to": 3280}, "p": 0.4, "t_dtw": 322},
            {"text": " \"", "offsets": {"from": 3280, "to": 3290}, "p": 0.5, "t_dtw": 330},
            {"text": "Try", "offsets": {"from": 3290, "to": 3560}, "p": 0.9, "t_dtw": 340},
            {"text": "[_TT_264]", "offsets": {"from": 5280, "to": 5280}, "p": 0.04, "t_dtw": -1}
        ]}]}"#;
        let words = parse_full_json(json).unwrap();
        let text: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(text, ["Kindred", "open-source.", "\"Try"]);
        assert!((words[0].start - 1.24).abs() < 1e-9);
        // Kindred lasts 0.8 s by its tokens' durations; the next word
        // starts later, so that duration stands.
        assert!((words[0].end - 2.04).abs() < 1e-9, "{:?}", words[0]);
        // open-source would last 0.96 s by its word tokens, but Try starts
        // first.
        assert!((words[1].end - 3.40).abs() < 1e-9, "{:?}", words[1]);
        assert_eq!(words[0].confidence, Some(0.6));
    }

    #[test]
    fn parses_annotations_as_words_of_their_own() {
        let json = r#"{"transcription": [{"tokens": [
            {"text": " [", "offsets": {"from": 0, "to": 10}, "p": 0.5, "t_dtw": 10},
            {"text": "BL", "offsets": {"from": 10, "to": 20}, "p": 0.5, "t_dtw": 12},
            {"text": "ANK", "offsets": {"from": 20, "to": 30}, "p": 0.5, "t_dtw": 14},
            {"text": "_", "offsets": {"from": 30, "to": 40}, "p": 0.5, "t_dtw": 16},
            {"text": "AUDIO", "offsets": {"from": 40, "to": 50}, "p": 0.5, "t_dtw": 18},
            {"text": "]", "offsets": {"from": 50, "to": 60}, "p": 0.5, "t_dtw": 20},
            {"text": " Nobody", "offsets": {"from": 100, "to": 400}, "p": 0.9, "t_dtw": 110},
            {"text": " (", "offsets": {"from": 400, "to": 410}, "p": 0.5, "t_dtw": 140},
            {"text": "sigh", "offsets": {"from": 410, "to": 500}, "p": 0.5, "t_dtw": 145},
            {"text": "s", "offsets": {"from": 500, "to": 510}, "p": 0.5, "t_dtw": 150},
            {"text": ")", "offsets": {"from": 510, "to": 520}, "p": 0.5, "t_dtw": 152},
            {"text": " misses", "offsets": {"from": 600, "to": 900}, "p": 0.9, "t_dtw": 160}
        ]}]}"#;
        let words = parse_full_json(json).unwrap();
        let text: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(text, ["[BLANK_AUDIO]", "Nobody", "(sighs)", "misses"]);
        assert!((words[1].start - 1.10).abs() < 1e-9, "{:?}", words[1]);
    }

    #[test]
    fn parses_without_dtw_times_fails() {
        let json = r#"{"transcription": [{"tokens": [
            {"text": " Hello", "offsets": {"from": 0, "to": 460}, "p": 0.6, "t_dtw": -1}
        ]}]}"#;
        let err = parse_full_json(json).unwrap_err();
        assert!(err.contains("word times"), "{err}");
    }

    #[test]
    fn drops_words_outside_speech() {
        let words = vec![
            word("um", 0.1, 0.4),
            word("Nobody", 1.1, 1.5),
            word("misses", 1.5, 1.9),
            word("birthday.", 2.0, 2.6),
            word("Thank", 4.0, 4.3),
            word("you.", 4.3, 4.6),
        ];
        let kept = keep_within(words.clone(), Some((1.0, 2.5)));
        let text: Vec<&str> = kept.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(text, ["Nobody", "misses", "birthday."]);
        // Within 0.3 s of the speech still counts.
        let kept = keep_within(words.clone(), Some((0.6, 4.1)));
        assert_eq!(kept.len(), 6);
        // Without a speech range nothing is dropped.
        assert_eq!(keep_within(words.clone(), None), words);
    }

    #[test]
    fn transcribes_with_dtw_and_no_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let whisper = fake_whisper(tmp.path(), &format!(r#"cp "{FIXTURE}" "$out.json""#));
        let cli = cli(tmp.path(), whisper);
        let Transcript::Words(words) = cli.transcribe(&tmp.path().join("take.wav")) else {
            panic!("expected words");
        };
        assert_eq!(words.len(), 22);
        let args = fs::read_to_string(tmp.path().join("args.txt")).unwrap();
        for expected in ["-nfa", "-dtw large.v3.turbo", "-ojf", "-l en", "-np"] {
            assert!(args.contains(expected), "{expected} missing from {args}");
        }
        assert!(!args.contains("prompt"), "{args}");
        // The temporary directory is gone.
        let out = args.split_whitespace().skip_while(|a| *a != "-of").nth(1);
        let out = PathBuf::from(out.unwrap());
        assert!(!out.parent().unwrap().exists(), "{}", out.display());
    }

    #[test]
    fn missing_binary_fails_soft() {
        let tmp = tempfile::tempdir().unwrap();
        let cli = cli(tmp.path(), tmp.path().join("no-such-whisper-cli"));
        let message = failure(cli.transcribe(&tmp.path().join("take.wav")));
        assert!(message.contains("no-such-whisper-cli"), "{message}");

        let mut cli = cli;
        cli.whisper = fake_whisper(tmp.path(), "exit 0");
        cli.ffmpeg = tmp.path().join("no-such-ffmpeg");
        let message = failure(cli.transcribe(&tmp.path().join("take.wav")));
        assert!(message.contains("no-such-ffmpeg"), "{message}");
    }

    #[test]
    fn tools_run_at_lower_priority() {
        let tmp = tempfile::tempdir().unwrap();
        let seen = tmp.path().join("nice.txt");
        let record = format!(r#"ps -o nice= -p $$ >> "{}""#, seen.display());
        let mut cli = cli(tmp.path(), fake_whisper(tmp.path(), &record));
        cli.ffmpeg = script(
            tmp.path(),
            "ffmpeg",
            &format!(r#"{record}; for a; do last=$a; done; : > "$last""#),
        );
        let _ = cli.transcribe(&tmp.path().join("t.wav"));
        let out = std::process::Command::new("ps")
            .args(["-o", "nice=", "-p", &std::process::id().to_string()])
            .output()
            .unwrap();
        let ours: i32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        let expected = (ours + 10).min(20);
        let seen: Vec<i32> = fs::read_to_string(&seen)
            .unwrap()
            .lines()
            .map(|line| line.trim().parse().unwrap())
            .collect();
        assert_eq!(seen, vec![expected, expected], "ffmpeg, then whisper-cli");
    }

    #[test]
    fn nonzero_exit_fails_soft() {
        let tmp = tempfile::tempdir().unwrap();
        let whisper = fake_whisper(tmp.path(), "echo 'error: failed to load model' >&2; exit 3");
        let message = failure(cli(tmp.path(), whisper).transcribe(&tmp.path().join("t.wav")));
        assert!(message.contains("failed to load model"), "{message}");
        assert!(message.contains('3'), "{message}");
    }

    #[test]
    fn a_hung_tool_is_stopped_and_fails_soft() {
        let tmp = tempfile::tempdir().unwrap();
        let whisper = fake_whisper(tmp.path(), "exec sleep 30");
        let mut cli = cli(tmp.path(), whisper);
        cli.whisper_timeout = std::time::Duration::from_millis(200);
        let started = std::time::Instant::now();
        let message = failure(cli.transcribe(&tmp.path().join("t.wav")));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(message.contains("whisper-cli"), "{message}");
        assert!(message.contains("timed out after 0.2 s"), "{message}");

        cli.ffmpeg = script(tmp.path(), "slow-ffmpeg", "exec sleep 30");
        cli.ffmpeg_timeout = std::time::Duration::from_millis(200);
        let message = failure(cli.transcribe(&tmp.path().join("t.wav")));
        assert!(message.contains("slow-ffmpeg"), "{message}");
        assert!(message.contains("timed out"), "{message}");
    }

    #[test]
    fn default_timeouts() {
        let tmp = tempfile::tempdir().unwrap();
        let cli = cli(tmp.path(), tmp.path().join("whisper-cli"));
        assert_eq!(cli.ffmpeg_timeout, std::time::Duration::from_secs(30));
        assert_eq!(cli.whisper_timeout, std::time::Duration::from_secs(120));
    }

    #[test]
    fn reads_output_with_split_utf8_lossily() {
        let tmp = tempfile::tempdir().unwrap();
        // whisper splits a multibyte character over two tokens, so each
        // token's text is invalid UTF-8 on its own.
        // `@1` and `@2` stand for the two halves of "é".
        let template = r#"{"transcription": [{"tokens": [
            {"text": " Caf", "offsets": {"from": 0, "to": 300}, "p": 0.9, "t_dtw": 100},
            {"text": "@1", "offsets": {"from": 300, "to": 350}, "p": 0.5, "t_dtw": 120},
            {"text": "@2", "offsets": {"from": 350, "to": 400}, "p": 0.5, "t_dtw": 125},
            {"text": " open", "offsets": {"from": 500, "to": 800}, "p": 0.9, "t_dtw": 150}
        ]}]}"#;
        let mut json = Vec::new();
        for (i, piece) in template.split('@').enumerate() {
            let piece = piece.as_bytes();
            match (i, piece.first()) {
                (0, _) => json.extend_from_slice(piece),
                (_, Some(b'1')) => json.push(0xc3),
                (_, _) => json.push(0xa9),
            }
            if i > 0 {
                json.extend_from_slice(&piece[1..]);
            }
        }
        let source = tmp.path().join("split.json");
        fs::write(&source, json).unwrap();
        let whisper = fake_whisper(
            tmp.path(),
            &format!(r#"cp "{}" "$out.json""#, source.display()),
        );
        let Transcript::Words(words) =
            cli(tmp.path(), whisper).transcribe(&tmp.path().join("t.wav"))
        else {
            panic!("expected words");
        };
        let text: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(text, ["Café", "open"]);
        // A byte that never completes a character becomes U+FFFD.
        let json = br#"{"transcription": [{"tokens": [
            {"text": " Caf", "offsets": {"from": 0, "to": 300}, "p": 0.9, "t_dtw": 100}
        ]}]}"#;
        let mut broken = json.to_vec();
        let at = broken.iter().position(|&b| b == b'C').unwrap();
        broken.insert(at + 3, 0xc3);
        let words = parse_full_json(&broken).unwrap();
        assert_eq!(words[0].text, "Caf\u{fffd}");
    }

    #[test]
    fn parses_without_a_dtw_key_fails_helpfully() {
        let json = r#"{"transcription": [{"tokens": [
            {"text": " Hello", "offsets": {"from": 0, "to": 460}, "p": 0.6}
        ]}]}"#;
        let err = parse_full_json(json).unwrap_err();
        assert!(err.contains("word times (DTW)"), "{err}");
    }

    #[test]
    fn parses_out_of_order_or_out_of_range_times_fails_soft() {
        let json = r#"{"transcription": [{"tokens": [
            {"text": " Nobody", "offsets": {"from": 0, "to": 300}, "p": 0.9, "t_dtw": 150},
            {"text": " misses", "offsets": {"from": 300, "to": 600}, "p": 0.9, "t_dtw": 120}
        ]}]}"#;
        let err = parse_full_json(json).unwrap_err();
        assert!(err.contains("out of order"), "{err}");

        let json = r#"{"transcription": [{"tokens": [
            {"text": " Nobody", "offsets": {"from": -9223372036854775808, "to": 9223372036854775807}, "p": 0.9, "t_dtw": 150}
        ]}]}"#;
        let err = parse_full_json(json).unwrap_err();
        assert!(err.contains("out of range"), "{err}");

        let json = r#"{"transcription": [{"tokens": [
            {"text": " Kind", "offsets": {"from": 0, "to": 9223372036854775807}, "p": 0.9, "t_dtw": 150},
            {"text": "red", "offsets": {"from": 0, "to": 9223372036854775807}, "p": 0.9, "t_dtw": 160}
        ]}]}"#;
        let err = parse_full_json(json).unwrap_err();
        assert!(err.contains("out of range"), "{err}");
    }

    #[test]
    fn tools_are_found_in_homebrew_with_a_minimal_path() {
        let dirs = tool_dirs(Some(std::ffi::OsStr::new("/usr/bin:/bin")));
        let dirs: Vec<&str> = dirs.iter().filter_map(|dir| dir.to_str()).collect();
        assert_eq!(
            dirs,
            ["/usr/bin", "/bin", "/opt/homebrew/bin", "/usr/local/bin"]
        );
        assert_eq!(tool_dirs(None).len(), 2);
    }

    #[test]
    fn discover_skips_files_that_are_not_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        let models = tmp.path().join("models");
        for dir in [&first, &second, &models] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(first.join("whisper-cli"), "not a program").unwrap();
        fs::write(first.join("ffmpeg"), "not a program").unwrap();
        fake_whisper(&second, "exit 0");
        fake_ffmpeg(&second);
        fs::write(models.join("ggml-large-v3-turbo.bin"), "").unwrap();
        let cli =
            WhisperCli::discover_in(&CheckConfig::default(), &[first, second.clone()], &models)
                .unwrap();
        assert_eq!(cli.whisper, second.join("whisper-cli"));
        assert_eq!(cli.ffmpeg, second.join("ffmpeg"));
    }

    #[test]
    fn garbage_json_fails_soft() {
        let tmp = tempfile::tempdir().unwrap();
        let whisper = fake_whisper(tmp.path(), r#"echo 'not json' > "$out.json""#);
        let message = failure(cli(tmp.path(), whisper).transcribe(&tmp.path().join("t.wav")));
        assert!(message.contains("whisper-cli output"), "{message}");

        let whisper = fake_whisper(tmp.path(), "exit 0");
        let message = failure(cli(tmp.path(), whisper).transcribe(&tmp.path().join("t.wav")));
        assert!(message.contains("whisper-cli output"), "{message}");
    }

    #[test]
    fn dtw_presets_follow_model_names() {
        assert_eq!(
            dtw_preset("ggml-large-v3-turbo.bin"),
            Some("large.v3.turbo")
        );
        assert_eq!(
            dtw_preset("ggml-large-v3-turbo-q5_0.bin"),
            Some("large.v3.turbo")
        );
        assert_eq!(dtw_preset("ggml-large-v3.bin"), Some("large.v3"));
        assert_eq!(dtw_preset("ggml-base.en.bin"), Some("base.en"));
        assert_eq!(dtw_preset("ggml-small-q8_0.bin"), Some("small"));
        assert_eq!(dtw_preset("my-finetune.bin"), None);
    }

    #[test]
    fn discover_finds_tools_and_reports_what_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        let models = tmp.path().join("models");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&models).unwrap();
        let config = CheckConfig::default();
        let dirs = [tmp.path().join("empty"), bin.clone()];

        let err = WhisperCli::discover_in(&config, &dirs, &models).unwrap_err();
        assert_eq!(err.code(), "missing-tool");
        assert!(err.to_string().contains("whisper-cli"), "{err}");

        fake_whisper(&bin, "exit 0");
        let err = WhisperCli::discover_in(&config, &dirs, &models).unwrap_err();
        assert!(err.to_string().contains("ffmpeg"), "{err}");

        fake_ffmpeg(&bin);
        let err = WhisperCli::discover_in(&config, &dirs, &models).unwrap_err();
        assert_eq!(err.code(), "missing-model");
        assert!(err.to_string().contains("ggml-large-v3-turbo.bin"), "{err}");

        fs::write(models.join("ggml-large-v3-turbo.bin"), "").unwrap();
        let cli = WhisperCli::discover_in(&config, &dirs, &models).unwrap();
        assert_eq!(cli.whisper, bin.join("whisper-cli"));
        assert_eq!(cli.ffmpeg, bin.join("ffmpeg"));
        assert_eq!(cli.model, models.join("ggml-large-v3-turbo.bin"));
        assert_eq!(cli.language, "en");
        assert_eq!(cli.dtw_preset, "large.v3.turbo");

        let custom = CheckConfig {
            whisper_model: "custom.bin".to_owned(),
            ..CheckConfig::default()
        };
        fs::write(models.join("custom.bin"), "").unwrap();
        let err = WhisperCli::discover_in(&custom, &dirs, &models).unwrap_err();
        assert_eq!(err.code(), "unsupported-model");
    }

    /// Speaks a line with macOS `say` after 1.5 s of silence and checks the
    /// real whisper-cli hears every word and places the first one after the
    /// silence. Run with `cargo test -p takewise-core -- --ignored`.
    #[test]
    #[ignore = "needs say, ffmpeg, whisper-cli and the model"]
    fn real_whisper_transcribes_say_speech() {
        const LINE: &str = "Kindred is free and open source. Try the demo; the link's below.";
        const PREROLL_S: f64 = 1.5;
        let cli = WhisperCli::discover(&CheckConfig::default()).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let spoken = tmp.path().join("spoken.aiff");
        let take = tmp.path().join("take.wav");
        let status = std::process::Command::new("say")
            .arg("-o")
            .arg(&spoken)
            .arg(LINE)
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new(&cli.ffmpeg)
            .args(["-nostdin", "-y", "-loglevel", "error", "-i"])
            .arg(&spoken)
            .args(["-af", "adelay=1500:all=1,apad=pad_dur=1", "-ar", "48000"])
            .args(["-ac", "1", "-c:a", "pcm_s24le"])
            .arg(&take)
            .status()
            .unwrap();
        assert!(status.success());

        let started = std::time::Instant::now();
        let transcript = cli.transcribe(&take);
        let elapsed = started.elapsed();
        let Transcript::Words(words) = transcript else {
            panic!("{transcript:?}");
        };
        eprintln!("transcribed in {elapsed:.2?}: {words:?}");
        assert_eq!(align(&normalize(LINE), &words), []);
        let first = words[0].start;
        assert!(
            (PREROLL_S - 0.1..=PREROLL_S + 0.5).contains(&first),
            "first word at {first}"
        );
    }
}
