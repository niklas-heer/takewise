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
//! Nothing here writes into a project, and nothing removes a rescue copy.
//! Bringing one back is done by hand (see `docs/reference.md`, "Rescued
//! audio"): the JSON file names the two files to create in the project,
//! after which `takewise status --repair` adopts the audio as it adopts a
//! take a crash left behind. `status` says when a project has rescue
//! copies ([`notices`]).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    /// How many copies the folder holds.
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
        let name = project_dir.file_name().map_or_else(
            || "project".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
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
            &format!("take-{line}"),
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
        let stem = format!("{:03}-{what}", self.count + 1);
        let wav = self.folder.join(format!("{stem}.wav"));
        let json = self.folder.join(format!("{stem}.json"));
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
        write_atomic(&json, &text)?;
        self.count += 1;
        Ok(wav)
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
}

/// The rescue folders in `root` with audio of the project in
/// `project_dir`, oldest first. A missing `root` holds none; files that do
/// not read as a rescue copy's are skipped.
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
                .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                .collect();
            files.sort();
            let audio: Vec<RescuedAudio> = files
                .iter()
                .filter_map(|path| serde_json::from_slice(&fs::read(path).ok()?).ok())
                .filter(|rescued: &RescuedAudio| {
                    ours(rescued) && folder.join(&rescued.audio).is_file()
                })
                .collect();
            (!audio.is_empty()).then_some(Found { folder, audio })
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
/// audio of the project in `project_dir`, for `status`.
pub fn notices(root: &Path, project_dir: &Path) -> Vec<Problem> {
    rescued_for(root, project_dir)
        .iter()
        .map(|found| {
            let day = found.audio.first().map_or_else(String::new, |rescued| {
                format!(" on {}", rescued.rescued_at.strftime("%Y-%m-%d"))
            });
            Problem::new(
                None,
                "rescued-audio",
                format!(
                    "{} could not be saved into this project{day} and {} kept in {}; to bring one back, copy its WAV to the path in the project that the JSON file beside it gives as `restore.audio`, write that file's `sidecar` object to `restore.sidecar`, then run `takewise status --repair`",
                    describe(&found.audio),
                    if found.audio.len() == 1 { "is" } else { "are" },
                    found.folder.display()
                ),
            )
        })
        .collect()
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
}
