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
//! ticks; macOS gives a reconnected device a new object). A lookup that
//! fails says nothing about the device: it is tried again for
//! [`LOOKUP_GRACE_S`] before it counts as a drop, and meanwhile nothing is
//! set again, stopped or cancelled, and a start waits. The route is set
//! again only when it must be: the configured one differs from the applied
//! one, or the applied device dropped, or setting it failed.
//!
//! Each tick does a constant number of single CoreAudio property reads
//! (the device's object, the default output's object); listing devices,
//! names and UIDs is left to starts, device changes
//! ([`Media::device_generation`]) and a poll every two seconds. Right
//! before a playback or a take's film is scheduled, and until the take's
//! film is seen playing, the device is checked again. A playback whose
//! route drops stops; a take goes on, watching the device it started on
//! (or, on the system output, which device that is), setting it again
//! after every drop, and its result says once that the film may have
//! played elsewhere.
//!
//! [`Media::output_connected`]: super::Media::output_connected
//! [`Media::device_generation`]: super::Media::device_generation

use takewise_media::DeviceLookup;

use super::RecordingWindow;
use super::recorder::StartHold;

/// How long a failing device lookup is tried again before it counts as a
/// drop.
pub const LOOKUP_GRACE_S: f64 = 0.5;

/// What a device lookup says against the object it was seen as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Seen {
    /// The same object: nothing happened.
    Same,
    /// Gone (`None`), or another object: it went away and perhaps came
    /// back.
    Dropped(Option<u32>),
    /// The lookup failed, within [`LOOKUP_GRACE_S`]: not known yet.
    Unsure,
}

/// Why the configured output cannot be used now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputProblem {
    /// The device is not connected.
    Missing(String),
    /// It is connected, but the players could not be set to it.
    Failed { name: String, error: String },
    /// Its lookup failed just now: not known to be there yet.
    Unconfirmed(String),
}

impl OutputProblem {
    /// What P and Shift+P say.
    pub fn for_playback(&self) -> String {
        match self {
            Self::Missing(name) => {
                format!("{name} is not connected: connect it or pick another output (A).")
            }
            Self::Failed { name, error } => format!("Cannot play through {name}: {error}."),
            Self::Unconfirmed(name) => {
                format!("{name} could not be checked just now; press it again.")
            }
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
            Self::Unconfirmed(name) => {
                format!(
                    "{name} could not be checked just now, so Takewise does not record yet; try again."
                )
            }
        }
    }
}

impl RecordingWindow {
    /// One lookup of the device `uid`, seen before as `object`. A failed
    /// read counts as a drop only once reads failed for
    /// [`LOOKUP_GRACE_S`].
    pub(super) fn see(&mut self, uid: &str, object: u32) -> Seen {
        self.seen_lookup(self.media.output_object(uid), object)
    }

    pub(super) fn seen_lookup(&mut self, lookup: DeviceLookup, object: u32) -> Seen {
        match lookup {
            DeviceLookup::Present(now) => {
                self.lookup_failed_since = None;
                if now == object {
                    Seen::Same
                } else {
                    Seen::Dropped(Some(now))
                }
            }
            DeviceLookup::Absent => {
                self.lookup_failed_since = None;
                Seen::Dropped(None)
            }
            DeviceLookup::Unknown => {
                let now = self.media.host_now_s();
                let since = *self.lookup_failed_since.get_or_insert(now);
                if now - since >= LOOKUP_GRACE_S {
                    Seen::Dropped(None)
                } else {
                    Seen::Unsure
                }
            }
        }
    }

    /// Puts the players on the configured route: the configured device, or
    /// the system output when none is configured; set only when it must be
    /// (see the module). Whether the applied device dropped is always
    /// checked, with one property read; with `recheck`, also whether the
    /// configured device is there by name (a start, a device change, the
    /// poll). A missing or unconfirmed device leaves the players alone and
    /// is said in the error; a route that could not be set stays due.
    pub(super) fn sync_output(&mut self, recheck: bool) -> Result<(), OutputProblem> {
        if let (Some(name), Some((uid, object))) =
            (self.film_output.clone(), self.applied_device.clone())
        {
            match self.see(&uid, object) {
                Seen::Same => {}
                Seen::Dropped(_) => self.route_due = true,
                Seen::Unsure => return Err(OutputProblem::Unconfirmed(name)),
            }
        } else if self.film_output.is_some() {
            // Set, but its object never read: not known.
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
            self.set_route(&name)?;
        }
        self.output_missing = None;
        self.output_error = None;
        Ok(())
    }

    /// Sets the players to the device `name` and reads which object it is.
    /// Not confirmed (gone again, or the read failed): the route stays due,
    /// and nothing may start on it.
    fn set_route(&mut self, name: &str) -> Result<(), OutputProblem> {
        if let Err(error) = self.media.set_output(Some(name)) {
            self.output_error = Some(error.clone());
            self.route_due = true;
            return Err(OutputProblem::Failed {
                name: name.to_owned(),
                error,
            });
        }
        self.film_output = Some(name.to_owned());
        self.route_due = true;
        self.applied_device = None;
        let uid = self.media.applied_output_uid();
        match uid.as_deref().map(|uid| self.media.output_object(uid)) {
            Some(DeviceLookup::Present(object)) => {
                self.applied_device = uid.map(|uid| (uid, object));
                self.route_due = false;
                Ok(())
            }
            Some(DeviceLookup::Unknown) => Err(OutputProblem::Unconfirmed(name.to_owned())),
            _ => {
                self.output_missing = Some(name.to_owned());
                Err(OutputProblem::Missing(name.to_owned()))
            }
        }
    }

    /// Before P, Shift+P or a take: the route checked and set as needed. A
    /// device that could not be looked up just now lets it begin: its
    /// start waits (see [`RecordingWindow::hold_start`]) until the device
    /// is confirmed, or counts as gone.
    pub(super) fn route_for_start(&mut self) -> Result<(), OutputProblem> {
        match self.sync_output(true) {
            Err(OutputProblem::Unconfirmed(_)) => Ok(()),
            other => other,
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
    /// playback whose route can no longer be used, but not on a lookup that
    /// failed once.
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
            Err(OutputProblem::Unconfirmed(_)) => {}
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
    /// route; a take checks the route it started on every tick, whatever
    /// `[output] device` says meanwhile.
    pub(super) fn watch_output(&mut self, now: f64) {
        self.cancel_calibration_if_output_dropped();
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
            self.check_take_route(changed || due);
        } else if changed || due {
            self.follow_output();
        }
    }

    /// When a take starts: the route it plays the film through, to watch.
    pub(super) fn snapshot_take_route(&mut self) {
        self.take_route = if let Some(name) = &self.film_output {
            Some(TakeRoute {
                name: name.clone(),
                uid: self.applied_device.as_ref().map(|(uid, _)| uid.clone()),
                object: self.applied_device.as_ref().map(|(_, object)| *object),
                system: false,
                tried: None,
            })
        } else {
            let object = match self.media.default_output_object() {
                DeviceLookup::Present(object) => Some(object),
                DeviceLookup::Absent | DeviceLookup::Unknown => None,
            };
            // Its name and UID are read once, here.
            self.media
                .default_output_uid()
                .map(|(name, uid)| TakeRoute {
                    name,
                    uid: Some(uid),
                    object,
                    system: true,
                    tried: None,
                })
        };
    }

    /// Checks the route a take started on, every tick (one property read)
    /// and as it stops. Once the take's film was seen playing, a drop of its
    /// device, or on the system output another default output, is noted
    /// on the take (said once with its result), and after every drop the
    /// route is set again as soon as the device is back: at once when it is
    /// seen back, else tried again on a device change or the poll (`event`).
    pub(super) fn check_take_route(&mut self, event: bool) {
        let Some(route) = self.take_route.clone() else {
            return;
        };
        if route.system {
            self.check_system_route(&route);
            return;
        }
        let (Some(uid), Some(object)) = (route.uid.clone(), route.object) else {
            return;
        };
        let back = match self.see(&uid, object) {
            Seen::Same | Seen::Unsure => return,
            Seen::Dropped(back) => back,
        };
        let note = format!(
            "{} was disconnected during the take, so the film may have played elsewhere, out loud: listen before you choose it.",
            route.name
        );
        if !self.recorder.note_route(note) {
            // Its film has not been seen playing: the start is held instead.
            return;
        }
        // Set again after the take at the latest.
        self.route_due = true;
        let configured = self.project.config.output.device.as_ref();
        if configured != Some(&route.name) {
            return;
        }
        // Back as another object: set it again at once (the take is noted,
        // so a glitch costs nothing), once per object unless devices changed.
        let Some(back) = back else {
            return;
        };
        if route.tried == Some(back) && !event {
            return;
        }
        if let Some(route) = &mut self.take_route {
            route.tried = Some(back);
        }
        if self.set_route(&route.name).is_ok()
            && let Some(route) = &mut self.take_route
        {
            route.uid = self.applied_device.as_ref().map(|(uid, _)| uid.clone());
            route.object = self.applied_device.as_ref().map(|(_, object)| *object);
        }
    }

    /// A take on the system output: the default output's object is compared
    /// every tick (one property read), its name looked up only when it
    /// changed.
    fn check_system_route(&mut self, route: &TakeRoute) {
        let lookup = self.media.default_output_object();
        let Some(object) = route.object else {
            // Not read when the take started: from the first read on.
            if let (DeviceLookup::Present(object), Some(route)) = (lookup, &mut self.take_route) {
                route.object = Some(object);
            }
            return;
        };
        let now = match self.seen_lookup(lookup, object) {
            Seen::Same | Seen::Unsure => return,
            Seen::Dropped(now) => now,
        };
        let other = now
            .and_then(|_| self.media.default_output_uid())
            .filter(|(_, uid)| route.uid.as_ref() != Some(uid));
        let note = match &other {
            Some((name, _)) => format!(
                "The system output changed from {} to {name} during the take: listen before you choose it.",
                route.name
            ),
            None => format!(
                "The system output ({}) dropped out during the take: listen before you choose it.",
                route.name
            ),
        };
        if self.recorder.note_route(note)
            && let Some(route) = &mut self.take_route
        {
            // Followed from here, so a later change is seen too.
            route.object = now;
            if let Some((name, uid)) = other {
                route.name = name;
                route.uid = Some(uid);
            }
        }
    }

    /// Each tick before the recorder starts a playback or a take's film, or
    /// while the take's film was not yet seen playing: whether it may start.
    /// The applied device is looked up right now (one property read),
    /// whatever the devices are said to have done: a playback follows the
    /// configured route (set again when the device dropped and is back); a
    /// take checks the device it started on. A lookup that failed makes it
    /// wait, not stop.
    pub(super) fn hold_start(&mut self) {
        let now = self.media.host_now_s();
        let hold = if self.recorder.playback_waiting(now) {
            match self.sync_output(false) {
                Ok(()) => StartHold::Go,
                Err(OutputProblem::Unconfirmed(_)) => StartHold::Wait,
                Err(problem) => {
                    StartHold::Stop(format!("The playback stopped. {}", problem.for_playback()))
                }
            }
        } else if self.recorder.film_waiting() {
            self.take_start_hold()
        } else {
            StartHold::Go
        };
        self.recorder.hold_start(hold);
    }

    fn take_start_hold(&mut self) -> StartHold {
        let Some(route) = self.take_route.clone().filter(|route| !route.system) else {
            return StartHold::Go;
        };
        let (Some(uid), Some(object)) = (route.uid, route.object) else {
            return StartHold::Go;
        };
        match self.see(&uid, object) {
            Seen::Same => StartHold::Go,
            Seen::Unsure => StartHold::Wait,
            Seen::Dropped(Some(_)) => {
                StartHold::Stop(format!("{} dropped out; start the take again.", route.name))
            }
            Seen::Dropped(None) => {
                StartHold::Stop(OutputProblem::Missing(route.name).for_recording())
            }
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

/// The output a take started on: its name, the device's UID and CoreAudio
/// object (each `None` when it could not be read), whether it is the
/// system output (whose device can change during the take), and the object
/// it was last set again to after a drop. A reload that changes
/// `[output] device` does not change it.
#[derive(Debug, Clone)]
pub struct TakeRoute {
    name: String,
    uid: Option<String>,
    object: Option<u32>,
    system: bool,
    tried: Option<u32>,
}
