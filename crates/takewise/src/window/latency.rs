//! The setup check's latency step: the loopback calibration of
//! `takewise calibrate`, run in the window with the window's media and the
//! input capture its meter already holds. The beep film replaces the
//! project's film while it plays; the project's film comes back after.
//! A consistent measurement is stored with the devices it was measured on;
//! anything else is shown and nothing is written.

use std::path::{Path, PathBuf};

use gpui_kit::AnyElement;
use takewise_core::{self as tw, CalibrationInfo, Route};

use takewise_media::DeviceLookup;

use super::output::{Lookup, Seen};
use super::setup::{Setup, calibration_needed, line};
use super::{RecordingWindow, colors};
use crate::calibrate::{self, Measurement, Run};

/// Makes the beep film in a directory: [`calibrate::make_beep_film`], or a
/// stand-in in tests.
pub type BeepFilm = fn(&Path) -> Result<PathBuf, String>;

/// Why a calibration ended without a result when the input, its channel
/// or the output changed while it measured.
pub const REROUTED: &str = "calibration cancelled: the devices changed";

/// A calibration while it runs.
pub struct Calibrating {
    run: Run,
    /// Holds the beep film until the run ends.
    _work: tempfile::TempDir,
    /// The route it measures: as configured, and as in use, when it began.
    configured: Route,
    route: Route,
    /// The very output device it began on (see [`OutputIdentity`]).
    output: OutputIdentity,
}

/// Which output device plays: the device the players are set to (its UID
/// and CoreAudio object), or the system output's object; not known when it
/// could not be read. A device that went away and came back keeps its UID
/// but has another object.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OutputIdentity {
    Device(String, u32),
    System(u32),
    Unknown,
}

/// Why a calibration ended when its output dropped out or changed.
pub const OUTPUT_CHANGED: &str =
    "the output dropped out or changed while measuring; press Space to measure again";

impl RecordingWindow {
    /// Space in the latency step: makes the beep film, puts it in place of
    /// the project's film and starts measuring the meter's input.
    pub(super) fn start_calibration(&mut self) {
        if self.calibrating.is_some() {
            return;
        }
        let started = self.begin_calibration();
        if let Some(setup) = &mut self.setup {
            match started {
                Ok(()) => setup.latency = None,
                Err(err) => setup.latency = Some(Err(err)),
            }
        }
    }

    fn begin_calibration(&mut self) -> Result<(), String> {
        if let Some(problem) = &self.input_problem {
            return Err(problem.clone());
        }
        // The beeps play where takes do, never on the system output in
        // place of a missing one.
        // The beeps start at once: only on a confirmed route.
        let ready = self
            .sync_output()
            .map_err(|problem| problem.for_playback())?;
        if ready == super::output::Ready::Pending {
            return Err("the output could not be checked just now; press Space again".to_owned());
        }
        let route = self.current_route();
        let device = route.input.clone().ok_or("No input device chosen.")?;
        let output = route.output.clone().unwrap_or_else(|| "unknown".to_owned());
        let work =
            tempfile::tempdir().map_err(|err| format!("No folder for the beep film: {err}"))?;
        let film = (self.beep_film)(work.path())?;
        if !self.recorder.start_tap() {
            return Err(
                "The input is not running yet; wait a moment and press Space again.".into(),
            );
        }
        let started = self
            .media
            .film_open(&film)
            .and_then(|()| Run::new(&*self.media, &device, &output));
        match started {
            Ok(run) => {
                // The beep film is in its place.
                self.film.set_aside();
                self.calibrating = Some(Calibrating {
                    run,
                    _work: work,
                    configured: Route::of(&self.project.config),
                    route,
                    output: self.output_identity(),
                });
                Ok(())
            }
            Err(err) => {
                self.recorder.stop_tap();
                self.reopen_film_now();
                Err(err)
            }
        }
    }

    /// Once per frame while a calibration runs: hands it the meter's
    /// buffers; stores the result once it is done.
    pub(super) fn tick_calibration(&mut self) {
        if self.cancel_calibration_if_rerouted() {
            self.message = Some(format!("Latency {REROUTED}."));
            return;
        }
        let Some(calibrating) = &mut self.calibrating else {
            return;
        };
        let result = if self.recorder.tap_broken() {
            Some(Err(
                "the input stopped or changed while measuring; press Space to measure again"
                    .to_owned(),
            ))
        } else {
            let buffers = self.recorder.drain_tap();
            calibrating.run.tick(&mut *self.media, buffers)
        };
        let Some(result) = result else {
            return;
        };
        self.end_calibration();
        let outcome = result.and_then(|measurement| self.store_calibration(&measurement));
        let Some(setup) = &mut self.setup else {
            return;
        };
        match outcome {
            // Opened with L: done once a measurement is stored.
            Ok(stored) if setup.latency_only => {
                self.setup = None;
                self.message = Some(format!("Latency measured: {stored}"));
            }
            Ok(stored) => setup.latency = Some(Ok(format!("{stored} Press Enter to finish."))),
            Err(err) => setup.latency = Some(Err(err)),
        }
    }

    /// Esc, or closing the setup check: stops a calibration in progress
    /// without writing anything.
    pub(super) fn cancel_calibration(&mut self) {
        if self.calibrating.is_some() {
            self.end_calibration();
        }
    }

    /// The output device the film plays through now: the one the players
    /// are set to, or the system's default output.
    fn output_identity(&self) -> OutputIdentity {
        match (&self.film_output, &self.applied_device) {
            (Some(_), Some((uid, Some(object)))) => OutputIdentity::Device(uid.clone(), *object),
            (Some(_), _) => OutputIdentity::Unknown,
            (None, _) => match self.media.default_output_object() {
                DeviceLookup::Present(object) => OutputIdentity::System(object),
                DeviceLookup::Absent | DeviceLookup::Unknown => OutputIdentity::Unknown,
            },
        }
    }

    /// Every tick, before anything sets the output again: cancels a
    /// calibration whose output device dropped out or changed since it
    /// began (a reconnect gives it another object), whose beeps may have
    /// played elsewhere. One property read; a single failed read does not
    /// cancel it (see [`LOOKUP_GRACE_S`]).
    ///
    /// [`LOOKUP_GRACE_S`]: super::output::LOOKUP_GRACE_S
    pub(super) fn cancel_calibration_if_output_dropped(&mut self) {
        let Some(calibrating) = &self.calibrating else {
            return;
        };
        let seen = match calibrating.output.clone() {
            OutputIdentity::Device(uid, object) => self.see(&uid, object, Lookup::Calibration),
            OutputIdentity::System(object) => {
                let lookup = self.media.default_output_object();
                self.seen_lookup(lookup, object, Lookup::Calibration)
            }
            // Not known when it began: nothing to compare.
            OutputIdentity::Unknown => return,
        };
        if !matches!(seen, Seen::Dropped(_) | Seen::Unreadable) {
            return;
        }
        self.end_calibration();
        if let Some(setup) = &mut self.setup {
            setup.latency = Some(Err(OUTPUT_CHANGED.to_owned()));
        }
        self.message = Some(format!("Latency calibration cancelled: {OUTPUT_CHANGED}."));
    }

    /// Cancels a calibration whose route changed since it began (in the
    /// panel, in `project.toml`, or the output in use), so no result
    /// measured on two routes is stored; says so in the latency step.
    /// Returns whether it cancelled one.
    pub(super) fn cancel_calibration_if_rerouted(&mut self) -> bool {
        let Some(calibrating) = &self.calibrating else {
            return false;
        };
        let (configured, route) = (calibrating.configured.clone(), calibrating.route.clone());
        if Route::of(&self.project.config) == configured && self.current_route() == route {
            return false;
        }
        self.end_calibration();
        if let Some(setup) = &mut self.setup {
            setup.latency = Some(Err(REROUTED.to_owned()));
        }
        true
    }

    fn end_calibration(&mut self) {
        self.calibrating = None;
        self.recorder.stop_tap();
        self.media.film_pause();
        self.reopen_film_now();
    }

    /// Puts the project's film back, at its volume and parked where it was;
    /// it warms up again before the next take. Only once its output can be
    /// used (see [`RecordingWindow::reopen_film`]): a player opened on a
    /// missing output fails.
    pub(super) fn restore_film(&mut self) -> Result<(), String> {
        let opened = self.media.film_open(&self.project.film_path());
        self.media.film_set_gain_db(self.volumes.film);
        self.media.film_set_muted(false);
        self.recorder.rewarm();
        self.park();
        opened
    }

    /// Stores a consistent measurement's offset with the devices it was
    /// measured on; says what was stored, or why nothing was.
    fn store_calibration(&mut self, measurement: &Measurement) -> Result<String, String> {
        let placed = format!(
            "{} of {} beeps placed{}",
            measurement.placed.len(),
            measurement.expected,
            measurement
                .mean_ms
                .map(|mean| format!(", mean error {mean:+.1} ms"))
                .unwrap_or_default()
        );
        let offset = calibrate::judge(measurement).map_err(|why| format!("{placed}: {why}"))?;
        let info = CalibrationInfo {
            input: measurement.input.clone(),
            output: measurement.output.clone(),
            buffer_frames: measurement.buffer_frames,
            date: crate::calibrate::today(),
            from: None,
        };
        tw::set_calibration(&self.dir, offset, &info)
            .map_err(|err| format!("{placed}, but it could not be saved: {err}"))?;
        self.remember_latency(offset, &info);
        self.reload();
        self.check_inputs();
        Ok(format!(
            "{placed}. Stored latency_offset_ms = {offset} for {} and {}.",
            measurement.input, measurement.output
        ))
    }

    /// The latency step's content.
    pub(super) fn latency_body(&self, setup: &Setup) -> Vec<AnyElement> {
        let mut body = Vec::new();
        let route = self.current_route();
        if let Some(why) = calibration_needed(&self.project.config, &route) {
            body.push(line(format!("Latency: {why}."), colors::WARN));
        }
        if let Some(note) = self.remembered_note(&route) {
            body.push(line(note, colors::DIM));
        }
        if self.calibrating.is_some() {
            body.push(line(
                "Measuring… keep the headphones at the mic until the beeps stop.".to_owned(),
                colors::RECORD,
            ));
            return body;
        }
        match &setup.latency {
            Some(Ok(stored)) => body.push(line(stored.clone(), colors::GOOD)),
            Some(Err(err)) => {
                body.push(line(format!("Not calibrated: {err}."), colors::WARN));
                body.push(line(
                    if setup.latency_only {
                        "Press Space to measure again, or Esc to close."
                    } else {
                        "Press Space to measure again, or Enter to finish without it."
                    }
                    .to_owned(),
                    colors::DIM,
                ));
            }
            None => body.push(line(
                "Hold your headphones against the microphone, press Space: beeps play for about 15 seconds."
                    .to_owned(),
                colors::TEXT,
            )),
        }
        body
    }
}
