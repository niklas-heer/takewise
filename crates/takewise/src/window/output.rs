//! Where the film and the takes play: the output configured in
//! `[output] device`, or the system's default output when none is.
//!
//! The window keeps apart what the players are set to (the applied route,
//! `film_output`) and whether the configured device is there now
//! ([`Media::output_connected`]). A configured device that is missing is
//! never replaced by the system output: playback and recording refuse until
//! it is back, and the players are moved to it again once it is, since they
//! may have fallen back to the default while it was gone. A route that
//! becomes unusable stops a playback; a take goes on, and says afterwards
//! that the film may have played elsewhere.
//!
//! [`Media::output_connected`]: super::Media::output_connected

use super::RecordingWindow;
use super::recorder;

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
    /// the system output when none is configured. With `force` (before a
    /// playback, a take or a latency measurement) the route is set again
    /// even when it seems unchanged, so a device that dropped out and came
    /// back between checks is used again rather than the default the
    /// players fell back to. A missing device leaves the players alone and
    /// is said in the error.
    pub(super) fn sync_output(&mut self, force: bool) -> Result<(), OutputProblem> {
        let Some(name) = self.project.config.output.device.clone() else {
            self.output_missing = None;
            self.output_error = None;
            if force || self.film_output.is_some() {
                self.media
                    .set_output(None)
                    .map_err(|error| OutputProblem::Failed {
                        name: "the system output".to_owned(),
                        error,
                    })?;
                self.film_output = None;
            }
            return Ok(());
        };
        if !self.media.output_connected(&name) {
            self.output_error = None;
            self.output_missing = Some(name.clone());
            return Err(OutputProblem::Missing(name));
        }
        let returned = self.output_missing.take().is_some();
        if force || returned || self.film_output.as_deref() != Some(&name) {
            if let Err(error) = self.media.set_output(Some(&name)) {
                self.output_error = Some(error.clone());
                return Err(OutputProblem::Failed { name, error });
            }
            self.film_output = Some(name);
        }
        self.output_error = None;
        Ok(())
    }

    /// When the window opens: puts the players on the configured route,
    /// and says so when its device is missing.
    pub(super) fn apply_output(&mut self) {
        if let Err(problem) = self.sync_output(false) {
            self.message = Some(problem.for_playback());
        }
    }

    /// Between takes (the device check, a reload): follows the configured
    /// route. Says when a missing output came back, unless the window is
    /// saying something else; stops a playback whose route can no longer
    /// be used.
    pub(super) fn follow_output(&mut self) {
        let was_missing = self.output_missing.clone();
        match self.sync_output(false) {
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

    /// Every two seconds while a playback or a take runs: a playback whose
    /// route can no longer be used stops; a take goes on, and notes that its
    /// output was lost, to say so once it ends.
    pub(super) fn watch_output(&mut self, now: f64) {
        let recording = self.recorder.is_recording();
        if !(recording || self.recorder.is_playing())
            || now < self.output_checked_at + super::INPUT_CHECK_S
        {
            return;
        }
        self.output_checked_at = now;
        if !recording {
            self.follow_output();
            return;
        }
        if let Some(name) = self.project.config.output.device.clone()
            && !self.media.output_connected(&name)
        {
            self.take_output_lost.get_or_insert(name);
        }
    }

    /// Once a take ended: says when its output was lost while it recorded.
    pub(super) fn note_lost_output(&mut self) {
        if self.recorder.is_recording() {
            return;
        }
        if let Some(name) = self.take_output_lost.take() {
            let note = format!(
                "{name} was disconnected during the take, so the film may have played elsewhere, out loud: listen before you choose it."
            );
            self.message = recorder::join(self.message.take(), Some(note));
        }
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
