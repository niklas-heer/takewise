//! Room tone: the five seconds of silence the setup check records, kept in
//! `takes/.room/<timestamp>.wav` with a sidecar `<timestamp>.json` naming
//! the input it was recorded on and its noise floor, for `clean` export's
//! noise reduction (see `docs/design.md`, "Setup check").

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, io};
use crate::fsutil::{rename_temp, write_atomic, write_temp};
use crate::project::TAKES_DIR;
use crate::takes::encode_wav;

/// The room tone directory, relative to the project.
pub const ROOM_DIR: &str = ".room";

/// A room tone's sidecar, `takes/.room/<timestamp>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomTone {
    /// The WAV, relative to the project, such as
    /// `takes/.room/20261123T101530Z.wav`.
    pub file: String,
    pub recorded_at: jiff::Timestamp,
    pub input: String,
    pub channel: u16,
    pub output: String,
    pub sample_rate: u32,
    pub samples: u64,
    /// As a take's pre-roll floor is measured: the 20th percentile of
    /// 10 ms frame levels.
    pub floor_dbfs: f64,
    /// Digital silence: a gated interface, useless for noise reduction.
    pub gated: bool,
}

/// What a new room tone was recorded on.
#[derive(Debug, Clone, PartialEq)]
pub struct RoomInput<'a> {
    pub input: &'a str,
    pub channel: u16,
    pub output: &'a str,
    pub floor_dbfs: f64,
    pub gated: bool,
}

/// Saves a room tone recorded at `at`: the WAV first, then its sidecar, so
/// a sidecar always has its audio.
pub fn save_room_tone(
    dir: &Path,
    samples: &[i32],
    rate: u32,
    at: jiff::Timestamp,
    input: &RoomInput<'_>,
) -> Result<RoomTone> {
    let room = dir.join(TAKES_DIR).join(ROOM_DIR);
    fs::create_dir_all(&room).map_err(io(&room))?;
    let name = free_name(&room, at);
    let wav = room.join(format!("{name}.wav"));
    let temp = write_temp(&wav, |file| {
        let copy = file.try_clone()?;
        encode_wav(copy, samples, rate)
            .map(drop)
            .map_err(std::io::Error::other)
    })?;
    rename_temp(&temp, &wav)?;
    let tone = RoomTone {
        file: format!("{TAKES_DIR}/{ROOM_DIR}/{name}.wav"),
        recorded_at: at,
        input: input.input.to_owned(),
        channel: input.channel,
        output: input.output.to_owned(),
        sample_rate: rate,
        samples: samples.len() as u64,
        floor_dbfs: input.floor_dbfs,
        gated: input.gated,
    };
    let json = room.join(format!("{name}.json"));
    let text = serde_json::to_vec_pretty(&tone).map_err(|err| Error::Parse {
        path: json.clone(),
        message: err.to_string(),
    })?;
    write_atomic(&json, &text)?;
    Ok(tone)
}

/// A name for a room tone recorded at `at` that `room` does not hold yet:
/// its UTC timestamp, with a counter when that is taken.
fn free_name(room: &Path, at: jiff::Timestamp) -> String {
    let stamp = at.strftime("%Y%m%dT%H%M%SZ").to_string();
    (0..1000)
        .map(|n| match n {
            0 => stamp.clone(),
            n => format!("{stamp}-{n}"),
        })
        .find(|name| {
            !room.join(format!("{name}.json")).exists()
                && !room.join(format!("{name}.wav")).exists()
        })
        .unwrap_or(stamp)
}

/// Copies the room tone `tone`, whose WAV is `wav`, into the folder
/// `into`: the WAV first, then its sidecar, whose `file` becomes
/// `<prefix>/<name>.wav`. Returns the copy's sidecar.
pub fn copy_room_tone(wav: &Path, tone: &RoomTone, into: &Path, prefix: &str) -> Result<RoomTone> {
    fs::create_dir_all(into).map_err(io(into))?;
    let name = free_name(into, tone.recorded_at);
    let target = into.join(format!("{name}.wav"));
    let temp = write_temp(&target, |file| {
        let mut source = fs::File::open(wav)?;
        std::io::copy(&mut source, file).map(drop)
    })?;
    rename_temp(&temp, &target)?;
    let copy = RoomTone {
        file: format!("{prefix}/{name}.wav"),
        ..tone.clone()
    };
    let json = into.join(format!("{name}.json"));
    let text = serde_json::to_vec_pretty(&copy).map_err(|err| Error::Parse {
        path: json.clone(),
        message: err.to_string(),
    })?;
    write_atomic(&json, &text)?;
    Ok(copy)
}

/// Copies a room tone recorded elsewhere (its WAV is `wav`) into the
/// project's `takes/.room/`, unless the project already holds it (the same
/// recording time and devices). Returns the project's sidecar of it.
pub fn import_room_tone(dir: &Path, wav: &Path, tone: &RoomTone) -> Result<RoomTone> {
    let same = |held: &RoomTone| {
        held.recorded_at == tone.recorded_at
            && held.input == tone.input
            && held.channel == tone.channel
            && held.output == tone.output
    };
    if let Some(held) = room_tones(dir)?.into_iter().find(same) {
        return Ok(held);
    }
    let room = dir.join(TAKES_DIR).join(ROOM_DIR);
    copy_room_tone(wav, tone, &room, &format!("{TAKES_DIR}/{ROOM_DIR}"))
}

/// Every room tone whose sidecar parses and whose WAV exists, oldest first.
pub fn room_tones(dir: &Path) -> Result<Vec<RoomTone>> {
    let room = dir.join(TAKES_DIR).join(ROOM_DIR);
    let entries = match fs::read_dir(&room) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(io(&room)(err)),
    };
    let mut tones: Vec<RoomTone> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "json")
                && !path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .filter_map(|path| serde_json::from_slice::<RoomTone>(&fs::read(&path).ok()?).ok())
        .filter(|tone| dir.join(&tone.file).is_file())
        .collect();
    tones.sort_by_key(|tone| tone.recorded_at);
    Ok(tones)
}

/// The WAV of a room tone.
pub fn room_tone_path(dir: &Path, tone: &RoomTone) -> PathBuf {
    dir.join(&tone.file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::takes::read_take_wav;
    use crate::testutil::project_tempdir;

    fn input(floor: f64) -> RoomInput<'static> {
        RoomInput {
            input: "Duo",
            channel: 1,
            output: "Duo",
            floor_dbfs: floor,
            gated: false,
        }
    }

    #[test]
    fn saves_and_lists_room_tones() {
        let tmp = project_tempdir();
        let dir = tmp.path();
        assert_eq!(room_tones(dir).unwrap(), []);
        let at: jiff::Timestamp = "2026-11-23T10:15:30Z".parse().unwrap();
        let first = save_room_tone(dir, &[1, -2, 3], 48_000, at, &input(-68.0)).unwrap();
        assert_eq!(first.file, "takes/.room/20261123T101530Z.wav");
        // The same second again gets its own name.
        let second = save_room_tone(dir, &[4; 10], 48_000, at, &input(-60.0)).unwrap();
        assert_eq!(second.file, "takes/.room/20261123T101530Z-1.wav");
        let later: jiff::Timestamp = "2026-11-23T11:00:00Z".parse().unwrap();
        let third = save_room_tone(dir, &[5; 10], 48_000, later, &input(-61.0)).unwrap();

        let tones = room_tones(dir).unwrap();
        assert_eq!(tones.len(), 3);
        assert_eq!(tones[2], third);
        assert!(tones.contains(&first) && tones.contains(&second));
        let (samples, rate) = read_take_wav(&room_tone_path(dir, &first)).unwrap();
        assert_eq!((samples, rate), (vec![1, -2, 3], 48_000));

        // A sidecar without its audio is not a room tone.
        fs::remove_file(room_tone_path(dir, &third)).unwrap();
        assert_eq!(room_tones(dir).unwrap().len(), 2);
    }
}
