//! The audio route (input, channel, output, capture buffer), whether the
//! latency calibration and the setup check still match it, and the route
//! saved as the default for new projects.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result, io};
use crate::fsutil::write_atomic;
use crate::project::ProjectConfig;

/// Where the app's defaults live unless `TAKEWISE_SETTINGS` names another
/// file: relative to the home directory.
const SETTINGS_FILE: &str = "Library/Application Support/Takewise/settings.toml";

/// The audio route in use: devices by name, the 1-based input channel and
/// the capture buffer size when known.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Route {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    pub channel: u16,
    /// The output device; `None` when unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// The capture buffer in frames; `None` when unknown. Not saved as a
    /// default.
    #[serde(skip)]
    pub buffer_frames: Option<u32>,
}

impl Route {
    /// The project's configured route, without the buffer size: input and
    /// channel from `[input]`, the output from `[output]` (`None` for the
    /// system default).
    pub fn of(config: &ProjectConfig) -> Route {
        Route {
            input: config.input.device.clone(),
            channel: config.input.channel,
            output: config.output.device.clone(),
            buffer_frames: None,
        }
    }
}

/// Why the latency calibration no longer fits `current`, if it does not:
/// the input, the output or the buffer size differ from the ones
/// `[calibration]` recorded, the record lacks the buffer size, or the
/// project has an offset measured before calibrations were recorded.
/// Unknown parts of `current` are not compared.
pub fn calibration_stale(config: &ProjectConfig, current: &Route) -> Option<String> {
    let again = "run `takewise calibrate --write` again";
    let Some(cal) = &config.calibration else {
        return (config.input.latency_offset_ms != 0).then(|| {
            format!(
                "the latency calibration does not say which devices it was measured on; {again}"
            )
        });
    };
    let mut changed = Vec::new();
    if current.input.as_deref() != Some(cal.input.as_str()) {
        changed.push(format!(
            "input {} (calibrated with {:?})",
            current
                .input
                .as_deref()
                .map_or_else(|| "unset".to_owned(), |name| format!("{name:?}")),
            cal.input
        ));
    }
    if let Some(output) = current.output.as_deref().filter(|o| *o != cal.output) {
        changed.push(format!(
            "output {output:?} (calibrated with {:?})",
            cal.output
        ));
    }
    match (cal.buffer_frames, current.buffer_frames) {
        (None, _) => changed.push("buffer size not recorded".to_owned()),
        (Some(then), Some(now)) if then != now => {
            changed.push(format!("buffer {now} frames (calibrated at {then})"));
        }
        _ => {}
    }
    (!changed.is_empty()).then(|| {
        format!(
            "the latency calibration is stale: {}; {again}",
            changed.join(", ")
        )
    })
}

/// The latency offset to apply on `current`, in ms, and whether the
/// calibration is stale there: `[input] latency_offset_ms` only while
/// [`calibration_stale`] finds the calibration still fits the route (the
/// same input, output and buffer), else `(0, true)`. An offset measured on
/// other devices would shift takes by the wrong amount, so none applies
/// until `takewise calibrate --write` runs on this route.
pub fn effective_latency_offset(config: &ProjectConfig, current: &Route) -> (i32, bool) {
    match calibration_stale(config, current) {
        Some(_) => (0, true),
        None => (config.input.latency_offset_ms, false),
    }
}

/// Why the setup check does not cover `current`: it never ran, or the
/// devices or channel differ from the ones it checked.
pub fn setup_warning(config: &ProjectConfig, current: &Route) -> Option<String> {
    let Some(setup) = &config.setup else {
        return Some("no setup check has run on this project".to_owned());
    };
    let same = current.input.as_deref() == Some(setup.input.as_str())
        && current.channel == setup.channel
        && current
            .output
            .as_deref()
            .is_none_or(|output| output == setup.output);
    (!same).then(|| {
        format!(
            "the devices changed since the setup check on {} ({:?} channel {}, output {:?})",
            setup.date, setup.input, setup.channel, setup.output
        )
    })
}

/// The settings file: `TAKEWISE_SETTINGS`, else
/// `~/Library/Application Support/Takewise/settings.toml`.
pub fn settings_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TAKEWISE_SETTINGS") {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("HOME").map(|home| Path::new(&home).join(SETTINGS_FILE))
}

/// The route saved for new projects, if any.
pub fn load_defaults() -> Result<Option<Route>> {
    match settings_path() {
        Some(path) => load_defaults_from(&path),
        None => Ok(None),
    }
}

/// Saves the route's devices and channel as the defaults for new projects.
pub fn save_defaults(route: &Route) -> Result<()> {
    match settings_path() {
        Some(path) => save_defaults_to(&path, route),
        None => Ok(()),
    }
}

pub fn load_defaults_from(path: &Path) -> Result<Option<Route>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(io(path)(err)),
    };
    toml::from_str(&text).map(Some).map_err(|err| Error::Parse {
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

pub fn save_defaults_to(path: &Path, route: &Route) -> Result<()> {
    #[cfg(test)]
    assert!(
        path.starts_with(std::env::temp_dir())
            || fs::canonicalize(std::env::temp_dir()).is_ok_and(|tmp| path.starts_with(tmp)),
        "a test wrote settings outside the temporary folder: {}",
        path.display()
    );
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(io(dir))?;
    }
    let text = toml::to_string(route).map_err(|err| Error::Parse {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    write_atomic(
        path,
        format!("# Takewise: the devices new projects start with.\n{text}").as_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::project::{CalibrationInfo, FilmInfo, Project, SetupInfo};

    fn config() -> ProjectConfig {
        let tmp = tempfile::tempdir().unwrap();
        let info = FilmInfo {
            duration_s: 10.0,
            size: 1,
            modified: "2026-11-23T00:00:00Z".to_owned(),
        };
        let mut config = Project::init(tmp.path(), Path::new("film.mp4"), info)
            .unwrap()
            .config;
        config.input.device = Some("Duo Main".to_owned());
        config.input.latency_offset_ms = 7;
        config.calibration = Some(CalibrationInfo {
            input: "Duo Main".to_owned(),
            output: "Duo Main".to_owned(),
            buffer_frames: Some(512),
            date: "2026-11-23".to_owned(),
            from: None,
        });
        config
    }

    fn route(input: &str, output: Option<&str>, buffer: Option<u32>) -> Route {
        Route {
            input: Some(input.to_owned()),
            channel: 1,
            output: output.map(str::to_owned),
            buffer_frames: buffer,
        }
    }

    #[test]
    fn calibration_fresh_on_the_same_route() {
        let config = config();
        assert_eq!(
            calibration_stale(&config, &route("Duo Main", Some("Duo Main"), Some(512))),
            None
        );
        // Unknown output and buffer are not compared.
        assert_eq!(
            calibration_stale(&config, &route("Duo Main", None, None)),
            None
        );
    }

    #[test]
    fn calibration_stale_on_device_change() {
        let config = config();
        let input = calibration_stale(&config, &route("MacBook Mic", Some("Duo Main"), Some(512)))
            .expect("stale on another input");
        assert!(input.contains("input \"MacBook Mic\""), "{input}");
        assert!(input.contains("calibrate --write"), "{input}");
        let output = calibration_stale(&config, &route("Duo Main", Some("Speakers"), Some(512)))
            .expect("stale on another output");
        assert!(output.contains("output \"Speakers\""), "{output}");
    }

    #[test]
    fn the_offset_applies_only_on_its_route() {
        let config = config();
        let same = route("Duo Main", Some("Duo Main"), Some(512));
        assert_eq!(effective_latency_offset(&config, &same), (7, false));
        // Another input, output or buffer: no offset, and stale.
        for other in [
            route("MacBook Mic", Some("Duo Main"), Some(512)),
            route("Duo Main", Some("Speakers"), Some(512)),
            route("Duo Main", Some("Duo Main"), Some(128)),
        ] {
            assert_eq!(
                effective_latency_offset(&config, &other),
                (0, true),
                "{other:?}"
            );
        }
        // An offset from before calibrations were recorded does not apply.
        let mut legacy = config.clone();
        legacy.calibration = None;
        assert_eq!(effective_latency_offset(&legacy, &same), (0, true));
        // No calibration and no offset: nothing to apply, nothing stale.
        legacy.input.latency_offset_ms = 0;
        assert_eq!(effective_latency_offset(&legacy, &same), (0, false));
    }

    #[test]
    fn calibration_stale_on_buffer_change() {
        let mut config = config();
        let stale = calibration_stale(&config, &route("Duo Main", Some("Duo Main"), Some(128)))
            .expect("stale on another buffer size");
        assert!(
            stale.contains("buffer 128 frames (calibrated at 512)"),
            "{stale}"
        );
        // A calibration that did not record its buffer is stale too.
        config.calibration.as_mut().unwrap().buffer_frames = None;
        let unknown = calibration_stale(&config, &route("Duo Main", Some("Duo Main"), Some(512)))
            .expect("stale without a recorded buffer");
        assert!(unknown.contains("buffer size not recorded"), "{unknown}");
    }

    #[test]
    fn calibration_stale_without_record() {
        let mut config = config();
        config.calibration = None;
        let stale = calibration_stale(&config, &route("Duo Main", None, None))
            .expect("an offset without a record is stale");
        assert!(stale.contains("does not say which devices"), "{stale}");
        // No offset and no record: nothing was calibrated, nothing is stale.
        config.input.latency_offset_ms = 0;
        assert_eq!(
            calibration_stale(&config, &route("Duo Main", None, None)),
            None
        );
    }

    #[test]
    fn setup_warns_when_missing_or_changed() {
        let mut config = config();
        let current = route("Duo Main", Some("Duo Main"), None);
        assert!(
            setup_warning(&config, &current)
                .unwrap()
                .contains("no setup check")
        );
        config.setup = Some(SetupInfo {
            date: "2026-11-23".to_owned(),
            input: "Duo Main".to_owned(),
            channel: 1,
            output: "Duo Main".to_owned(),
            room_floor_dbfs: Some(-68.0),
            speech_to_noise_db: Some(46.0),
            from: None,
        });
        assert_eq!(setup_warning(&config, &current), None);
        let other = Route {
            channel: 2,
            ..current
        };
        assert!(
            setup_warning(&config, &other)
                .unwrap()
                .contains("devices changed")
        );
    }

    #[test]
    fn defaults_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("Takewise/settings.toml");
        assert_eq!(load_defaults_from(&path).unwrap(), None);
        let saved = Route {
            input: Some("Duo Main".to_owned()),
            channel: 2,
            output: Some("Duo Main".to_owned()),
            buffer_frames: Some(128),
        };
        save_defaults_to(&path, &saved).unwrap();
        let loaded = load_defaults_from(&path).unwrap().unwrap();
        assert_eq!(
            loaded,
            Route {
                buffer_frames: None,
                ..saved
            },
            "the buffer size is not a default"
        );
    }
}
