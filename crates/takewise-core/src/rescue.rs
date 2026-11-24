//! Rescue copies: audio the recording window still held only in memory
//! when it had to close, because saving it into the project kept failing
//! (the project's disk is gone or full, its folder cannot be written).
//!
//! A rescue copy does not depend on the project's disk. It lives in
//! `rescue/<project folder name>-<timestamp>/` beside the settings file
//! (`~/Library/Application Support/Takewise/rescue/…`; beside the file
//! `TAKEWISE_SETTINGS` names, when set): each take or passage as an
//! ordinary WAV, as the project would hold it, with a JSON file of the
//! same name that says which project and line it belongs to and carries
//! everything the project would have stored with it (the line's snapshot,
//! the device, where the audio lies in the film).
//!
//! Nothing here writes into a project, and nothing removes or replaces a
//! rescue copy: every copy gets a file name that nothing in its folder
//! has, and a WAV whose JSON file could not be written stays where it is.
//! Bringing one back is done by hand (see `docs/reference.md`, "Rescued
//! audio"): the JSON file names the two files to create in the project,
//! after which `takewise status --repair` adopts the audio as it adopts a
//! take a crash left behind. `status` says when a project has rescue
//! copies ([`notices`]).

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result, io};
use crate::fsutil::{rename_temp, sync_dir, sync_parent, write_atomic, write_temp};
use crate::passage::{PASSAGES_DIR, PassageInput};
use crate::project::TAKES_DIR;
use crate::script::{Problem, is_valid_id};
use crate::takes::{RecordedInput, encode_wav};

/// The folder of the rescue copies, beside the settings file.
pub const RESCUE_DIR: &str = "rescue";

/// The version of [`RescuedAudio`].
const VERSION: u32 = 1;

/// The most bytes of a line id that a rescue copy's file name carries. A
/// line id may be as long as a folder name (`takes/<line>/`), which leaves
/// no room for the rest of the file name.
const NAME_ID_BYTES: usize = 80;

/// The most bytes of a project folder's name that a rescue folder's name
/// carries, which leaves room for the timestamp after it.
const NAME_PROJECT_BYTES: usize = 200;

/// The rescue folder beside the settings file
/// ([`crate::settings_path`], which `TAKEWISE_SETTINGS` overrides), if
/// there is a home.
pub fn rescue_root() -> Option<PathBuf> {
    crate::settings_path().map(|settings| beside(&settings))
}

/// The rescue folder beside the settings file `settings`.
pub fn beside(settings: &Path) -> PathBuf {
    settings.with_file_name(RESCUE_DIR)
}

/// What a rescue copy is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Take,
    Passage,
}

/// Where a rescue copy's two files go in its project, relative to the
/// project folder, for `takewise status --repair` to adopt them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restore {
    /// Where to copy the WAV, such as
    /// `takes/hook/.6f0e….wav.partial`.
    pub audio: String,
    /// Where to write [`RescuedAudio::sidecar`], such as
    /// `takes/hook/.6f0e….json.partial`.
    pub sidecar: String,
}

/// A rescue copy's JSON file, beside its WAV.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RescuedAudio {
    pub v: u32,
    pub kind: Kind,
    /// The project the audio was recorded for: its folder, absolute.
    pub project: String,
    pub rescued_at: jiff::Timestamp,
    /// The WAV beside this file: mono 24-bit PCM, as a take's.
    pub audio: String,
    pub sample_rate: u32,
    pub samples: u64,
    /// The line of a take, or the lines of a passage, in script order.
    pub lines: Vec<String>,
    pub restore: Restore,
    /// What the project stores beside the audio while it is recorded, to
    /// be written to [`Restore::sidecar`] as it is: for a take the line's
    /// snapshot, the device and the take's offset from the line's start;
    /// for a passage its lines with their snapshots, the device, the
    /// latency and the film time of its first sample.
    pub sidecar: serde_json::Value,
}

/// A rescue folder being written: `<root>/<project folder name>-<timestamp>/`.
#[derive(Debug)]
pub struct Rescue {
    folder: PathBuf,
    project: String,
    at: jiff::Timestamp,
    /// How many WAVs the folder holds: the copies, and the audio whose
    /// JSON file could not be written.
    count: usize,
    /// The folder [`RESCUE_DIR`] was made for this one.
    new_root: bool,
}

impl Rescue {
    /// Makes a new rescue folder in `root` for audio of the project in
    /// `project_dir`, rescued at `at`. A folder of that name that exists
    /// (two rescues within a second) is never reused.
    pub fn create(root: &Path, project_dir: &Path, at: jiff::Timestamp) -> Result<Rescue> {
        #[cfg(test)]
        assert!(
            root.starts_with(std::env::temp_dir())
                || fs::canonicalize(std::env::temp_dir()).is_ok_and(|tmp| root.starts_with(tmp)),
            "a test rescued audio outside the temporary folder: {}",
            root.display()
        );
        let new_root = !root.is_dir();
        fs::create_dir_all(root).map_err(io(root))?;
        let name = folder_prefix(project_dir);
        let stamp = at.strftime("%Y%m%dT%H%M%SZ").to_string();
        for n in 1..1000 {
            let folder = match n {
                1 => root.join(format!("{name}-{stamp}")),
                n => root.join(format!("{name}-{stamp}-{n}")),
            };
            match fs::create_dir(&folder) {
                Ok(()) => {
                    return Ok(Rescue {
                        folder,
                        project: absolute(project_dir).display().to_string(),
                        at,
                        count: 0,
                        new_root,
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(io(&folder)(err)),
            }
        }
        Err(io(root)(std::io::Error::other(
            "no free name for a rescue folder",
        )))
    }

    /// The folder the copies are written into.
    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// Copies a take of `line` into the folder: its audio from `samples`,
    /// and `input` as the project would store it. Returns the WAV.
    pub fn add_take(
        &mut self,
        line: &str,
        input: &RecordedInput,
        samples: &[i32],
    ) -> Result<PathBuf> {
        if !is_valid_id(line) {
            return Err(Error::UnknownLine(line.to_owned()));
        }
        let id = uuid::Uuid::new_v4();
        let audio = format!(".{id}.wav.partial");
        let into = format!("{TAKES_DIR}/{line}");
        self.add(
            Kind::Take,
            &format!("take-{}", shortened(line, NAME_ID_BYTES)),
            vec![line.to_owned()],
            input.sample_rate,
            samples,
            Restore {
                audio: format!("{into}/{audio}"),
                sidecar: format!("{into}/.{id}.json.partial"),
            },
            &|path| crate::takes::partial_meta_json(path, input, &audio),
        )
    }

    /// Copies a passage into the folder: its audio from `samples`, and
    /// `input` as the project would store it. Returns the WAV.
    pub fn add_passage(&mut self, input: &PassageInput, samples: &[i32]) -> Result<PathBuf> {
        if let Some(bad) = input.lines.iter().find(|line| !is_valid_id(&line.id)) {
            return Err(Error::UnknownLine(bad.id.clone()));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let into = format!("{TAKES_DIR}/{PASSAGES_DIR}");
        self.add(
            Kind::Passage,
            "passage",
            input.lines.iter().map(|line| line.id.clone()).collect(),
            input.sample_rate,
            samples,
            Restore {
                audio: format!("{into}/.{id}.wav.partial"),
                sidecar: format!("{into}/.{id}.json.partial"),
            },
            &|path| crate::passage::partial_sidecar_json(path, &id, input),
        )
    }

    /// Writes one copy: the WAV first, then its JSON file, each synced
    /// before it gets its name, so a JSON file always has its audio.
    ///
    /// The copy's name is reserved first ([`Rescue::reserve`]), so it
    /// never replaces a file that is there. When the audio is written and
    /// its JSON file cannot be, the WAV stays and the error names it
    /// ([`Error::RescueJson`]): it is then the only copy of that audio.
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        kind: Kind,
        what: &str,
        lines: Vec<String>,
        sample_rate: u32,
        samples: &[i32],
        restore: Restore,
        sidecar: &dyn Fn(&Path) -> Result<serde_json::Value>,
    ) -> Result<PathBuf> {
        let (stem, wav) = self.reserve(what)?;
        let json = self.folder.join(format!("{stem}.json"));
        let audio = || -> Result<Vec<u8>> {
            let rescued = RescuedAudio {
                v: VERSION,
                kind,
                project: self.project.clone(),
                rescued_at: self.at,
                audio: format!("{stem}.wav"),
                sample_rate,
                samples: samples.len() as u64,
                lines,
                restore,
                sidecar: sidecar(&json)?,
            };
            let text = serde_json::to_vec_pretty(&rescued).map_err(|err| Error::Parse {
                path: json.clone(),
                message: err.to_string(),
            })?;
            let temp = write_temp(&wav, |file| {
                let copy = file.try_clone()?;
                encode_wav(copy, samples, sample_rate)
                    .map(drop)
                    .map_err(std::io::Error::other)
            })?;
            rename_temp(&temp, &wav)?;
            Ok(text)
        };
        let text = match audio() {
            Ok(text) => text,
            Err(err) => {
                // The audio did not get its name: the reservation, which
                // is still the empty file made above, frees it again.
                if fs::metadata(&wav).is_ok_and(|reserved| reserved.len() == 0) {
                    let _ = fs::remove_file(&wav);
                }
                return Err(err);
            }
        };
        // The name is taken from here on, whatever becomes of the JSON.
        self.count += 1;
        write_json(&json, &text).map_err(|err| Error::RescueJson {
            wav: wav.clone(),
            source: Box::new(err),
        })?;
        Ok(wav)
    }

    /// Reserves the next free name for a copy of `what`, `NNN-<what>`, by
    /// creating its WAV, empty, only if nothing has that name: the audio
    /// is then renamed over that empty file and over nothing else. Returns
    /// the name without its extension, and the WAV.
    fn reserve(&self, what: &str) -> Result<(String, PathBuf)> {
        for number in self.count + 1..self.count + 1000 {
            let stem = format!("{number:03}-{what}");
            let wav = self.folder.join(format!("{stem}.wav"));
            match OpenOptions::new().write(true).create_new(true).open(&wav) {
                Ok(_) => return Ok((stem, wav)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(io(&wav)(err)),
            }
        }
        Err(io(&self.folder)(std::io::Error::other(
            "no free name for a rescue copy",
        )))
    }

    /// Makes the copies durable: syncs the folder and its place in the
    /// rescue folder. Only then is the audio safe outside this process's
    /// memory. Returns the folder.
    pub fn finish(self) -> Result<PathBuf> {
        sync_dir(&self.folder)?;
        sync_parent(&self.folder)?;
        if self.new_root
            && let Some(root) = self.folder.parent()
        {
            sync_parent(root)?;
        }
        Ok(self.folder)
    }
}

/// The start of the name of every rescue folder of the project in
/// `project_dir`: the project folder's name, shortened when it is longer
/// than [`NAME_PROJECT_BYTES`].
fn folder_prefix(project_dir: &Path) -> String {
    project_dir.file_name().map_or_else(
        || "project".to_owned(),
        |name| shortened(&name.to_string_lossy(), NAME_PROJECT_BYTES),
    )
}

/// Whether `folder` is named as [`Rescue::create`] names a rescue folder
/// of the project in `project_dir`: `<project folder name>-<timestamp>`,
/// or that with `-<n>`.
fn named_for(folder: &Path, project_dir: &Path) -> bool {
    let name = folder.file_name().map(|name| name.to_string_lossy());
    let Some(rest) = name
        .as_deref()
        .and_then(|name| name.strip_prefix(folder_prefix(project_dir).as_str()))
        .and_then(|rest| rest.strip_prefix('-'))
    else {
        return false;
    };
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    // `20261124T101530Z`, as `create` writes the time.
    let (Some(stamp), Some(more)) = (rest.get(..16), rest.get(16..)) else {
        return false;
    };
    let stamped =
        digits(&stamp[..8]) && &stamp[8..9] == "T" && digits(&stamp[9..15]) && &stamp[15..] == "Z";
    stamped && (more.is_empty() || more.strip_prefix('-').is_some_and(digits))
}

/// `name` as part of a file name, in at most `max` bytes: itself, or,
/// when it is longer, its beginning and eight hex digits of its SHA-256,
/// which tell it from another name that begins the same. A copy's JSON
/// file has what was shortened whole (`lines`, `project`).
fn shortened(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_owned();
    }
    let digest = Sha256::digest(name.as_bytes());
    let hash = format!(
        "{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    );
    let head = &name[..name.floor_char_boundary(max.saturating_sub(1 + hash.len()))];
    format!("{}-{hash}", head.trim_end_matches('-'))
}

#[cfg(any(test, feature = "test-faults"))]
thread_local! {
    static FAIL_JSON_WRITES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Makes writing a rescue copy's JSON file fail on this thread, as a disk
/// that fills up just after the audio can, until `false` is given. For
/// tests.
#[cfg(any(test, feature = "test-faults"))]
pub fn fail_json_writes(fail: bool) {
    FAIL_JSON_WRITES.set(fail);
}

/// Writes a rescue copy's JSON file `json`, synced before it gets its
/// name.
fn write_json(json: &Path, text: &[u8]) -> Result<()> {
    #[cfg(any(test, feature = "test-faults"))]
    if FAIL_JSON_WRITES.get() {
        let failed = std::io::Error::other("the disk failed (test)");
        return Err(io(json)(failed));
    }
    write_atomic(json, text)
}

/// `dir` as an absolute path, resolved when it can be (the project's disk
/// may be gone when its audio is rescued).
fn absolute(dir: &Path) -> PathBuf {
    fs::canonicalize(dir)
        .or_else(|_| std::path::absolute(dir))
        .unwrap_or_else(|_| dir.to_path_buf())
}

/// A rescue folder that holds audio of a project.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub folder: PathBuf,
    /// Its copies of that project's audio, in the order they were written.
    pub audio: Vec<RescuedAudio>,
    /// Its WAVs that no JSON file describes, by file name: audio whose
    /// JSON file could not be written. Nothing says which line it is of
    /// but its name, so a repair cannot adopt it.
    pub without_json: Vec<String>,
}

/// The rescue folders in `root` with audio of the project in
/// `project_dir`, oldest first. A missing `root` holds none; files that do
/// not read as a rescue copy's are skipped.
///
/// A WAV that no JSON file describes is listed too
/// ([`Found::without_json`]), in a folder that has copies of this project,
/// or that has no copy of any project and is named for this one: a rescue
/// folder is written for one project, and its name is all that says which
/// when no JSON file could be written.
pub fn rescued_for(root: &Path, project_dir: &Path) -> Vec<Found> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let project = absolute(project_dir);
    let ours = |rescued: &RescuedAudio| {
        let from = Path::new(&rescued.project);
        from == project || from == project_dir || absolute(from) == project
    };
    let mut found: Vec<Found> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|folder| folder.is_dir())
        .filter_map(|folder| {
            let mut files: Vec<PathBuf> = fs::read_dir(&folder)
                .ok()?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect();
            files.sort();
            let has = |path: &Path, ext: &str| path.extension().is_some_and(|e| e == ext);
            // The copies of any project, to know which WAVs have one.
            let copies: Vec<RescuedAudio> = files
                .iter()
                .filter(|path| has(path, "json"))
                .filter_map(|path| serde_json::from_slice(&fs::read(path).ok()?).ok())
                .collect();
            let mine = if copies.is_empty() {
                named_for(&folder, project_dir)
            } else {
                copies.iter().any(&ours)
            };
            let without_json: Vec<String> = files
                .iter()
                .filter(|path| mine && has(path, "wav"))
                // An empty WAV is a name reserved for audio that never
                // arrived (see `Rescue::reserve`).
                .filter(|path| fs::metadata(path).is_ok_and(|wav| wav.len() > 0))
                .filter_map(|path| Some(path.file_name()?.to_string_lossy().into_owned()))
                .filter(|name| !copies.iter().any(|copy| &copy.audio == name))
                .collect();
            let audio: Vec<RescuedAudio> = copies
                .into_iter()
                .filter(|rescued| ours(rescued) && folder.join(&rescued.audio).is_file())
                .collect();
            (!audio.is_empty() || !without_json.is_empty()).then_some(Found {
                folder,
                audio,
                without_json,
            })
        })
        .collect();
    found.sort_by_key(|found| {
        let first = found.audio.first().map(|rescued| rescued.rescued_at);
        (first, found.folder.clone())
    });
    found
}

/// "1 take (hook) and 1 passage (a, b)": what a rescue folder holds.
pub fn describe(audio: &[RescuedAudio]) -> String {
    let lines = |kind: Kind| -> Vec<String> {
        audio
            .iter()
            .filter(|rescued| rescued.kind == kind)
            .map(|rescued| rescued.lines.join(", "))
            .collect()
    };
    let part = |kind: Kind, one: &str, many: &str| {
        let lines = lines(kind);
        match lines.as_slice() {
            [] => None,
            [only] => Some(format!("1 {one} ({only})")),
            several => Some(format!("{} {many} ({})", several.len(), several.join("; "))),
        }
    };
    [
        part(Kind::Take, "take", "takes"),
        part(Kind::Passage, "passage", "passages"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" and ")
}

/// A `rescued-audio` notice for each rescue folder in `root` that holds
/// audio of the project in `project_dir`, for `status`: what it holds, how
/// to bring it back, when the folder may go, and the WAVs in it that have
/// no JSON file.
pub fn notices(root: &Path, project_dir: &Path) -> Vec<Problem> {
    rescued_for(root, project_dir)
        .iter()
        .map(|found| Problem::new(None, "rescued-audio", notice(found)))
        .collect()
}

/// What `status` says about the rescue folder `found`.
fn notice(found: &Found) -> String {
    let folder = found.folder.display();
    let orphans = found.without_json.len();
    let files = format!(
        "{orphans} WAV file{} without {} JSON file{} ({})",
        if orphans == 1 { "" } else { "s" },
        if orphans == 1 { "its" } else { "their" },
        if orphans == 1 { "" } else { "s" },
        found.without_json.join(", ")
    );
    // Nothing places such audio in the project, so nothing adopts it.
    let by_hand = "which could not be written, so `takewise status --repair` cannot bring that audio back: listen to it and keep what you need before deleting the folder";
    let Some(first) = found.audio.first() else {
        return format!(
            "audio that could not be saved into this project is kept in {folder} as {files}, {by_hand}"
        );
    };
    let copies = format!(
        "{} could not be saved into this project on {} and {} kept in {folder}; to bring one back, copy its WAV to the path in the project that the JSON file beside it gives as `restore.audio`, write that file's `sidecar` object to `restore.sidecar`, then run `takewise status --repair`; delete the folder once every recording in it is back in the project",
        describe(&found.audio),
        first.rescued_at.strftime("%Y-%m-%d"),
        if found.audio.len() == 1 { "is" } else { "are" },
    );
    if orphans == 0 {
        copies
    } else {
        format!("{copies}; the folder also holds {files}, {by_hand}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::passage::{Latency, PassageLine};
    use crate::project::{FilmInfo, Project};
    use crate::takes::{read_take_wav, repair};
    use crate::testutil::{RATE, burst, noise};

    const SCRIPT: &str = r#"
[[line]]
id = "a"
start = 10.0
end = 13.0
text = "One."

[[line]]
id = "b"
start = 13.5
end = 16.0
text = "Two."
"#;

    fn project(tmp: &tempfile::TempDir) -> PathBuf {
        let dir = tmp.path().join("film.takewise");
        let info = FilmInfo {
            duration_s: 30.0,
            size: 1,
            modified: "2026-11-22T20:05:11Z".to_owned(),
        };
        Project::init(&dir, Path::new("film.mp4"), info).unwrap();
        fs::write(dir.join("script.toml"), SCRIPT).unwrap();
        dir
    }

    fn at() -> jiff::Timestamp {
        "2026-11-24T10:15:30Z".parse().unwrap()
    }

    fn take_input(dir: &Path, line: &str) -> RecordedInput {
        RecordedInput {
            sample_rate: RATE,
            device: "Test Mic".to_owned(),
            channel: 1,
            line_offset_s: -2.0,
            snapshot: Project::open(dir).unwrap().snapshot(line).unwrap(),
            dropouts: Some(0),
            film_shift_ms: None,
            passage: None,
        }
    }

    fn passage_input(dir: &Path) -> PassageInput {
        let project = Project::open(dir).unwrap();
        PassageInput {
            sample_rate: RATE,
            device: "Test Mic".to_owned(),
            channel: 1,
            latency: Latency {
                input_s: 0.012,
                offset_ms: -29.0,
            },
            preroll_s: 3.0,
            lines: ["a", "b"]
                .iter()
                .map(|id| PassageLine {
                    id: (*id).to_owned(),
                    snapshot: project.snapshot(id).unwrap(),
                })
                .collect(),
            first_sample_film_s: Some(7.0),
            dropouts: None,
            film_shift_ms: None,
        }
    }

    /// Two sentences read in one go from film time 7 s on.
    fn spoken() -> Vec<i32> {
        let mut audio = noise(10.0);
        burst(&mut audio, 3.0, 5.8);
        burst(&mut audio, 6.6, 8.8);
        audio
    }

    /// Brings a rescue copy back as `docs/reference.md` says: the WAV and
    /// the `sidecar` object go where `restore` names them.
    fn restore_by_hand(dir: &Path, folder: &Path, rescued: &RescuedAudio) {
        let audio = dir.join(&rescued.restore.audio);
        fs::create_dir_all(audio.parent().unwrap()).unwrap();
        fs::copy(folder.join(&rescued.audio), &audio).unwrap();
        let sidecar = serde_json::to_vec_pretty(&rescued.sidecar).unwrap();
        fs::write(dir.join(&rescued.restore.sidecar), sidecar).unwrap();
    }

    #[test]
    fn a_take_and_a_passage_are_rescued_and_found_for_their_project() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = project(&tmp);
        let root = tmp.path().join("Takewise/rescue");
        let take: Vec<i32> = (0..4800).collect();
        let passage = spoken();

        crate::fsutil::dir_syncs::start();
        let mut rescue = Rescue::create(&root, &dir, at()).unwrap();
        let folder = rescue.folder().to_path_buf();
        assert_eq!(folder, root.join("film.takewise-20261124T101530Z"));
        let wav = rescue.add_take("a", &take_input(&dir, "a"), &take).unwrap();
        assert_eq!(wav, folder.join("001-take-a.wav"));
        let wav = rescue.add_passage(&passage_input(&dir), &passage).unwrap();
        assert_eq!(wav, folder.join("002-passage.wav"));
        assert_eq!(rescue.finish().unwrap(), folder);
        // Durable under its names: the folder, the rescue folder it is
        // in, and that one's own folder, since it was made just now.
        let synced = crate::fsutil::dir_syncs::take();
        let home = root.parent().unwrap().to_path_buf();
        assert_eq!(synced, [folder.clone(), root.clone(), home]);

        // Ordinary WAVs, sample for sample what was in memory.
        assert_eq!(
            read_take_wav(&folder.join("001-take-a.wav")).unwrap(),
            (take, RATE)
        );
        assert_eq!(
            read_take_wav(&folder.join("002-passage.wav")).unwrap(),
            (passage, RATE)
        );

        let found = rescued_for(&root, &dir);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].folder, folder);
        let [take, passage] = found[0].audio.as_slice() else {
            panic!("{found:?}");
        };
        assert_eq!(
            (take.kind, take.lines.as_slice()),
            (Kind::Take, &["a".to_owned()][..])
        );
        assert_eq!(take.samples, 4800);
        assert_eq!(take.sidecar["snapshot"]["text"], "One.");
        assert_eq!(take.sidecar["line_offset_s"], -2.0);
        assert_eq!(passage.kind, Kind::Passage);
        assert_eq!(passage.lines, ["a", "b"]);
        assert_eq!(passage.sidecar["first_sample_film_s"], 7.0);
        assert_eq!(passage.sidecar["lines"][1]["snapshot"]["start"], 13.5);
        assert_eq!(describe(&found[0].audio), "1 take (a) and 1 passage (a, b)");

        // Another project has none, and neither has a missing folder.
        let other = tmp.path().join("other.takewise");
        assert_eq!(rescued_for(&root, &other), []);
        assert_eq!(rescued_for(&tmp.path().join("none"), &dir), []);

        let notices = notices(&root, &dir);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].code, "rescued-audio");
        assert!(
            notices[0].message.starts_with(
                "1 take (a) and 1 passage (a, b) could not be saved into this project on 2026-11-24 and are kept in "
            ),
            "{}",
            notices[0].message
        );
        assert!(notices[0].message.contains(&folder.display().to_string()));
    }

    #[test]
    fn a_second_rescue_in_the_same_second_gets_its_own_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = project(&tmp);
        let root = tmp.path().join("rescue");
        let first = Rescue::create(&root, &dir, at()).unwrap();
        let second = Rescue::create(&root, &dir, at()).unwrap();
        assert_ne!(first.folder(), second.folder());
        assert_eq!(
            second.folder(),
            root.join("film.takewise-20261124T101530Z-2")
        );
    }

    #[test]
    fn a_project_whose_disk_is_gone_is_rescued_and_found_again() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = project(&tmp);
        let input = take_input(&dir, "a");
        let root = tmp.path().join("rescue");
        // The project's folder cannot be reached while its audio is
        // rescued, and is back later.
        let away = tmp.path().join("unmounted");
        fs::rename(&dir, &away).unwrap();
        let mut rescue = Rescue::create(&root, &dir, at()).unwrap();
        rescue.add_take("a", &input, &[1, 2, 3]).unwrap();
        rescue.finish().unwrap();
        fs::rename(&away, &dir).unwrap();
        assert_eq!(rescued_for(&root, &dir).len(), 1);
    }

    #[test]
    fn a_rescued_take_comes_back_through_repair() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = project(&tmp);
        let root = tmp.path().join("rescue");
        let mut audio = noise(5.0);
        burst(&mut audio, 2.0, 3.0);
        let mut rescue = Rescue::create(&root, &dir, at()).unwrap();
        rescue
            .add_take("a", &take_input(&dir, "a"), &audio)
            .unwrap();
        let folder = rescue.finish().unwrap();

        let found = rescued_for(&root, &dir);
        restore_by_hand(&dir, &folder, &found[0].audio[0]);
        let report = repair(&dir).unwrap();
        assert_eq!(report.adopted.len(), 1, "{report:?}");
        let ledger = crate::ledger::read(&dir).unwrap();
        let state = crate::state::fold(&Project::open(&dir).unwrap(), &ledger);
        let take = &state.lines[0].takes[0];
        assert_eq!((take.take, take.line_offset_s), (1, -2.0));
        assert_eq!(read_take_wav(&dir.join(&take.file)).unwrap().0, audio);
        // The rescue copy is still there: nothing removes it.
        assert!(folder.join("001-take-a.wav").is_file());
    }

    #[test]
    fn a_rescued_passage_comes_back_through_repair() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = project(&tmp);
        let root = tmp.path().join("rescue");
        let audio = spoken();
        let mut rescue = Rescue::create(&root, &dir, at()).unwrap();
        rescue.add_passage(&passage_input(&dir), &audio).unwrap();
        let folder = rescue.finish().unwrap();

        let found = rescued_for(&root, &dir);
        restore_by_hand(&dir, &folder, &found[0].audio[0]);
        let report = repair(&dir).unwrap();
        assert_eq!(report.passages_published.len(), 1, "{report:?}");
        assert_eq!(report.pieces.len(), 2, "{report:?}");
        let passage = crate::read_passage(&dir, &report.passages_published[0]).unwrap();
        assert_eq!(read_take_wav(&dir.join(passage.file())).unwrap().0, audio);
        let ledger = crate::ledger::read(&dir).unwrap();
        let state = crate::state::fold(&Project::open(&dir).unwrap(), &ledger);
        let takes: Vec<usize> = state.lines.iter().map(|line| line.takes.len()).collect();
        assert_eq!(takes, [1, 1]);
    }

    #[test]
    fn a_copy_that_cannot_be_written_leaves_no_json_without_audio() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = project(&tmp);
        let root = tmp.path().join("rescue");
        let mut rescue = Rescue::create(&root, &dir, at()).unwrap();
        crate::takes::fail_wav_writes_after(Some(100));
        let failed = rescue.add_take("a", &take_input(&dir, "a"), &vec![1; 4800]);
        crate::takes::fail_wav_writes_after(None);
        assert!(failed.is_err());
        assert_eq!(fs::read_dir(rescue.folder()).unwrap().count(), 0);
        assert_eq!(rescued_for(&root, &dir), []);
        // The next copy takes the number the failed one did not use.
        let wav = rescue
            .add_take("a", &take_input(&dir, "a"), &vec![1; 4800])
            .unwrap();
        assert!(wav.ends_with("001-take-a.wav"), "{}", wav.display());
        // A line id that is no file name is refused, not written.
        assert!(
            rescue
                .add_take("../x", &take_input(&dir, "a"), &[1])
                .is_err()
        );
    }

    #[test]
    fn a_copy_whose_json_cannot_be_written_keeps_its_audio() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = project(&tmp);
        let root = tmp.path().join("rescue");
        let mut rescue = Rescue::create(&root, &dir, at()).unwrap();
        let first: Vec<i32> = (0..4800).collect();
        let second: Vec<i32> = (0..2400).rev().collect();

        // The audio is written, then the disk fails for its JSON file.
        fail_json_writes(true);
        let failed = rescue.add_take("a", &take_input(&dir, "a"), &first);
        fail_json_writes(false);
        let kept = match failed {
            Err(Error::RescueJson { wav, .. }) => wav,
            other => panic!("{other:?}"),
        };
        assert_eq!(kept, rescue.folder().join("001-take-a.wav"));
        assert_eq!(read_take_wav(&kept).unwrap(), (first.clone(), RATE));

        // Another take of the same line gets a name of its own: the WAV
        // that has no JSON file is the only copy of its audio.
        let wav = rescue
            .add_take("a", &take_input(&dir, "a"), &second)
            .unwrap();
        assert_eq!(wav, rescue.folder().join("002-take-a.wav"));
        assert_eq!(read_take_wav(&kept).unwrap(), (first, RATE));
        assert_eq!(read_take_wav(&wav).unwrap(), (second, RATE));
        // Whatever has the next name, however it got there, stays too.
        let stray = rescue.folder().join("003-passage.wav");
        fs::write(&stray, b"not ours").unwrap();
        let wav = rescue.add_passage(&passage_input(&dir), &[1, 2]).unwrap();
        assert_eq!(wav, rescue.folder().join("004-passage.wav"));
        assert_eq!(fs::read(&stray).unwrap(), b"not ours");
        fs::remove_file(&stray).unwrap();
        fs::remove_file(&wav).unwrap();
        fs::remove_file(wav.with_extension("json")).unwrap();
        let folder = rescue.finish().unwrap();

        // `status` names the WAV that has no JSON file too.
        let found = rescued_for(&root, &dir);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].audio.len(), 1);
        assert_eq!(found[0].audio[0].audio, "002-take-a.wav");
        assert_eq!(found[0].without_json, ["001-take-a.wav"]);
        let notices = notices(&root, &dir);
        assert_eq!(notices.len(), 1);
        let message = &notices[0].message;
        assert!(
            message.starts_with("1 take (a) could not be saved into this project"),
            "{message}"
        );
        assert!(
            message.contains("also holds 1 WAV file without its JSON file (001-take-a.wav)"),
            "{message}"
        );
        assert!(message.contains(&folder.display().to_string()), "{message}");
    }

    #[test]
    fn audio_without_any_json_file_is_still_listed_for_its_project() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = project(&tmp);
        let root = tmp.path().join("rescue");
        let mut rescue = Rescue::create(&root, &dir, at()).unwrap();
        fail_json_writes(true);
        let failed = rescue.add_passage(&passage_input(&dir), &spoken());
        fail_json_writes(false);
        assert!(
            matches!(failed, Err(Error::RescueJson { .. })),
            "{failed:?}"
        );
        let folder = rescue.finish().unwrap();

        // Nothing in the folder says which project it is for but its
        // name, which is the project folder's.
        let found = rescued_for(&root, &dir);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].audio, []);
        assert_eq!(found[0].without_json, ["001-passage.wav"]);
        let notices = notices(&root, &dir);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].code, "rescued-audio");
        let message = &notices[0].message;
        assert!(
            message.starts_with("audio that could not be saved into this project is kept in ")
                && message.contains(&folder.display().to_string())
                && message.contains("as 1 WAV file without its JSON file (001-passage.wav)"),
            "{message}"
        );
        // A project of another name does not list it, nor one whose name
        // only begins the same.
        assert_eq!(rescued_for(&root, &tmp.path().join("film")), []);
        assert_eq!(rescued_for(&root, &tmp.path().join("other.takewise")), []);
        // Nor is a folder of another project of the same name, whose
        // copies say where they belong, taken for this one's.
        let elsewhere = tmp.path().join("elsewhere/film.takewise");
        fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        let mut theirs = Rescue::create(&root, &elsewhere, at()).unwrap();
        theirs
            .add_take("a", &take_input(&dir, "a"), &[1, 2, 3])
            .unwrap();
        fail_json_writes(true);
        let failed = theirs.add_take("a", &take_input(&dir, "a"), &[4, 5, 6]);
        fail_json_writes(false);
        assert!(failed.is_err());
        let theirs = theirs.finish().unwrap();
        let found = rescued_for(&root, &dir);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].folder, folder);
        // That project sees its own folder, and the one without any JSON
        // file as well: its name fits both projects, and audio listed
        // twice is better than audio nobody is told of.
        let found = rescued_for(&root, &elsewhere);
        let folders: Vec<&Path> = found.iter().map(|found| found.folder.as_path()).collect();
        assert_eq!(folders, [folder.as_path(), theirs.as_path()]);
        assert_eq!(found[1].without_json, ["002-take-a.wav"]);
    }

    #[test]
    fn a_long_line_id_gets_a_file_name_that_fits() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = project(&tmp);
        let root = tmp.path().join("rescue");
        // Valid in a project: `takes/<line>/` is a folder name of 250 bytes.
        let long = format!("{}-end", "a".repeat(246));
        let other = format!("{}-fin", "a".repeat(246));
        assert!(is_valid_id(&long) && long.len() == 250);
        let mut rescue = Rescue::create(&root, &dir, at()).unwrap();
        let wav = rescue
            .add_take(&long, &take_input(&dir, "a"), &[1, 2, 3])
            .unwrap();
        let name = wav.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.len() <= 100, "{name}");
        assert!(name.starts_with("001-take-aaaaaaaa"), "{name}");
        assert_eq!(read_take_wav(&wav).unwrap().0, [1, 2, 3]);
        // Another line that begins the same is told apart by name.
        let wav = rescue
            .add_take(&other, &take_input(&dir, "a"), &[4, 5, 6])
            .unwrap();
        let second = wav.file_name().unwrap().to_string_lossy().into_owned();
        assert_ne!(name[3..], second[3..], "after the number");
        // A short id is kept as it is.
        let wav = rescue.add_take("b", &take_input(&dir, "b"), &[7]).unwrap();
        assert!(wav.ends_with("003-take-b.wav"), "{}", wav.display());
        rescue.finish().unwrap();

        // The whole id is in the JSON file, and in the paths to restore to.
        let found = rescued_for(&root, &dir);
        let copy = &found[0].audio[0];
        assert_eq!(copy.lines, [long.as_str()]);
        assert!(
            copy.restore.audio.starts_with(&format!("takes/{long}/.")),
            "{}",
            copy.restore.audio
        );
        assert_eq!(found[0].audio[1].lines, [other]);
    }

    #[test]
    fn a_long_project_folder_name_gets_a_rescue_folder_that_fits() {
        let tmp = tempfile::tempdir().unwrap();
        let input = take_input(&project(&tmp), "a");
        let root = tmp.path().join("rescue");
        // As long as a folder name may be: the timestamp no longer fits.
        let long = tmp.path().join(format!("{}.takewise", "p".repeat(240)));
        assert_eq!(long.file_name().unwrap().len(), 249);
        let mut rescue = Rescue::create(&root, &long, at()).unwrap();
        let name = rescue.folder().file_name().unwrap().to_string_lossy();
        assert!(name.len() <= 220, "{name}");
        assert!(name.ends_with("-20261124T101530Z"), "{name}");
        rescue.add_take("a", &input, &[1, 2, 3]).unwrap();
        // Found by what its JSON file says, and by its name when there is
        // none.
        fail_json_writes(true);
        let failed = rescue.add_take("a", &input, &[4, 5, 6]);
        fail_json_writes(false);
        assert!(failed.is_err());
        let folder = rescue.finish().unwrap();
        let found = rescued_for(&root, &long);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].audio[0].project, long.display().to_string());
        fs::remove_file(folder.join("001-take-a.json")).unwrap();
        let found = rescued_for(&root, &long);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].without_json.len(), 2);

        // A name that is not only ASCII is cut between two characters.
        let name = format!("{}{}", "p".repeat(190), "ü".repeat(30));
        let short = shortened(&name, NAME_PROJECT_BYTES);
        assert!(short.len() <= NAME_PROJECT_BYTES, "{short}");
        assert!(
            short.starts_with(&format!("{}-", "p".repeat(190))),
            "{short}"
        );
        assert_eq!(
            shortened("film.takewise", NAME_PROJECT_BYTES),
            "film.takewise"
        );
    }
}
