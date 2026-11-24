//! The remembered setup: latency measurements and setup checks kept across
//! projects, so a new project on the same devices need not measure again
//! (see `docs/design.md`, "Setup and sound").
//!
//! Everything lives in `remembered.json` beside the settings file, with
//! copies of the setup checks' room tones in `rooms/` next to it. The file
//! is changed only by one locked read-modify-write ([`update`]), never
//! through `settings.toml`, which `save_defaults_to` rewrites whole.
//!
//! - **Latency** is remembered per exact route (input, output and capture
//!   buffer frames), newest first, and copied into a project that has no
//!   calibration fitting the route in use, with `from = "remembered"`.
//! - **The setup check** is remembered per exact input, channel and output
//!   with a copy of its room tone, for the day it ran: a project on that
//!   route without a setup check from today adopts it, room tone and all,
//!   so `clean` and the setup offer keep working from the project's files.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::edit::{set_calibration_when, set_setup};
use crate::error::{Error, Result, io};
use crate::fsutil::{sync_dir, sync_parent, write_atomic};
use crate::project::{CalibrationInfo, ProjectConfig, REMEMBERED, SetupInfo};
use crate::room::{RoomTone, copy_room_tone, import_room_tone};
use crate::route::{Route, calibration_stale, settings_path};

/// The remembered setup's file, beside the settings file.
pub const REMEMBERED_FILE: &str = "remembered.json";
/// The folder of the remembered room tones, beside [`REMEMBERED_FILE`].
pub const ROOMS_DIR: &str = "rooms";
/// The lock every change of [`REMEMBERED_FILE`] and [`ROOMS_DIR`] holds.
const LOCK_FILE: &str = ".remembered.lock";

/// `remembered.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Remembered {
    /// One measurement per exact route, the newest.
    pub latency: Vec<RememberedLatency>,
    /// One setup check per exact input, channel and output, from the day
    /// of the last one; earlier days are pruned.
    pub setup: Vec<RememberedSetup>,
}

/// A latency measurement and the exact route it was measured on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RememberedLatency {
    pub input: String,
    pub output: String,
    pub buffer_frames: u32,
    pub latency_offset_ms: i32,
    /// `YYYY-MM-DD`, the local day it was measured.
    pub date: String,
}

impl RememberedLatency {
    /// The measurement of a stored `[calibration]`, if it records its
    /// buffer size (else its route is not exact).
    pub fn of(offset_ms: i32, info: &CalibrationInfo) -> Option<Self> {
        Some(Self {
            input: info.input.clone(),
            output: info.output.clone(),
            buffer_frames: info.buffer_frames?,
            latency_offset_ms: offset_ms,
            date: info.date.clone(),
        })
    }

    /// Whether it was measured on exactly `route`; an unknown part of
    /// `route` never matches.
    pub fn fits(&self, route: &Route) -> bool {
        route.input.as_deref() == Some(self.input.as_str())
            && route.output.as_deref() == Some(self.output.as_str())
            && route.buffer_frames == Some(self.buffer_frames)
    }

    fn same_route(&self, other: &Self) -> bool {
        (&self.input, &self.output, self.buffer_frames)
            == (&other.input, &other.output, other.buffer_frames)
    }

    /// The `[calibration]` a project gets when it copies this measurement.
    pub fn calibration(&self) -> CalibrationInfo {
        CalibrationInfo {
            input: self.input.clone(),
            output: self.output.clone(),
            buffer_frames: Some(self.buffer_frames),
            date: self.date.clone(),
            from: Some(REMEMBERED.to_owned()),
        }
    }
}

/// A setup check's result and its room tone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RememberedSetup {
    pub setup: SetupInfo,
    /// The room tone's sidecar; its `file` is relative to the folder of
    /// [`REMEMBERED_FILE`], such as `rooms/20261124T101530Z.wav`.
    pub room_tone: RoomTone,
}

impl RememberedSetup {
    fn on(&self, input: &str, channel: u16, output: &str) -> bool {
        self.setup.input == input && self.setup.channel == channel && self.setup.output == output
    }
}

/// `remembered.json` beside the settings file ([`settings_path`], which
/// `TAKEWISE_SETTINGS` overrides), if there is a home.
pub fn remembered_path() -> Option<PathBuf> {
    settings_path().map(|settings| beside(&settings))
}

/// `remembered.json` beside the settings file `settings`.
pub fn beside(settings: &Path) -> PathBuf {
    settings.with_file_name(REMEMBERED_FILE)
}

/// Reads `path`; a missing file remembers nothing.
pub fn read(path: &Path) -> Result<Remembered> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Remembered::default());
        }
        Err(err) => return Err(io(path)(err)),
    };
    serde_json::from_slice(&bytes).map_err(|err| Error::Parse {
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

/// The one way `path` (and the room tones beside it) change: under the
/// lock, reads it, lets `change` edit it with the folder it lives in, and
/// writes it back atomically when it changed.
fn update<T>(path: &Path, change: impl FnOnce(&mut Remembered, &Path) -> Result<T>) -> Result<T> {
    update_then(path, change, |_, _| {})
}

#[cfg(test)]
thread_local! {
    static WRITES_FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// [`update`], then `then` with what is now on disk and its folder, still
/// under the lock: for what may only happen once the file is committed,
/// such as removing the files it no longer refers to. When the file cannot
/// be written, or its folder not synced after it, `then` does not run, and
/// everything the file on disk refers to is still there.
fn update_then<T>(
    path: &Path,
    change: impl FnOnce(&mut Remembered, &Path) -> Result<T>,
    then: impl FnOnce(&Remembered, &Path),
) -> Result<T> {
    #[cfg(test)]
    assert!(
        path.starts_with(std::env::temp_dir())
            || fs::canonicalize(std::env::temp_dir()).is_ok_and(|tmp| path.starts_with(tmp)),
        "a test remembered a setup outside the temporary folder: {}",
        path.display()
    );
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(io(dir))?;
    let lock_path = dir.join(LOCK_FILE);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(io(&lock_path))?;
    let _lock = crate::fsutil::LockedFile::lock(lock).map_err(io(&lock_path))?;
    let before = read(path)?;
    let mut remembered = before.clone();
    let out = change(&mut remembered, dir)?;
    if remembered != before {
        let text = serde_json::to_vec_pretty(&remembered).map_err(|err| Error::Parse {
            path: path.to_path_buf(),
            message: err.to_string(),
        })?;
        #[cfg(test)]
        if WRITES_FAIL.get() {
            let failed = std::io::Error::other("the store could not be written (test)");
            return Err(io(path)(failed));
        }
        write_atomic(path, &text)?;
        // The rename is durable before anything it replaced is removed: a
        // power loss could otherwise bring back the old file, naming
        // files `then` had already deleted.
        sync_parent(path)?;
    }
    then(&remembered, dir);
    Ok(out)
}

/// Remembers a measurement just stored with `takewise calibrate --write`
/// or the setup check: it replaces whatever was remembered for its route.
/// A measurement without its buffer size is not remembered (`false`).
pub fn remember_latency(path: &Path, offset_ms: i32, info: &CalibrationInfo) -> Result<bool> {
    let Some(measured) = RememberedLatency::of(offset_ms, info) else {
        return Ok(false);
    };
    update(path, |remembered, _| {
        remembered
            .latency
            .retain(|known| !known.same_route(&measured));
        remembered.latency.push(measured);
        Ok(true)
    })
}

/// What [`sync_latency`] did to the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LatencySync {
    /// Nothing: its calibration fits the route, or nothing remembered does.
    Unchanged,
    /// Copied this remembered measurement into `[calibration]`.
    Applied(RememberedLatency),
}

/// For a project opened, or whose route changed, on the live `route`
/// (devices and buffer as resolved now):
///
/// 1. its own measurement (a `[calibration]` not copied from here, with its
///    buffer size) is remembered for its route when nothing is, or what is
///    remembered there is older;
/// 2. when its calibration does not fit `route` and a measurement for
///    exactly `route` is remembered, that one is copied into
///    `[input] latency_offset_ms` and `[calibration]` with
///    `from = "remembered"`. A project's own fitting measurement always
///    wins, and an incomplete route matches nothing;
/// 3. a calibration that was itself copied does not fit once a newer
///    measurement is remembered for the route (a later day, or another
///    offset from the same day): the newer one is copied in its place, so
///    measuring again in one project reaches the others.
///
/// `config` is the project as the caller read it, which may be a while
/// ago (the window's copy). Whether the project's calibration gives way is
/// decided again from the file, under the project lock that writes the
/// copy: a measurement stored in the project since is never replaced.
pub fn sync_latency(
    path: &Path,
    dir: &Path,
    config: &ProjectConfig,
    route: &Route,
) -> Result<LatencySync> {
    let own = config
        .calibration
        .as_ref()
        .filter(|cal| !cal.is_remembered())
        .and_then(|cal| RememberedLatency::of(config.input.latency_offset_ms, cal));
    let found = update(path, |remembered, _| {
        if let Some(own) = own {
            match remembered.latency.iter_mut().find(|k| k.same_route(&own)) {
                Some(known) if known.date < own.date => *known = own,
                Some(_) => {}
                None => remembered.latency.push(own),
            }
        }
        Ok(remembered.latency.iter().find(|k| k.fits(route)).cloned())
    })?;
    let Some(remembered) = found else {
        return Ok(LatencySync::Unchanged);
    };
    let applied = set_calibration_when(dir, |on_disk| {
        let fits = on_disk.calibration.is_some() && calibration_stale(on_disk, route).is_none();
        (!fits || superseded(on_disk, &remembered))
            .then(|| (remembered.latency_offset_ms, remembered.calibration()))
    })?;
    Ok(if applied {
        LatencySync::Applied(remembered)
    } else {
        LatencySync::Unchanged
    })
}

/// Whether the project's calibration is a copy that `remembered`, the
/// measurement now remembered for its route, replaces: one from a later
/// day, or from the same day with another offset. A measurement made in
/// the project itself is never superseded.
fn superseded(config: &ProjectConfig, remembered: &RememberedLatency) -> bool {
    let Some(cal) = config
        .calibration
        .as_ref()
        .filter(|cal| cal.is_remembered())
    else {
        return false;
    };
    let copy = (cal.date.as_str(), config.input.latency_offset_ms);
    let now = (remembered.date.as_str(), remembered.latency_offset_ms);
    remembered.date.as_str() >= cal.date.as_str() && copy != now
}

/// What the window and `status` say about a latency offset copied from an
/// earlier measurement, while it is the one in use on `route`: only on a
/// route known to be exactly the one it was measured on (see
/// [`calibration_stale`]).
pub fn latency_note(config: &ProjectConfig, route: &Route) -> Option<String> {
    let cal = config
        .calibration
        .as_ref()
        .filter(|cal| cal.is_remembered())?;
    calibration_stale(config, route).is_none().then(|| {
        format!(
            "latency from an earlier measurement on these devices ({})",
            cal.date
        )
    })
}

/// Remembers a setup check's result, saved in the project `dir` with the
/// room tone `tone`, for its exact input, channel and output, with a copy
/// of the room tone in `rooms/`. Results and room tones from days before
/// `today` (the local day) are pruned: the room tones only after
/// `remembered.json` is committed without them, so a failed write leaves
/// every room tone the file on disk still names. Committed means durable:
/// the new room tone and its folder are synced before the file names it,
/// and the file's folder is synced before an older room tone is removed,
/// so no power loss leaves a `remembered.json` whose room tone is gone.
pub fn remember_setup(
    path: &Path,
    dir: &Path,
    info: &SetupInfo,
    tone: &RoomTone,
    today: jiff::civil::Date,
) -> Result<()> {
    let today = today.to_string();
    update_then(
        path,
        |remembered, home| {
            let rooms = home.join(ROOMS_DIR);
            let new_folder = !rooms.is_dir();
            let copy = copy_room_tone(&dir.join(&tone.file), tone, &rooms, ROOMS_DIR)?;
            // The copy is on disk under its names before the store names
            // it: its files are synced as they are written, their folder
            // here (and a folder made just now, in its own).
            sync_dir(&rooms)?;
            if new_folder {
                sync_dir(home)?;
            }
            remembered.setup.retain(|known| {
                known.setup.date == today && !known.on(&info.input, info.channel, &info.output)
            });
            remembered.setup.push(RememberedSetup {
                setup: SetupInfo {
                    from: None,
                    ..info.clone()
                },
                room_tone: copy,
            });
            Ok(())
        },
        |remembered, home| prune_rooms(&home.join(ROOMS_DIR), &remembered.setup),
    )
}

/// Removes every file in `rooms` that no remembered setup refers to: the
/// room tones of earlier days and of results replaced since.
fn prune_rooms(rooms: &Path, kept: &[RememberedSetup]) {
    let stems: Vec<&str> = kept
        .iter()
        .filter_map(|known| Path::new(&known.room_tone.file).file_stem()?.to_str())
        .collect();
    let Ok(entries) = fs::read_dir(rooms) else {
        return;
    };
    for path in entries.filter_map(|entry| entry.ok().map(|e| e.path())) {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if path.is_file() && !stems.contains(&stem) {
            let _ = fs::remove_file(&path);
        }
    }
}

/// For a project on the live `route` without a setup check of its own for
/// it from `today` (the local day): adopts the setup check remembered for
/// exactly this input, channel and output from today, if any. Its room
/// tone is copied into the project's `takes/.room/` first, then `[setup]`
/// is written with `from = "remembered"`; returns what was written.
pub fn adopt_setup(
    path: &Path,
    dir: &Path,
    config: &ProjectConfig,
    route: &Route,
    today: jiff::civil::Date,
) -> Result<Option<SetupInfo>> {
    let today = today.to_string();
    let (Some(input), Some(output)) = (route.input.as_deref(), route.output.as_deref()) else {
        return Ok(None);
    };
    let has_own = config.setup.as_ref().is_some_and(|setup| {
        setup.date == today
            && setup.input == input
            && setup.channel == route.channel
            && setup.output == output
    });
    if has_own || !path.is_file() {
        return Ok(None);
    }
    let adopted = update(path, |remembered, home| {
        let Some(known) = remembered
            .setup
            .iter()
            .find(|known| known.setup.date == today && known.on(input, route.channel, output))
        else {
            return Ok(None);
        };
        let wav = home.join(&known.room_tone.file);
        import_room_tone(dir, &wav, &known.room_tone)?;
        Ok(Some(SetupInfo {
            from: Some(REMEMBERED.to_owned()),
            ..known.setup.clone()
        }))
    })?;
    if let Some(info) = &adopted {
        set_setup(dir, info)?;
    }
    Ok(adopted)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::edit::set_calibration;
    use crate::project::{FilmInfo, Project, read_config};
    use crate::room::{RoomInput, room_tones, save_room_tone};
    use crate::takes::read_take_wav;

    const DUO: &str = "RODECaster Duo Main Stereo";
    const XM5: &str = "WH-1000XM5";

    struct Home {
        _tmp: tempfile::TempDir,
        settings: PathBuf,
        remembered: PathBuf,
    }

    /// A settings folder with a `settings.toml` in it.
    fn home() -> Home {
        let tmp = tempfile::tempdir().unwrap();
        let settings = tmp.path().join("Takewise/settings.toml");
        crate::route::save_defaults_to(
            &settings,
            &Route {
                input: Some(DUO.to_owned()),
                channel: 1,
                output: Some(XM5.to_owned()),
                buffer_frames: None,
            },
        )
        .unwrap();
        let remembered = beside(&settings);
        Home {
            _tmp: tmp,
            settings,
            remembered,
        }
    }

    fn project(tmp: &tempfile::TempDir, name: &str) -> PathBuf {
        let dir = tmp.path().join(name);
        let info = FilmInfo {
            duration_s: 10.0,
            size: 1,
            modified: "2026-11-23T00:00:00Z".to_owned(),
        };
        Project::init(&dir, Path::new("film.mp4"), info).unwrap();
        crate::edit::set_route(&dir, Some((DUO, 1)), Some(XM5)).unwrap();
        dir
    }

    fn route(buffer: u32) -> Route {
        Route {
            input: Some(DUO.to_owned()),
            channel: 1,
            output: Some(XM5.to_owned()),
            buffer_frames: Some(buffer),
        }
    }

    fn measured(date: &str) -> CalibrationInfo {
        CalibrationInfo {
            input: DUO.to_owned(),
            output: XM5.to_owned(),
            buffer_frames: Some(512),
            date: date.to_owned(),
            from: None,
        }
    }

    fn day(text: &str) -> jiff::civil::Date {
        text.parse().unwrap()
    }

    #[test]
    fn a_measurement_applies_on_exactly_its_route() {
        let home = home();
        let tmp = tempfile::tempdir().unwrap();
        assert!(remember_latency(&home.remembered, -29, &measured("2026-11-23")).unwrap());

        // Another buffer size: nothing is applied.
        let other = project(&tmp, "other.takewise");
        let config = read_config(&other).unwrap();
        assert_eq!(
            sync_latency(&home.remembered, &other, &config, &route(256)).unwrap(),
            LatencySync::Unchanged
        );
        assert_eq!(read_config(&other).unwrap().calibration, None);
        // Nor on an incomplete route.
        let unknown = Route {
            buffer_frames: None,
            ..route(512)
        };
        assert_eq!(
            sync_latency(&home.remembered, &other, &config, &unknown).unwrap(),
            LatencySync::Unchanged
        );

        // The exact route: copied in, marked, with the measurement's date.
        let dir = project(&tmp, "new.takewise");
        let config = read_config(&dir).unwrap();
        let synced = sync_latency(&home.remembered, &dir, &config, &route(512)).unwrap();
        assert!(matches!(synced, LatencySync::Applied(ref l) if l.latency_offset_ms == -29));
        let config = read_config(&dir).unwrap();
        assert_eq!(config.input.latency_offset_ms, -29);
        let cal = config.calibration.clone().unwrap();
        assert!(cal.is_remembered());
        assert_eq!(cal.date, "2026-11-23");
        assert_eq!(cal.buffer_frames, Some(512));
        assert_eq!(
            crate::route::effective_latency_offset(&config, &route(512)),
            (-29, false)
        );
        assert_eq!(
            latency_note(&config, &route(512)).as_deref(),
            Some("latency from an earlier measurement on these devices (2026-11-23)")
        );
        assert_eq!(latency_note(&config, &route(256)), None);
        // Applying again changes nothing.
        assert_eq!(
            sync_latency(&home.remembered, &dir, &config, &route(512)).unwrap(),
            LatencySync::Unchanged
        );
    }

    #[test]
    fn a_copied_offset_is_not_active_on_a_route_that_became_unknown() {
        let home = home();
        let tmp = tempfile::tempdir().unwrap();
        remember_latency(&home.remembered, -29, &measured("2026-11-23")).unwrap();
        let dir = project(&tmp, "new.takewise");
        let config = read_config(&dir).unwrap();
        sync_latency(&home.remembered, &dir, &config, &route(512)).unwrap();
        let config = read_config(&dir).unwrap();
        let offset = |route: &Route| crate::route::effective_latency_offset(&config, route);
        assert_eq!(offset(&route(512)), (-29, false));

        // The interface is unplugged, or the output cannot be read: the
        // copied offset is neither used nor said to be in use.
        let no_buffer = Route {
            buffer_frames: None,
            ..route(512)
        };
        let no_output = Route {
            output: None,
            ..route(512)
        };
        for unresolved in [&no_buffer, &no_output] {
            assert_eq!(offset(unresolved), (0, true), "{unresolved:?}");
            assert_eq!(latency_note(&config, unresolved), None, "{unresolved:?}");
            assert!(calibration_stale(&config, unresolved).is_some());
            // Nothing is remembered for a route that is not whole.
            assert_eq!(
                sync_latency(&home.remembered, &dir, &config, unresolved).unwrap(),
                LatencySync::Unchanged
            );
        }
        assert_eq!(read_config(&dir).unwrap(), config, "kept for later");
        // Known again: in use again.
        assert_eq!(offset(&route(512)), (-29, false));
        assert!(latency_note(&config, &route(512)).is_some());
    }

    #[test]
    fn a_copied_offset_follows_a_newer_measurement() {
        let home = home();
        let tmp = tempfile::tempdir().unwrap();
        remember_latency(&home.remembered, -29, &measured("2026-11-23")).unwrap();
        let dir = project(&tmp, "new.takewise");
        let sync = |dir: &Path| {
            let config = read_config(dir).unwrap();
            sync_latency(&home.remembered, dir, &config, &route(512)).unwrap()
        };
        assert!(matches!(sync(&dir), LatencySync::Applied(_)));
        assert_eq!(sync(&dir), LatencySync::Unchanged);

        // Measured again in another project the next day: the copy follows.
        remember_latency(&home.remembered, -31, &measured("2026-11-24")).unwrap();
        let synced = sync(&dir);
        assert!(matches!(synced, LatencySync::Applied(ref l) if l.latency_offset_ms == -31));
        let config = read_config(&dir).unwrap();
        assert_eq!(config.input.latency_offset_ms, -31);
        let cal = config.calibration.unwrap();
        assert!(cal.is_remembered());
        assert_eq!(cal.date, "2026-11-24");
        assert_eq!(sync(&dir), LatencySync::Unchanged);

        // And again the same day (Bluetooth headphones reconnected).
        remember_latency(&home.remembered, -34, &measured("2026-11-24")).unwrap();
        assert!(matches!(sync(&dir), LatencySync::Applied(_)));
        assert_eq!(read_config(&dir).unwrap().input.latency_offset_ms, -34);

        // An older measurement (the store was rebuilt from an old project)
        // does not replace the newer copy.
        std::fs::remove_file(&home.remembered).unwrap();
        remember_latency(&home.remembered, -20, &measured("2026-11-20")).unwrap();
        assert_eq!(sync(&dir), LatencySync::Unchanged);
        assert_eq!(read_config(&dir).unwrap().input.latency_offset_ms, -34);

        // A project's own measurement is never replaced by a newer one.
        let own = project(&tmp, "own.takewise");
        set_calibration(&own, -12, &measured("2026-11-19")).unwrap();
        assert_eq!(sync(&own), LatencySync::Unchanged);
        assert_eq!(read_config(&own).unwrap().input.latency_offset_ms, -12);
    }

    #[test]
    fn the_newest_measurement_wins() {
        let home = home();
        remember_latency(&home.remembered, -29, &measured("2026-11-23")).unwrap();
        remember_latency(&home.remembered, -28, &measured("2026-11-24")).unwrap();
        let other = CalibrationInfo {
            buffer_frames: Some(256),
            ..measured("2026-11-24")
        };
        remember_latency(&home.remembered, -20, &other).unwrap();
        let remembered = read(&home.remembered).unwrap();
        assert_eq!(remembered.latency.len(), 2, "{remembered:?}");
        let on_512 = remembered
            .latency
            .iter()
            .find(|l| l.fits(&route(512)))
            .unwrap();
        assert_eq!(on_512.latency_offset_ms, -28);
        // Without a buffer size, the route is not exact: not remembered.
        let unknown = CalibrationInfo {
            buffer_frames: None,
            ..measured("2026-11-25")
        };
        assert!(!remember_latency(&home.remembered, -1, &unknown).unwrap());
        assert_eq!(read(&home.remembered).unwrap(), remembered);
    }

    #[test]
    fn a_projects_own_measurement_wins() {
        let home = home();
        let tmp = tempfile::tempdir().unwrap();
        remember_latency(&home.remembered, -29, &measured("2026-11-24")).unwrap();
        let dir = project(&tmp, "own.takewise");
        set_calibration(&dir, -12, &measured("2026-11-20")).unwrap();
        let config = read_config(&dir).unwrap();
        assert_eq!(
            sync_latency(&home.remembered, &dir, &config, &route(512)).unwrap(),
            LatencySync::Unchanged
        );
        let config = read_config(&dir).unwrap();
        assert_eq!(config.input.latency_offset_ms, -12);
        assert!(!config.calibration.unwrap().is_remembered());
        // The older own measurement does not replace the newer remembered one.
        let remembered = read(&home.remembered).unwrap();
        assert_eq!(remembered.latency[0].latency_offset_ms, -29);
    }

    #[test]
    fn a_stale_view_of_the_project_does_not_replace_a_measurement_made_since() {
        let home = home();
        let tmp = tempfile::tempdir().unwrap();
        remember_latency(&home.remembered, -29, &measured("2026-11-23")).unwrap();
        let dir = project(&tmp, "open.takewise");
        // The window read the project while it had no calibration.
        let stale = read_config(&dir).unwrap();
        assert_eq!(stale.calibration, None);
        // Then `takewise calibrate --write` measured in the project.
        set_calibration(&dir, -35, &measured("2026-11-24")).unwrap();

        // The window syncs with what it read: the project's own
        // measurement, which is on disk by now, stays.
        assert_eq!(
            sync_latency(&home.remembered, &dir, &stale, &route(512)).unwrap(),
            LatencySync::Unchanged
        );
        let config = read_config(&dir).unwrap();
        assert_eq!(config.input.latency_offset_ms, -35);
        let cal = config.calibration.unwrap();
        assert!(!cal.is_remembered(), "{cal:?}");
        assert_eq!(cal.date, "2026-11-24");

        // A copy that was already replaced on disk is not written twice
        // either: the view that still has the older copy changes nothing.
        let other = project(&tmp, "copied.takewise");
        let config = read_config(&other).unwrap();
        sync_latency(&home.remembered, &other, &config, &route(512)).unwrap();
        let old_copy = read_config(&other).unwrap();
        remember_latency(&home.remembered, -31, &measured("2026-11-24")).unwrap();
        sync_latency(&home.remembered, &other, &old_copy, &route(512)).unwrap();
        let before = fs::read(other.join(crate::project::PROJECT_FILE)).unwrap();
        assert_eq!(
            sync_latency(&home.remembered, &other, &old_copy, &route(512)).unwrap(),
            LatencySync::Unchanged
        );
        assert_eq!(
            fs::read(other.join(crate::project::PROJECT_FILE)).unwrap(),
            before
        );
    }

    #[test]
    fn an_existing_project_calibration_becomes_remembered() {
        let home = home();
        let tmp = tempfile::tempdir().unwrap();
        // Measured before anything was remembered, as in 0.1.3.
        let measured_dir = project(&tmp, "focal-promo.takewise");
        set_calibration(&measured_dir, -29, &measured("2026-11-23")).unwrap();
        let config = read_config(&measured_dir).unwrap();
        sync_latency(&home.remembered, &measured_dir, &config, &route(512)).unwrap();
        let remembered = read(&home.remembered).unwrap();
        assert_eq!(
            remembered.latency,
            [RememberedLatency {
                input: DUO.to_owned(),
                output: XM5.to_owned(),
                buffer_frames: 512,
                latency_offset_ms: -29,
                date: "2026-11-23".to_owned(),
            }]
        );
        // A new project on the same devices gets it.
        let dir = project(&tmp, "kindred.takewise");
        let config = read_config(&dir).unwrap();
        sync_latency(&home.remembered, &dir, &config, &route(512)).unwrap();
        assert_eq!(read_config(&dir).unwrap().input.latency_offset_ms, -29);
        // A copied calibration is not remembered as a measurement of its own.
        let copied = read_config(&dir).unwrap();
        remember_latency(&home.remembered, -30, &measured("2026-11-25")).unwrap();
        sync_latency(&home.remembered, &dir, &copied, &route(128)).unwrap();
        assert_eq!(
            read(&home.remembered).unwrap().latency[0].latency_offset_ms,
            -30
        );
    }

    #[test]
    fn init_copies_nothing() {
        let home = home();
        remember_latency(&home.remembered, -29, &measured("2026-11-23")).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dir = project(&tmp, "fresh.takewise");
        let config = read_config(&dir).unwrap();
        assert_eq!(config.calibration, None);
        assert_eq!(config.setup, None);
        assert_eq!(config.input.latency_offset_ms, 0);
    }

    /// Runs a setup check's save in `dir` at `at`: a room tone, then
    /// `[setup]`, then remembers it.
    fn setup_check(home: &Home, dir: &Path, at: &str, floor: f64) -> SetupInfo {
        let at: jiff::Timestamp = at.parse().unwrap();
        let input = RoomInput {
            input: DUO,
            channel: 1,
            output: XM5,
            floor_dbfs: floor,
            gated: false,
        };
        let tone = save_room_tone(dir, &[3, -3, 2], 48_000, at, &input).unwrap();
        let info = SetupInfo {
            date: jiff::tz::TimeZone::UTC.to_datetime(at).date().to_string(),
            input: DUO.to_owned(),
            channel: 1,
            output: XM5.to_owned(),
            room_floor_dbfs: Some(floor),
            speech_to_noise_db: Some(44.0),
            from: None,
        };
        set_setup(dir, &info).unwrap();
        let today = day(&info.date);
        remember_setup(&home.remembered, dir, &info, &tone, today).unwrap();
        info
    }

    #[test]
    fn a_setup_check_is_adopted_on_the_same_day_only() {
        let home = home();
        let tmp = tempfile::tempdir().unwrap();
        let checked = project(&tmp, "checked.takewise");
        let info = setup_check(&home, &checked, "2026-11-24T09:00:00Z", -70.0);

        // The next day: nothing is adopted.
        let dir = project(&tmp, "new.takewise");
        let config = read_config(&dir).unwrap();
        let tomorrow = day("2026-11-25");
        assert_eq!(
            adopt_setup(&home.remembered, &dir, &config, &route(512), tomorrow).unwrap(),
            None
        );
        assert_eq!(room_tones(&dir).unwrap(), []);
        // Another channel: nothing either.
        let other = Route {
            channel: 2,
            ..route(512)
        };
        let today = day("2026-11-24");
        assert_eq!(
            adopt_setup(&home.remembered, &dir, &config, &other, today).unwrap(),
            None
        );

        // The same day and devices: the room tone is copied, then [setup].
        let adopted = adopt_setup(&home.remembered, &dir, &config, &route(512), today)
            .unwrap()
            .expect("adopted");
        assert!(adopted.is_remembered());
        let config = read_config(&dir).unwrap();
        assert_eq!(config.setup, Some(adopted.clone()));
        assert_eq!(
            SetupInfo {
                from: None,
                ..adopted
            },
            info
        );
        let tones = room_tones(&dir).unwrap();
        assert_eq!(tones.len(), 1);
        assert!(
            tones[0].file.starts_with("takes/.room/"),
            "{}",
            tones[0].file
        );
        assert_eq!(
            tones[0].recorded_at,
            room_tones(&checked).unwrap()[0].recorded_at
        );
        let (samples, _) = read_take_wav(&dir.join(&tones[0].file)).unwrap();
        assert_eq!(samples, [3, -3, 2]);
        // Adopting again changes nothing: [setup] is from today now.
        assert_eq!(
            adopt_setup(&home.remembered, &dir, &config, &route(512), today).unwrap(),
            None
        );
        assert_eq!(room_tones(&dir).unwrap().len(), 1);

        // A project with its own check from today keeps it.
        let own = project(&tmp, "own.takewise");
        setup_check(&home, &own, "2026-11-24T08:00:00Z", -60.0);
        let config = read_config(&own).unwrap();
        assert_eq!(
            adopt_setup(&home.remembered, &own, &config, &route(512), today).unwrap(),
            None
        );
        assert_eq!(room_tones(&own).unwrap().len(), 1);
    }

    #[test]
    fn an_older_setup_is_replaced_by_todays() {
        let home = home();
        let tmp = tempfile::tempdir().unwrap();
        let dir = project(&tmp, "old.takewise");
        setup_check(&home, &dir, "2026-11-23T09:00:00Z", -60.0);
        let other = project(&tmp, "checked.takewise");
        setup_check(&home, &other, "2026-11-24T09:00:00Z", -70.0);
        let config = read_config(&dir).unwrap();
        let adopted = adopt_setup(
            &home.remembered,
            &dir,
            &config,
            &route(512),
            day("2026-11-24"),
        )
        .unwrap()
        .expect("yesterday's own check gives way to today's");
        assert_eq!(adopted.room_floor_dbfs, Some(-70.0));
        assert_eq!(
            room_tones(&dir).unwrap().len(),
            2,
            "both room tones are kept"
        );
    }

    #[test]
    fn earlier_days_are_pruned() {
        let home = home();
        let tmp = tempfile::tempdir().unwrap();
        let dir = project(&tmp, "p.takewise");
        setup_check(&home, &dir, "2026-11-23T09:00:00Z", -60.0);
        let rooms = home.remembered.with_file_name(ROOMS_DIR);
        assert_eq!(fs::read_dir(&rooms).unwrap().count(), 2, "WAV and sidecar");
        // The same devices the next day: only the new one is kept.
        setup_check(&home, &dir, "2026-11-24T09:00:00Z", -61.0);
        let remembered = read(&home.remembered).unwrap();
        assert_eq!(remembered.setup.len(), 1);
        assert_eq!(remembered.setup[0].setup.date, "2026-11-24");
        let mut names: Vec<String> = fs::read_dir(&rooms)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["20261124T090000Z.json", "20261124T090000Z.wav"]);
        assert_eq!(
            remembered.setup[0].room_tone.file,
            "rooms/20261124T090000Z.wav"
        );
        // Again the same day: the newer replaces it.
        setup_check(&home, &dir, "2026-11-24T10:00:00Z", -62.0);
        let remembered = read(&home.remembered).unwrap();
        assert_eq!(remembered.setup.len(), 1);
        assert_eq!(remembered.setup[0].setup.room_floor_dbfs, Some(-62.0));
        assert_eq!(fs::read_dir(&rooms).unwrap().count(), 2);
    }

    #[test]
    fn a_failed_store_write_keeps_the_room_tone_it_still_names() {
        let home = home();
        let tmp = tempfile::tempdir().unwrap();
        let dir = project(&tmp, "p.takewise");
        setup_check(&home, &dir, "2026-11-24T09:00:00Z", -60.0);
        let before = fs::read(&home.remembered).unwrap();
        let rooms = home.remembered.with_file_name(ROOMS_DIR);
        let old = rooms.join("20261124T090000Z.wav");
        assert!(old.is_file());

        // The check runs again; its room tone is copied, then the store
        // cannot be written.
        let at: jiff::Timestamp = "2026-11-24T10:00:00Z".parse().unwrap();
        let input = RoomInput {
            input: DUO,
            channel: 1,
            output: XM5,
            floor_dbfs: -62.0,
            gated: false,
        };
        let tone = save_room_tone(&dir, &[5, -5, 4], 48_000, at, &input).unwrap();
        let info = read_config(&dir).unwrap().setup.unwrap();
        WRITES_FAIL.set(true);
        let failed = remember_setup(&home.remembered, &dir, &info, &tone, day("2026-11-24"));
        WRITES_FAIL.set(false);
        assert!(failed.is_err());
        assert!(rooms.join("20261124T100000Z.wav").is_file(), "copied");

        // The store still names the earlier room tone, and still has it.
        assert_eq!(fs::read(&home.remembered).unwrap(), before);
        assert!(old.is_file(), "the room tone the store names was removed");
        let fresh = project(&tmp, "fresh.takewise");
        let config = read_config(&fresh).unwrap();
        let today = day("2026-11-24");
        let adopted = adopt_setup(&home.remembered, &fresh, &config, &route(512), today);
        assert!(adopted.unwrap().is_some(), "still adoptable");
        let tones = room_tones(&fresh).unwrap();
        let (samples, _) = read_take_wav(&fresh.join(&tones[0].file)).unwrap();
        assert_eq!(samples, [3, -3, 2]);

        // Once the store is written, the replaced room tone goes.
        remember_setup(&home.remembered, &dir, &info, &tone, today).unwrap();
        assert!(!old.exists());
        assert_eq!(fs::read_dir(&rooms).unwrap().count(), 2, "WAV and sidecar");
    }

    #[test]
    fn room_tones_are_pruned_only_once_the_new_state_is_durable() {
        let home = home();
        let tmp = tempfile::tempdir().unwrap();
        let dir = project(&tmp, "p.takewise");
        let rooms = home.remembered.with_file_name(ROOMS_DIR);
        let store_dir = home.remembered.parent().unwrap().to_path_buf();
        // The first check makes the folder of the room tones, which is
        // synced into its own folder too.
        crate::fsutil::dir_syncs::start();
        setup_check(&home, &dir, "2026-11-23T09:00:00Z", -60.0);
        let synced = crate::fsutil::dir_syncs::take();
        assert_eq!(
            synced,
            [rooms.clone(), store_dir.clone(), store_dir.clone()]
        );
        let old = rooms.join("20261123T090000Z.wav");

        // The next day's check: the new room tone and its folder are
        // synced before the store names it, and the store's folder before
        // the room tone it named until then is removed.
        crate::fsutil::dir_syncs::start();
        setup_check(&home, &dir, "2026-11-24T09:00:00Z", -61.0);
        let synced = crate::fsutil::dir_syncs::take();
        assert_eq!(synced, [rooms.clone(), store_dir.clone()]);
        assert!(!old.exists(), "pruned");

        // A store folder that cannot be synced: the store's rename may
        // not survive a power loss, so nothing is pruned.
        let kept = rooms.join("20261124T090000Z.wav");
        let at: jiff::Timestamp = "2026-11-24T10:00:00Z".parse().unwrap();
        let input = RoomInput {
            input: DUO,
            channel: 1,
            output: XM5,
            floor_dbfs: -62.0,
            gated: false,
        };
        let tone = save_room_tone(&dir, &[5, -5, 4], 48_000, at, &input).unwrap();
        let info = read_config(&dir).unwrap().setup.unwrap();
        crate::fsutil::dir_syncs::fail(Some(store_dir));
        let failed = remember_setup(&home.remembered, &dir, &info, &tone, day("2026-11-24"));
        crate::fsutil::dir_syncs::fail(None);
        assert!(failed.is_err());
        assert!(kept.is_file(), "the room tone the old store names");
        assert!(rooms.join("20261124T100000Z.wav").is_file(), "the new one");
        // A room tone whose folder cannot be synced is not published.
        let before = fs::read(&home.remembered).unwrap();
        crate::fsutil::dir_syncs::fail(Some(rooms.clone()));
        let failed = remember_setup(&home.remembered, &dir, &info, &tone, day("2026-11-24"));
        crate::fsutil::dir_syncs::fail(None);
        assert!(failed.is_err());
        assert_eq!(fs::read(&home.remembered).unwrap(), before);
    }

    #[test]
    fn settings_toml_is_untouched() {
        let home = home();
        let before = fs::read(&home.settings).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dir = project(&tmp, "p.takewise");
        remember_latency(&home.remembered, -29, &measured("2026-11-23")).unwrap();
        let config = read_config(&dir).unwrap();
        sync_latency(&home.remembered, &dir, &config, &route(512)).unwrap();
        setup_check(&home, &dir, "2026-11-24T09:00:00Z", -60.0);
        let fresh = project(&tmp, "q.takewise");
        let config = read_config(&fresh).unwrap();
        adopt_setup(
            &home.remembered,
            &fresh,
            &config,
            &route(512),
            day("2026-11-24"),
        )
        .unwrap();
        assert_eq!(fs::read(&home.settings).unwrap(), before);
        assert_eq!(
            crate::route::load_defaults_from(&home.settings)
                .unwrap()
                .unwrap()
                .input
                .as_deref(),
            Some(DUO)
        );
    }

    #[test]
    fn a_remembered_file_from_a_newer_version_loads() {
        let home = home();
        fs::write(
            &home.remembered,
            r#"{"latency":[{"input":"a","output":"b","buffer_frames":512,"latency_offset_ms":-3,"date":"2026-11-24","extra":1}],"future":[]}"#,
        )
        .unwrap();
        let remembered = read(&home.remembered).unwrap();
        assert_eq!(remembered.latency.len(), 1);
        assert_eq!(remembered.setup, []);
        assert_eq!(
            read(&home.remembered.with_file_name("none.json")).unwrap(),
            Remembered::default()
        );
    }
}
