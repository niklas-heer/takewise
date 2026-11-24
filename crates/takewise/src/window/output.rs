//! Where the film and the takes play: the output configured in
//! `[output] device`, or the system's default output when none is.
//!
//! The window keeps apart what the players are set to (the applied route,
//! `film_output`) and whether the configured device is there now
//! ([`Media::output_connected`]). A configured device that is missing is
//! never replaced by the system output: playback and recording refuse until
//! it is back. The route is set again only when it must be: when the
//! configured one differs from the applied one, or the devices changed
//! ([`Media::device_generation`]) or the device was seen missing since it
//! was last set, since the players may have fallen back to the default.
//!
//! Device changes are acted on at the next tick, with a poll every two
//! seconds as the fallback: a route that becomes unusable stops a playback;
//! a take goes on, watching the very device it started on, and its result
//! says that the film may have played elsewhere.
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
    /// the system output when none is configured. The route is set only
    /// when it must be (see the module). With `recheck`, the device's
    /// presence is looked up even when nothing seems to have changed (a
    /// start, a device change, the poll); without, nothing is looked up
    /// while the devices stayed as they were (each frame before a start).
    /// A missing device leaves the players alone and is said in the error;
    /// a route that could not be set stays due to be set again.
    pub(super) fn sync_output(&mut self, recheck: bool) -> Result<(), OutputProblem> {
        let generation = self.media.device_generation();
        if generation != self.devices_seen {
            self.devices_seen = generation;
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
                return Err(OutputProblem::Failed { name, error });
            }
            self.film_output = Some(name);
            self.route_due = false;
        }
        self.output_missing = None;
        self.output_error = None;
        Ok(())
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

    /// Once per tick: acts on a device change at once, and while a
    /// playback or a take runs also every two seconds (should a change go
    /// unreported). A playback follows the configured route; a take
    /// watches the device it started on, whatever `[output] device` says
    /// meanwhile, and notes its loss on the take.
    pub(super) fn watch_output(&mut self, now: f64) {
        let recording = self.recorder.is_recording();
        if !recording {
            self.take_route = None;
        }
        let generation = self.media.device_generation();
        let changed = generation != self.devices_watched;
        let busy = recording || self.recorder.is_playing();
        let due = busy && now >= self.output_checked_at + super::INPUT_CHECK_S;
        if !(changed || due) {
            return;
        }
        self.devices_watched = generation;
        if due {
            self.output_checked_at = now;
        }
        if !recording {
            self.follow_output();
        } else if let Some(name) = self.take_route_lost() {
            self.recorder.note_route_lost(&name);
        }
    }

    /// The device a take started on, when it is gone: its name.
    fn take_route_lost(&self) -> Option<String> {
        let route = self.take_route.as_ref()?;
        let uid = route.uid.as_deref()?;
        (!self.media.output_uid_connected(uid)).then(|| route.name.clone())
    }

    /// When a take starts: the route it plays the film through, to watch.
    pub(super) fn snapshot_take_route(&mut self) {
        self.take_route = self.film_output.clone().map(|name| TakeRoute {
            name,
            uid: self.media.applied_output_uid(),
        });
    }

    /// Each tick before the recorder starts a playback or a take's film:
    /// why it must not start, if the route can no longer be used. A
    /// playback follows the configured route (set again when the device is
    /// back); a take checks the device it started on.
    pub(super) fn hold_start(&mut self) {
        let problem = if self.recorder.playback_waiting() {
            self.sync_output(false)
                .err()
                .map(|problem| format!("The playback stopped. {}", problem.for_playback()))
        } else if self.recorder.film_waiting() {
            self.take_route_lost()
                .map(|name| OutputProblem::Missing(name).for_recording())
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

/// The output a take started on: its name, and the UID of the device
/// (`None` when unknown), so a reload that changes `[output] device` does
/// not change what the take watches.
#[derive(Debug, Clone)]
pub struct TakeRoute {
    name: String,
    uid: Option<String>,
}
