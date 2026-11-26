//! Where the film and the takes play: the output configured in
//! `[output] device`, or the system's default output when none is.
//!
//! The window keeps apart what the players are set to (the applied route,
//! `film_output`, and the very device: its UID and CoreAudio object) and
//! whether a device is there now.
//!
//! A route is *confirmed* once the CoreAudio object of the device the
//! players were set to has been read. Nothing is scheduled on an
//! unconfirmed route: a start waits, reading the object again each tick,
//! and gives up after [`LOOKUP_GRACE_S`]. A confirmed route is judged by
//! its object alone (one property read); the device is looked up by name
//! (a listing) only to select it or set it again.
//!
//! The applied device has *dropped* when it is gone or has another object
//! than when it was confirmed (it went away and came back, perhaps between
//! two ticks; macOS gives a reconnected device a new object). A read that
//! fails says nothing about the device: reads that keep failing (no gap
//! longer than [`FAILURE_GAP_S`]) count as a drop only after
//! [`LOOKUP_GRACE_S`], each kind of lookup on its own; meanwhile nothing is
//! set again, stopped or cancelled, and a start waits (one already
//! scheduled is called off, to be scheduled again once confirmed).
//!
//! Each tick does a constant number of single CoreAudio property reads.
//! Device changes ([`Media::device_generation`]) are acted on at the next
//! tick, with a poll every two seconds as the fallback. A playback whose
//! route drops stops; a take goes on, watching the device it started on
//! (or, on the system output, which device that is), setting it again
//! after every drop, and its result says once that the film may have
//! played elsewhere.
//!
//! [`Media::device_generation`]: super::Media::device_generation

use takewise_media::DeviceLookup;

use super::RecordingWindow;
use super::recorder::StartHold;

/// How long failing device reads are tried again before they count as a
/// drop, or an unconfirmed start gives up.
pub const LOOKUP_GRACE_S: f64 = 0.5;
/// Failed reads further apart than this are not one failing streak: the
/// next starts the grace again. Long enough for the ticks of a window that
/// is not drawn (every 50 to 150 ms).
pub const FAILURE_GAP_S: f64 = 0.25;

/// A streak of failed reads of one kind of lookup.
#[derive(Debug, Default, Clone, Copy)]
pub struct Failing {
    since: Option<f64>,
    last: f64,
}

impl Failing {
    /// A read succeeded: the streak is over.
    fn ok(&mut self) {
        self.since = None;
    }

    /// A read failed at `now`: whether reads have now failed for
    /// [`LOOKUP_GRACE_S`] without a gap longer than [`FAILURE_GAP_S`].
    fn failed(&mut self, now: f64) -> bool {
        let since = match self.since {
            Some(since) if now - self.last <= FAILURE_GAP_S => since,
            _ => now,
        };
        self.since = Some(since);
        self.last = now;
        now - since >= LOOKUP_GRACE_S
    }
}

/// The failing streaks of each kind of lookup, kept apart.
#[derive(Debug, Default)]
pub struct Failures {
    applied: Failing,
    take: Failing,
    system: Failing,
    calibration: Failing,
}

/// Which kind of lookup a read is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Lookup {
    /// The device the players are set to.
    Applied,
    /// The device a take started on.
    Take,
    /// The system's default output, during a take.
    System,
    /// The output of a latency measurement.
    Calibration,
}

/// What a device lookup says against the object it was seen as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Seen {
    /// The same object: nothing happened.
    Same,
    /// Gone (`None`), or another object: it went away and perhaps came
    /// back.
    Dropped(Option<u32>),
    /// The read failed, within [`LOOKUP_GRACE_S`]: not known yet.
    Unsure,
    /// Reads kept failing for [`LOOKUP_GRACE_S`]: treated as gone, but
    /// said as not checked, since nothing says it went.
    Unreadable,
}

/// Whether the route may be used now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ready {
    /// Confirmed.
    Yes,
    /// Set, its object not read yet: a start waits.
    Pending,
}

/// Why the configured output cannot be used now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputProblem {
    /// The device is not connected.
    Missing(String),
    /// It is connected, but the players could not be set to it.
    Failed { name: String, error: String },
    /// Its object could not be read for [`LOOKUP_GRACE_S`]: not known to
    /// be there.
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
            Self::Unconfirmed(name) => format!("{name} could not be checked; try again."),
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
            Self::Unconfirmed(name) => format!(
                "{name} could not be checked, so the film did not start; start the take again."
            ),
        }
    }
}

impl RecordingWindow {
    fn failing(&mut self, kind: Lookup) -> &mut Failing {
        match kind {
            Lookup::Applied => &mut self.failing.applied,
            Lookup::Take => &mut self.failing.take,
            Lookup::System => &mut self.failing.system,
            Lookup::Calibration => &mut self.failing.calibration,
        }
    }

    /// One read of the device `uid`, seen before as `object`.
    pub(super) fn see(&mut self, uid: &str, object: u32, kind: Lookup) -> Seen {
        let lookup = self.media.output_object(uid);
        self.seen_lookup(lookup, object, kind)
    }

    /// What `lookup` says against `object`: a failed read counts as a drop
    /// only once reads of this kind failed for [`LOOKUP_GRACE_S`].
    pub(super) fn seen_lookup(&mut self, lookup: DeviceLookup, object: u32, kind: Lookup) -> Seen {
        match lookup {
            DeviceLookup::Present(now) => {
                self.failing(kind).ok();
                if now == object {
                    Seen::Same
                } else {
                    Seen::Dropped(Some(now))
                }
            }
            DeviceLookup::Absent => {
                self.failing(kind).ok();
                Seen::Dropped(None)
            }
            DeviceLookup::Unknown => {
                let now = self.media.host_now_s();
                if self.failing(kind).failed(now) {
                    Seen::Unreadable
                } else {
                    Seen::Unsure
                }
            }
        }
    }

    /// Confirms the route the players are set to by its object: read again
    /// while it was not read yet (`Pending` while reads fail, within the
    /// grace); a confirmed one is compared with what it is now. A device
    /// that dropped makes the route due to be set again (`Missing`).
    fn confirm_applied(&mut self) -> Result<Ready, OutputProblem> {
        let Some(name) = self.film_output.clone() else {
            return Ok(Ready::Yes);
        };
        let Some((uid, object)) = self.applied_device.clone() else {
            self.route_due = true;
            return Err(OutputProblem::Missing(name));
        };
        if let Some(object) = object {
            return match self.see(&uid, object, Lookup::Applied) {
                Seen::Same => Ok(Ready::Yes),
                Seen::Unsure => Ok(Ready::Pending),
                Seen::Unreadable => {
                    self.route_due = true;
                    Err(OutputProblem::Unconfirmed(name))
                }
                Seen::Dropped(_) => {
                    self.route_due = true;
                    Err(OutputProblem::Missing(name))
                }
            };
        }
        match self.media.output_object(&uid) {
            DeviceLookup::Present(object) => {
                self.failing.applied.ok();
                self.applied_device = Some((uid, Some(object)));
                Ok(Ready::Yes)
            }
            DeviceLookup::Absent => {
                self.failing.applied.ok();
                self.route_due = true;
                self.output_missing = Some(name.clone());
                Err(OutputProblem::Missing(name))
            }
            DeviceLookup::Unknown => {
                let now = self.media.host_now_s();
                if self.failing.applied.failed(now) {
                    self.route_due = true;
                    Err(OutputProblem::Unconfirmed(name))
                } else {
                    Ok(Ready::Pending)
                }
            }
        }
    }

    /// Puts the players on the configured route: the configured device, or
    /// the system output when none is configured. A route already on the
    /// configured device is judged by its object alone (see
    /// [`RecordingWindow::confirm_applied`]); the device is looked up by
    /// name only to select it or set it again. A missing device leaves the
    /// players alone and is said in the error; a route that could not be
    /// set stays due; one set and not yet confirmed is `Pending`.
    pub(super) fn sync_output(&mut self) -> Result<Ready, OutputProblem> {
        let Some(name) = self.project.config.output.device.clone() else {
            self.output_missing = None;
            self.output_error = None;
            // No device of its own to read: a streak of failed reads of
            // the one chosen before is over.
            self.failing.applied.ok();
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
            return Ok(Ready::Yes);
        };
        if self.film_output.as_deref() == Some(&name) && !self.route_due {
            match self.confirm_applied() {
                Ok(ready) => {
                    if ready == Ready::Yes {
                        self.output_missing = None;
                        self.output_error = None;
                    }
                    return Ok(ready);
                }
                Err(OutputProblem::Missing(_)) => {}
                Err(problem) => return Err(problem),
            }
        }
        // Selecting it, or setting it again: by name.
        if !self.media.output_connected(&name) {
            self.output_error = None;
            self.output_missing = Some(name.clone());
            self.route_due = true;
            return Err(OutputProblem::Missing(name));
        }
        // Missing until the device it came back as is confirmed.
        let ready = self.set_route(&name)?;
        if ready == Ready::Yes {
            self.output_missing = None;
        }
        self.output_error = None;
        Ok(ready)
    }

    /// Sets the players to the device `name`, then confirms it by its
    /// object (`Pending` while that read fails).
    fn set_route(&mut self, name: &str) -> Result<Ready, OutputProblem> {
        if let Err(error) = self.media.set_output(Some(name)) {
            self.output_error = Some(error.clone());
            self.route_due = true;
            return Err(OutputProblem::Failed {
                name: name.to_owned(),
                error,
            });
        }
        self.film_output = Some(name.to_owned());
        self.route_due = false;
        self.failing.applied.ok();
        self.applied_device = self.media.applied_output_uid().map(|uid| (uid, None));
        self.confirm_applied()
    }

    /// Before P, Shift+P or a take: the route checked and set as needed. A
    /// route set and not yet confirmed lets it begin: its start waits (see
    /// [`RecordingWindow::hold_start`]).
    pub(super) fn route_for_start(&mut self) -> Result<(), OutputProblem> {
        self.sync_output().map(drop)
    }

    /// When the window opens: puts the players on the configured route,
    /// and says so when its device is missing.
    pub(super) fn apply_output(&mut self) {
        if let Err(problem) = self.sync_output() {
            self.say_route(problem.for_playback());
        }
    }

    /// Between takes and during a playback (a device change, the poll, a
    /// reload): follows the configured route. Says when a missing output
    /// came back, unless the window is saying something else; stops a
    /// playback whose route can no longer be used (not on a read that
    /// failed for a moment, which leaves it `Pending`).
    pub(super) fn follow_output(&mut self) {
        let was_missing = self.output_missing.clone();
        match self.sync_output() {
            Ok(Ready::Yes) => {
                let back = was_missing.filter(|name| self.film_output.as_ref() == Some(name));
                // Never over a take's result or warning.
                if let Some(name) = back
                    && (self.message.is_none() || self.message != self.take_result)
                {
                    self.say_route(format!(
                        "{name} is connected: the film and your takes play there."
                    ));
                }
            }
            Ok(Ready::Pending) => {}
            Err(problem) => {
                if self.recorder.is_playing() {
                    self.recorder.stop_playback(&mut *self.media);
                    self.say_route(format!("The playback stopped. {}", problem.for_playback()));
                }
            }
        }
    }

    /// Once per tick, before the recorder's: reads whether a take's film
    /// moved, then acts on a device change at once, and while a playback or
    /// a take runs also every two seconds (should a change go unreported).
    /// A playback follows the configured route; a take checks the route it
    /// started on every tick, whatever `[output] device` says meanwhile.
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
        } else if changed || due || self.route_pending() {
            self.follow_output();
        }
    }

    /// Whether a player opened now would play, by the route as this module
    /// keeps it (it alone sets the route, and says so): the configured
    /// device set, confirmed and not failing; or, with none configured,
    /// the players on the system output and one there (one property read).
    /// No listing, and nothing set; not before the latest device change
    /// has been acted on. `default` is the system output's object, read
    /// once by the caller (only looked at on the system output).
    pub(super) fn route_usable(&self, default: DeviceLookup) -> bool {
        if self.route_due || self.devices_watched != self.media.device_generation() {
            return false;
        }
        match &self.project.config.output.device {
            // A streak of failed reads of the applied device counts only
            // for that device, never against the system output.
            Some(name) => {
                self.failing.applied.since.is_none()
                    && self.film_output.as_ref() == Some(name)
                    && self.output_missing.is_none()
                    && matches!(self.applied_device, Some((_, Some(_))))
            }
            None => self.film_output.is_none() && matches!(default, DeviceLookup::Present(_)),
        }
    }

    /// Has the applied device's object read afresh before the route counts
    /// as usable again (the device may have just gone): the next tick reads
    /// it, as for a route just set.
    pub(super) fn reconfirm_route(&mut self) {
        if let Some((_, object)) = &mut self.applied_device {
            *object = None;
        }
    }

    /// Whether the route waits on its device's object (reads failing
    /// within the grace, or not read yet since it was set): read again
    /// every tick, one property read, until it resolves, as before a start.
    fn route_pending(&self) -> bool {
        !self.route_due
            && (self.failing.applied.since.is_some()
                || matches!(self.applied_device, Some((_, None))))
    }

    /// When a take starts: the route it plays the film through, to watch.
    /// A device route not confirmed yet is confirmed before the film starts
    /// (see [`RecordingWindow::take_start_hold`]).
    pub(super) fn snapshot_take_route(&mut self) {
        self.failing.take.ok();
        self.failing.system.ok();
        self.take_route = if let Some(name) = &self.film_output {
            Some(TakeRoute {
                name: name.clone(),
                uid: self.applied_device.as_ref().map(|(uid, _)| uid.clone()),
                object: self.applied_device.as_ref().and_then(|(_, object)| *object),
                system: false,
                tried: None,
                pending: None,
            })
        } else {
            let object = match self.media.default_output_object() {
                DeviceLookup::Present(object) => Some(object),
                DeviceLookup::Absent | DeviceLookup::Unknown => None,
            };
            // Its name and UID are read once, here; a take is never left
            // unwatched for want of them: its object is the baseline, under
            // a plain label until the name can be read.
            let named = self.media.default_output_uid();
            Some(TakeRoute {
                name: named
                    .as_ref()
                    .map_or_else(|| SYSTEM_OUTPUT.to_owned(), |(name, _)| name.clone()),
                uid: named.map(|(_, uid)| uid),
                object,
                system: true,
                tried: None,
                pending: None,
            })
        };
    }

    /// Reads the object of a take's device that was not confirmed when the
    /// take started: `Seen::Same` once it is, `Unsure` while reads fail
    /// (within the grace), `Dropped(None)` when it is gone or reads failed
    /// for too long.
    fn confirm_take_device(&mut self) -> Seen {
        let Some(uid) = self.take_route.as_ref().and_then(|route| route.uid.clone()) else {
            let now = self.media.host_now_s();
            return if self.failing.take.failed(now) {
                Seen::Unreadable
            } else {
                Seen::Unsure
            };
        };
        match self.media.output_object(&uid) {
            DeviceLookup::Present(object) => {
                self.failing.take.ok();
                if let Some(route) = &mut self.take_route {
                    route.object = Some(object);
                }
                if let Some((applied, unread @ None)) = &mut self.applied_device
                    && *applied == uid
                {
                    *unread = Some(object);
                    if self.output_missing == self.film_output {
                        self.output_missing = None;
                    }
                }
                Seen::Same
            }
            DeviceLookup::Absent => {
                self.failing.take.ok();
                Seen::Dropped(None)
            }
            DeviceLookup::Unknown => {
                let now = self.media.host_now_s();
                if self.failing.take.failed(now) {
                    Seen::Unreadable
                } else {
                    Seen::Unsure
                }
            }
        }
    }

    /// Checks the route a take started on, every tick (one property read)
    /// and as it stops. Once the take's film was seen running, a drop of its
    /// device, or on the system output another default output, is noted
    /// on the take (said once with its result), and after every drop the
    /// route is set again as soon as the device is back: at once when it is
    /// seen back, else tried again on a device change or the poll (`event`).
    pub(super) fn check_take_route(&mut self, event: bool) {
        let Some(route) = self.take_route.clone() else {
            return;
        };
        if route.system {
            self.check_system_route(&route, event);
            return;
        }
        let (Some(uid), Some(object)) = (route.uid.clone(), route.object) else {
            // Not confirmed yet: its film waits for that (see
            // `take_start_hold`), so nothing played.
            self.confirm_take_device();
            return;
        };
        let seen = self.see(&uid, object, Lookup::Take);
        let back = match seen {
            Seen::Same | Seen::Unsure => return,
            Seen::Unreadable => None,
            Seen::Dropped(back) => back,
        };
        let note = if seen == Seen::Unreadable {
            format!(
                "{} could not be checked during the take: the film may have played elsewhere; listen before you choose it.",
                route.name
            )
        } else {
            format!(
                "{} was disconnected during the take, so the film may have played elsewhere, out loud: listen before you choose it.",
                route.name
            )
        };
        if !self.recorder.note_route(note) {
            // Its film has not been seen running: the start is held instead.
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
        if self.set_route(&route.name) == Ok(Ready::Yes)
            && let Some(route) = &mut self.take_route
        {
            route.uid = self.applied_device.as_ref().map(|(uid, _)| uid.clone());
            route.object = self.applied_device.as_ref().and_then(|(_, object)| *object);
        }
    }

    /// A take on the system output: the default output's object is compared
    /// every tick (one property read). Its name is read once per new object
    /// and kept with it, so a change during the pre-roll is noted, without a
    /// listing each frame, once the film was seen running.
    fn check_system_route(&mut self, route: &TakeRoute, event: bool) {
        let lookup = self.media.default_output_object();
        let Some(object) = route.object else {
            // Not read when the take started: from the first read on.
            if let (DeviceLookup::Present(object), Some(route)) = (lookup, &mut self.take_route) {
                route.object = Some(object);
            }
            return;
        };
        let now = match self.seen_lookup(lookup, object, Lookup::System) {
            Seen::Same => {
                // Its name, not read at the start: on a change or the poll.
                if event && route.uid.is_none() {
                    self.name_system_route();
                }
                return;
            }
            Seen::Unsure => return,
            Seen::Unreadable => None,
            Seen::Dropped(now) => now,
        };
        let pending = match &route.pending {
            Some((seen, named)) if *seen == now => named.clone(),
            _ => {
                let named = now.and_then(|_| self.media.default_output_uid());
                if let Some(route) = &mut self.take_route {
                    route.pending = Some((now, named.clone()));
                }
                named
            }
        };
        // The same device back as a new object, or none: it dropped out.
        let back = pending
            .as_ref()
            .is_some_and(|(_, uid)| route.uid.as_ref() == Some(uid));
        let other = pending.filter(|_| !back);
        let from = route.uid.as_ref().map(|_| route.name.as_str());
        let what = if now.is_none() || back {
            format!(
                "dropped out{}",
                from.map(|n| format!(" ({n})")).unwrap_or_default()
            )
        } else {
            let to = other.as_ref().map(|(name, _)| format!(" to {name}"));
            let from = from.map(|n| format!(" from {n}"));
            format!(
                "changed{}{}",
                from.unwrap_or_default(),
                to.unwrap_or_default()
            )
        };
        let note =
            format!("The system output {what} during the take: listen before you choose it.");
        if self.recorder.note_route(note)
            && let Some(route) = &mut self.take_route
        {
            // Followed from here, so a later change is seen too.
            route.object = now;
            route.pending = None;
            if let Some((name, uid)) = other {
                route.name = name;
                route.uid = Some(uid);
            }
        }
    }

    /// Each tick before the recorder starts a playback or a take's film, or
    /// while the take's film was not yet seen running: whether it may
    /// start. Nothing starts on an unconfirmed route: it waits while the
    /// device's object cannot be read, and stops once that failed for
    /// [`LOOKUP_GRACE_S`]. A playback follows the configured route (set
    /// again when the device dropped and is back); a take checks the device
    /// it started on.
    pub(super) fn hold_start(&mut self) {
        let now = self.media.host_now_s();
        let mut waiting_for = None;
        let hold = if self.recorder.playback_waiting(now) {
            match self.sync_output() {
                // On the system output, only once there is one; known to be
                // none, that is what a playback that gives up says.
                Ok(Ready::Yes) if self.film_output.is_none() => {
                    match self.media.default_output_object() {
                        DeviceLookup::Present(_) => StartHold::Go,
                        DeviceLookup::Absent => {
                            waiting_for = Some("there is no output to play through".to_owned());
                            StartHold::Wait
                        }
                        DeviceLookup::Unknown => StartHold::Wait,
                    }
                }
                Ok(Ready::Yes) => StartHold::Go,
                Ok(Ready::Pending) => StartHold::Wait,
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
        self.recorder.waiting_for(waiting_for);
    }

    /// Whether a take's film may start: on the system output, yes; on a
    /// device, only once its object is confirmed and unchanged.
    pub(super) fn take_start_hold(&mut self) -> StartHold {
        let Some(route) = self.take_route.clone() else {
            return StartHold::Go;
        };
        if route.system {
            return StartHold::Go;
        }
        let seen = match (&route.uid, route.object) {
            (Some(uid), Some(object)) => self.see(uid, object, Lookup::Take),
            _ => self.confirm_take_device(),
        };
        if !matches!(seen, Seen::Same | Seen::Unsure) {
            // Before stopping it: a film that moved already played, and its
            // loss is noted instead.
            self.recorder.probe_film(&*self.media);
            if !self.recorder.film_waiting() {
                return StartHold::Go;
            }
        }
        match seen {
            Seen::Same => StartHold::Go,
            Seen::Unsure => StartHold::Wait,
            Seen::Dropped(Some(_)) => {
                StartHold::Stop(format!("{} dropped out; start the take again.", route.name))
            }
            // Reads failed: not checked, which is not the same as gone.
            Seen::Unreadable => {
                StartHold::Stop(OutputProblem::Unconfirmed(route.name).for_recording())
            }
            Seen::Dropped(None) => {
                StartHold::Stop(OutputProblem::Missing(route.name).for_recording())
            }
        }
    }

    /// A take on the system output whose name could not be read when it
    /// started: read it now (a listing), keeping the object as it was.
    fn name_system_route(&mut self) {
        if let Some((name, uid)) = self.media.default_output_uid()
            && let Some(route) = &mut self.take_route
        {
            route.name = name;
            route.uid = Some(uid);
        }
    }

    /// Says something about the route (a refusal, a stop, its output being
    /// back).
    pub(super) fn say_route(&mut self, text: String) {
        self.message = Some(text);
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
/// object (each `None` while it could not be read), whether it is the
/// system output (whose device can change during the take), the object it
/// was last set again to after a drop, and on the system output another
/// default output seen and not yet noted (its object, and its name and
/// UID, read once). A reload that changes `[output] device` does not change
/// it.
#[derive(Debug, Clone)]
pub struct TakeRoute {
    name: String,
    uid: Option<String>,
    object: Option<u32>,
    system: bool,
    tried: Option<u32>,
    pending: Option<(Option<u32>, Option<Named>)>,
}

/// A device's name and UID.
type Named = (String, String);

/// What the system output is called while its name cannot be read.
const SYSTEM_OUTPUT: &str = "the system output";

#[cfg(test)]
mod tests {
    use super::{Failing, LOOKUP_GRACE_S};

    #[test]
    fn only_a_streak_of_failed_reads_is_a_drop() {
        let mut failing = Failing::default();
        // One failed read, then another much later: two streaks of one.
        assert!(!failing.failed(10.0));
        assert!(!failing.failed(11.0));
        // Reads failing every tick for the grace: a drop.
        let mut at = 20.0;
        while at < 20.0 + LOOKUP_GRACE_S {
            assert!(!failing.failed(at), "{at}");
            at += 0.1;
        }
        assert!(failing.failed(at));
        // A good read ends the streak.
        failing.ok();
        assert!(!failing.failed(at + 0.1));
    }
}
