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
use takewise_core::{CalibrationInfo, Route, SetupInfo, calibration_stale};

use super::RecordingWindow;

/// What the panel adds for a latency in use on a Bluetooth output,
/// whether it was copied from an earlier measurement or measured here.
pub const BLUETOOTH_HINT: &str = "Bluetooth delay can change after reconnecting; press L to measure it again (15 s) if takes sound late.";

impl RecordingWindow {
    /// Uses the settings file `settings` (none: no defaults, nothing
    /// remembered, no rescue copies): device choices are saved there as
    /// defaults for new projects, the remembered setup beside it applies
    /// to this project now, as when the window opens, and audio that
    /// cannot be saved into the project when the window closes is rescued
    /// into `rescue/` beside it.
    pub(super) fn use_settings(&mut self, settings: Option<PathBuf>) {
        self.remembered_path = settings.as_deref().map(remembered::beside);
        self.rescue_root = settings.as_deref().map(takewise_core::rescue::beside);
        self.defaults_path = settings;
        self.remembered_for = None;
        self.check_inputs();
        self.note_rescued();
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

    /// What the panel says about the latency in use on `route`: that it
    /// was copied from an earlier measurement on these devices, when it
    /// was, and with a Bluetooth output that it may need measuring again.
    /// The headphones can reconnect after a measurement made here as
    /// well, so that hint does not depend on where the latency came from.
    pub(super) fn remembered_note(&self, route: &Route) -> Option<String> {
        let config = &self.project.config;
        let copied = remembered::latency_note(config, route).map(|note| {
            let mut chars = note.chars();
            let first = chars.next().map(|c| c.to_uppercase().to_string());
            format!("{}{}.", first.unwrap_or_default(), chars.as_str())
        });
        let in_use = config.calibration.is_some() && calibration_stale(config, route).is_none();
        let hint = (in_use && self.bluetooth_output()).then(|| BLUETOOTH_HINT.to_owned());
        let notes: Vec<String> = copied.into_iter().chain(hint).collect();
        (!notes.is_empty()).then(|| notes.join(" "))
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
