//! Where the film and the takes play: the output configured in
//! `[output] device`, or the system's default output when none is.
//!
//! The window keeps apart what the players are set to (the applied route,
//! `film_output`, and the very device: its UID and CoreAudio object) and
//! whether the configured device is there now ([`Media::output_connected`]).
//! A configured device that is missing is never replaced by the system
//! output: playback and recording refuse until it is back.
//!
//! The applied device has *dropped* when it is gone or has another object
//! than when it was set (it went away and came back, perhaps between two
//! ticks: the players may have fallen back to the default meanwhile). The
//! route is set again only when it must be: the configured one differs
//! from the applied one, or the applied device dropped, or setting it
//! failed. Other device changes leave a playing film alone.
//!
//! Device changes ([`Media::device_generation`]) are acted on at the next
//! tick, with a poll every two seconds as the fallback, and right before a
//! playback or a take's film is scheduled the applied device is checked
//! once more. A playback whose route drops stops; a take goes on, watching
//! the device it started on (or, on the system output, which device that
//! is), and its result says that the film may have played elsewhere.
//!
//! [`Media::output_connected`]: super::Media::output_connected
//! [`Media::device_generation`]: super::Media::device_generation

use super::RecordingWindow;

/// Why the configured output cannot be used now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputProblem {
    /// The device is not connected.
    Missing(String),
    /// It is connected, but the players could not be set to it.
    Failed { name: String, error: String },
}

impl OutputProblem {
    /// What P and Shift+P say.
    pub fn for_playback(&self) -> String {
        match self {
            Self::Missing(name) => {
                format!("{name} is not connected: connect it or pick another output (A).")
            }
            Self::Failed { name, error } => format!("Cannot play through {name}: {error}."),
        }
    }

    /// What a take refused for it says.
    pub fn for_recording(&self) -> String {
        match self {
            Self::Missing(name) => format!(
                "{name} is not connected, so Takewise does not record: the film would play out loud into your microphone. Connect it, or pick another output, or System output, in the audio panel (A)."
            ),
            Self::Failed { name, error } => format!(
                "Cannot play through {name} ({error}), so Takewise does not record. Pick another output, or System output, in the audio panel (A)."
            ),
        }
    }
}

impl RecordingWindow {
    /// Puts the players on the configured route: the configured device, or
    /// the system output when none is configured; set only when it must be
    /// (see the module). Whether the applied device dropped is always
    /// checked; with `recheck`, also whether the configured device is there
    /// by name (a start, a device change, the poll), without, not while
    /// the applied device is in place. A missing device leaves the players
    /// alone and is said in the error; a route that could not be set stays
    /// due to be set again.
    pub(super) fn sync_output(&mut self, recheck: bool) -> Result<(), OutputProblem> {
        if self.applied_dropped() {
            self.route_due = true;
        }
        let Some(name) = self.project.config.output.device.clone() else {
            self.output_missing = None;
            self.output_error = None;
            if self.route_due || self.film_output.is_some() {
                self.media
                    .set_output(None)
                    .map_err(|error| OutputProblem::Failed {
                        name: "the system output".to_owned(),
                        error,
                    })?;
                self.film_output = None;
                self.applied_device = None;
                self.route_due = false;
            }
            return Ok(());
        };
        let applied = self.film_output.as_deref() == Some(&name);
        if !recheck && !self.route_due && applied {
            return Ok(());
        }
        if !self.media.output_connected(&name) {
            self.output_error = None;
            self.output_missing = Some(name.clone());
            self.route_due = true;
            return Err(OutputProblem::Missing(name));
        }
        if self.route_due || !applied {
            if let Err(error) = self.media.set_output(Some(&name)) {
                self.output_error = Some(error.clone());
                self.route_due = true;
                return Err(OutputProblem::Failed { name, error });
            }
            self.film_output = Some(name);
            self.applied_device = self.media.applied_output_uid().and_then(|uid| {
                let object = self.media.output_object(&uid)?;
                Some((uid, object))
            });
            self.route_due = false;
        }
        self.output_missing = None;
        self.output_error = None;
        Ok(())
    }

    /// Whether the device the players were set to is gone, or is another
    /// CoreAudio object than then (it went away and came back).
    fn applied_dropped(&self) -> bool {
        match (&self.film_output, &self.applied_device) {
            (Some(_), Some((uid, object))) => self.media.output_object(uid) != Some(*object),
            // Set, but its object could not be read then: not known.
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// When the window opens: puts the players on the configured route,
    /// and says so when its device is missing.
    pub(super) fn apply_output(&mut self) {
        if let Err(problem) = self.sync_output(true) {
            self.message = Some(problem.for_playback());
        }
    }

    /// Between takes and during a playback (a device change, the poll, a
    /// reload): follows the configured route. Says when a missing output
    /// came back, unless the window is saying something else; stops a
    /// playback whose route can no longer be used.
    pub(super) fn follow_output(&mut self) {
        let was_missing = self.output_missing.clone();
        match self.sync_output(true) {
            Ok(()) => {
                let back = was_missing.filter(|name| self.film_output.as_ref() == Some(name));
                if let Some(name) = back {
                    // Only in place of nothing, or of what was said about
                    // this output.
                    let quiet = self
                        .message
                        .as_ref()
                        .is_none_or(|said| said.contains(&name));
                    if quiet {
                        self.message = Some(format!(
                            "{name} is connected: the film and your takes play there."
                        ));
                    }
                }
            }
            Err(problem) => {
                if self.recorder.is_playing() {
                    self.recorder.stop_playback(&mut *self.media);
                    self.message =
                        Some(format!("The playback stopped. {}", problem.for_playback()));
                }
            }
        }
    }

    /// Once per tick, before the recorder's: acts on a device change at
    /// once, and while a playback or a take runs also every two seconds
    /// (should a change go unreported). A playback follows the configured
    /// route; a take checks the route it started on, whatever
    /// `[output] device` says meanwhile.
    pub(super) fn watch_output(&mut self, now: f64) {
        let recording = self.recorder.is_recording();
        if !recording {
            self.take_route = None;
        }
        let generation = self.media.device_generation();
        let changed = generation != self.devices_watched;
        let busy = recording || self.recorder.is_playing();
        let due = busy && now >= self.output_checked_at + super::INPUT_CHECK_S;
        if changed {
            self.devices_watched = generation;
        }
        if due {
            self.output_checked_at = now;
        }
        if recording {
            // The device's object is cheap to read: every tick, so a take
            // ending now has it checked. The system output's device only
            // on a change or the poll.
            self.check_take_route(changed || due);
        } else if changed || due {
            self.follow_output();
        }
    }

    /// When a take starts: the route it plays the film through, to watch.
    pub(super) fn snapshot_take_route(&mut self) {
        self.take_route = match &self.film_output {
            Some(name) => Some(TakeRoute {
                name: name.clone(),
                device: self.applied_device.clone(),
                system: false,
                noted: false,
            }),
            None => self.media.default_output_uid().map(|(name, uid)| {
                let object = self.media.output_object(&uid);
                TakeRoute {
                    name,
                    device: object.map(|object| (uid, object)),
                    system: true,
                    noted: false,
                }
            }),
        };
    }

    /// Checks the route a take started on: a device that dropped (gone, or
    /// back as another object), or on the system output another default
    /// output (with `full`: it costs a little more), is noted on the take
    /// once its film started, and the route is set again once the take
    /// ended.
    pub(super) fn check_take_route(&mut self, full: bool) {
        let Some(route) = &self.take_route else {
            return;
        };
        if route.noted {
            return;
        }
        let dropped = route
            .device
            .as_ref()
            .is_some_and(|(uid, object)| self.media.output_object(uid) != Some(*object));
        let note = if route.system {
            let now = full.then(|| self.media.default_output_uid()).flatten();
            let other =
                now.filter(|(_, uid)| route.device.as_ref().is_none_or(|(was, _)| was != uid));
            match other {
                Some((name, _)) => Some(format!(
                    "The system output changed to {name} during the take: the film may have played out loud; listen before you choose it."
                )),
                None => dropped.then(|| format!(
                    "The system output ({}) dropped out during the take: the film may have played out loud; listen before you choose it.",
                    route.name
                )),
            }
        } else {
            dropped.then(|| format!(
                "{} was disconnected during the take, so the film may have played elsewhere, out loud: listen before you choose it.",
                route.name
            ))
        };
        if let Some(note) = note
            && self.recorder.note_route(note)
        {
            if let Some(route) = &mut self.take_route {
                route.noted = true;
            }
            // Set again once the take ended, not during it.
            self.route_due = true;
        }
    }

    /// Each tick before the recorder starts a playback or a take's film:
    /// why it must not start, if the route can no longer be used. The
    /// applied device is looked up right now, whatever the devices are
    /// said to have done: a playback follows the configured route (set
    /// again when the device dropped and is back); a take checks the device
    /// it started on.
    pub(super) fn hold_start(&mut self) {
        let problem = if self.recorder.playback_waiting() {
            self.sync_output(false)
                .err()
                .map(|problem| format!("The playback stopped. {}", problem.for_playback()))
        } else if self.recorder.film_waiting() {
            self.take_route
                .as_ref()
                .filter(|route| !route.system)
                .filter(|route| {
                    route
                        .device
                        .as_ref()
                        .is_none_or(|(uid, object)| self.media.output_object(uid) != Some(*object))
                })
                .map(|route| OutputProblem::Missing(route.name.clone()).for_recording())
        } else {
            None
        };
        self.recorder.hold_start(problem);
    }

    /// What the panel says under the output when the configured one is not
    /// in use.
    pub(super) fn output_note(&self) -> Option<String> {
        let configured = self.project.config.output.device.as_ref()?;
        if self.output_missing.as_ref() == Some(configured) {
            return Some(format!(
                "{configured} is not connected: nothing plays until it is, or pick another output."
            ));
        }
        self.output_error
            .as_ref()
            .map(|error| format!("{configured} could not be used: {error}"))
    }
}

/// The output a take started on: its name, the device (UID and CoreAudio
/// object; `None` when unknown), whether it is the system output (whose
/// device can change during the take), and whether its loss was noted.
/// A reload that changes `[output] device` does not change it.
#[derive(Debug, Clone)]
pub struct TakeRoute {
    name: String,
    device: Option<(String, u32)>,
    system: bool,
    noted: bool,
}
