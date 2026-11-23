//! The setup check's latency step: the loopback calibration of
//! `takewise calibrate`, run in the window with the window's media and the
//! input capture its meter already holds. The beep film replaces the
//! project's film while it plays; the project's film comes back after.
//! A consistent measurement is stored with the devices it was measured on;
//! anything else is shown and nothing is written.

use std::path::{Path, PathBuf};

use gpui_kit::AnyElement;
use takewise_core::{self as tw, CalibrationInfo};

use super::setup::{Setup, calibration_needed, line};
use super::{RecordingWindow, colors};
use crate::calibrate::{self, Measurement, Run};

/// Makes the beep film in a directory: [`calibrate::make_beep_film`], or a
/// stand-in in tests.
pub type BeepFilm = fn(&Path) -> Result<PathBuf, String>;

/// A calibration while it runs.
pub struct Calibrating {
    run: Run,
    /// Holds the beep film until the run ends.
    _work: tempfile::TempDir,
}

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
        let route = self.current_route();
        let device = route.input.ok_or("No input device chosen.")?;
        let output = route.output.unwrap_or_else(|| "unknown".to_owned());
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
                self.calibrating = Some(Calibrating { run, _work: work });
                Ok(())
            }
            Err(err) => {
                self.recorder.stop_tap();
                self.restore_film();
                Err(err)
            }
        }
    }

    /// Once per frame while a calibration runs: hands it the meter's
    /// buffers; stores the result once it is done.
    pub(super) fn tick_calibration(&mut self) {
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
        if let Some(setup) = &mut self.setup {
            setup.latency = Some(outcome);
        }
    }

    /// Esc, or closing the setup check: stops a calibration in progress
    /// without writing anything.
    pub(super) fn cancel_calibration(&mut self) {
        if self.calibrating.is_some() {
            self.end_calibration();
        }
    }

    fn end_calibration(&mut self) {
        self.calibrating = None;
        self.recorder.stop_tap();
        self.media.film_pause();
        self.restore_film();
    }

    /// Puts the project's film back, at its volume and parked where it was;
    /// it warms up again before the next take.
    fn restore_film(&mut self) {
        if let Err(err) = self.media.film_open(&self.project.film_path()) {
            self.message = Some(format!("Could not reopen the film: {err}"));
        }
        self.media.film_set_gain_db(self.film_gain_db);
        self.media.film_set_muted(false);
        self.recorder.rewarm();
        self.park();
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
            date: jiff::ZonedDateTime::now().date().to_string(),
        };
        tw::set_calibration(&self.dir, offset, &info)
            .map_err(|err| format!("{placed}, but it could not be saved: {err}"))?;
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
        if let Some(why) = calibration_needed(&self.project.config, &self.current_route()) {
            body.push(line(format!("Latency: {why}."), colors::WARN));
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
                    "Press Space to measure again, or Enter to finish without it.".to_owned(),
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
