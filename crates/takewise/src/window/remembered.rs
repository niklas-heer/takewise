//! The remembered setup in the window (see `takewise_core::remembered`):
//! when the window opens, and whenever the route in use or the day
//! changes, a project without a fitting latency calibration gets the one
//! remembered for exactly these devices and buffer, and a project without
//! today's setup check for these devices adopts today's from another
//! project. Measurements and setup checks made here are remembered for the
//! next project. All of it is a convenience: failing to read or write the
//! remembered setup never stops recording.

use std::path::PathBuf;

use takewise_core::remembered::{self, LatencySync};
use takewise_core::room::RoomTone;
use takewise_core::{CalibrationInfo, Route, SetupInfo};

use super::RecordingWindow;

/// What the panel adds for a remembered latency on a Bluetooth output.
pub const BLUETOOTH_HINT: &str = "Bluetooth delay can change after reconnecting; press C to measure again (15 s) if takes sound late.";

impl RecordingWindow {
    /// Uses the settings file `settings` (none: no defaults, nothing
    /// remembered): device choices are saved there as defaults for new
    /// projects, and the remembered setup beside it applies to this
    /// project now, as when the window opens.
    pub(super) fn use_settings(&mut self, settings: Option<PathBuf>) {
        self.remembered_path = settings.as_deref().map(remembered::beside);
        self.defaults_path = settings;
        self.remembered_for = None;
        self.check_inputs();
    }

    /// Between takes, on the route in use: applies the remembered latency
    /// and adopts today's remembered setup check, each only where the
    /// project has none of its own that fits. Runs once per route and day,
    /// and only while the input is connected.
    pub(super) fn use_remembered(&mut self) {
        let Some(path) = self.remembered_path.clone() else {
            return;
        };
        if self.recorder.is_recording() {
            return;
        }
        #[cfg(test)]
        super::start::isolated::assert_temporary(&path);
        let route = self.current_route();
        let connected = |name: &str| self.inputs.iter().any(|input| input.name == name);
        if !route.input.as_deref().is_some_and(connected) {
            return;
        }
        let today = jiff::ZonedDateTime::now().date();
        let key = (route.clone(), today);
        if self.remembered_for.as_ref() == Some(&key) {
            return;
        }
        self.remembered_for = Some(key);

        let mut used = Vec::new();
        let mut failed = Vec::new();
        match remembered::sync_latency(&path, &self.dir, &self.project.config, &route) {
            Ok(LatencySync::Applied(latency)) => used.push(format!(
                "the latency measured on these devices on {}",
                latency.date
            )),
            Ok(LatencySync::Unchanged) => {}
            Err(err) => failed.push(format!("the remembered latency: {err}")),
        }
        match remembered::adopt_setup(&path, &self.dir, &self.project.config, &route, today) {
            Ok(Some(_)) => used.push("today's setup check from another project".to_owned()),
            Ok(None) => {}
            Err(err) => failed.push(format!("the remembered setup check: {err}")),
        }
        if !used.is_empty() {
            self.reload();
        }
        if self.message.is_none() {
            if !failed.is_empty() {
                self.message = Some(format!("Could not use {}.", failed.join("; ")));
            } else if !used.is_empty() {
                self.message = Some(format!("Using {}.", used.join(" and ")));
            }
        }
    }

    /// What the panel says when the latency in use was copied from an
    /// earlier measurement on these devices; with a Bluetooth output, also
    /// that it may need measuring again.
    pub(super) fn remembered_note(&self, route: &Route) -> Option<String> {
        let note = remembered::latency_note(&self.project.config, route)?;
        let mut chars = note.chars();
        let first = chars.next().map(|c| c.to_uppercase().to_string());
        let mut text = format!("{}{}.", first.unwrap_or_default(), chars.as_str());
        if self.bluetooth_output() {
            text.push(' ');
            text.push_str(BLUETOOTH_HINT);
        }
        Some(text)
    }

    /// Whether the setup check offers its latency step although the
    /// calibration fits: it was copied from an earlier measurement, and
    /// the output is Bluetooth, whose delay can change.
    pub(super) fn remeasure_offered(&self, route: &Route) -> bool {
        remembered::latency_note(&self.project.config, route).is_some() && self.bluetooth_output()
    }

    /// Remembers a measurement just stored, quietly.
    pub(super) fn remember_latency(&self, offset_ms: i32, info: &CalibrationInfo) {
        if let Some(path) = &self.remembered_path {
            #[cfg(test)]
            super::start::isolated::assert_temporary(path);
            let _ = remembered::remember_latency(path, offset_ms, info);
        }
    }

    /// Remembers a setup check just saved, with its room tone, quietly.
    pub(super) fn remember_setup(
        &self,
        info: &SetupInfo,
        tone: &RoomTone,
        today: jiff::civil::Date,
    ) {
        if let Some(path) = &self.remembered_path {
            #[cfg(test)]
            super::start::isolated::assert_temporary(path);
            let _ = remembered::remember_setup(path, &self.dir, info, tone, today);
        }
    }
}
