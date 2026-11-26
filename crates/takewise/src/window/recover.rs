//! Players that failed. macOS fails a playing `AVPlayerItem` whose output
//! device goes away (status Failed, AVFoundation error -11800), and the
//! item never plays again, even once the device is back and set again
//! (see the smoke test `players_after_output_loss`): only a new player
//! does, and only once the device is there (one opened while it is gone
//! fails the same way).
//!
//! So when the film's player fails, a playback on it stops, and so does a
//! take (its film no longer moves, so it would never reach its end); the
//! take keeps what it recorded. The film is set aside (the recorder treats
//! it as not ready, and nothing queued starts on it) and opened again as
//! the window opens it (its output, level and warm-up) once the route is
//! usable: the configured device set and confirmed afresh after the
//! failure, or a system output there, as `output.rs` keeps and sets the
//! route (recovery only reads it). It is tried once the device is
//! confirmed, again at once when its [`Identity`] changes, and otherwise
//! after a growing pause ([`FIRST_RETRY_S`] up to [`LAST_RETRY_S`]); right
//! before it opens, a device's object is read once more.
//!
//! Whether the file or the output is at fault is judged by the outcome: a
//! film opened again on a usable output that fails too is said to be
//! unplayable, with the player's reason, and tried again less and less
//! often. A film that does not load in time counts as failed. A reopened
//! film still loading is restarted only when the output it was opened on
//! is replaced (another identity), not when the route merely cannot be
//! confirmed for a moment, nor for other device changes.
//!
//! What it says is rendered from these facts each frame, beside the
//! window's message: the notice ([`RecordingWindow::film_note`]) and, after
//! a start was refused or a playback stopped for it, the status
//! ([`RecordingWindow::film_status`]). No take result or playback message
//! is replaced, and both go once the film is ready again. A take player
//! that failed needs nothing more: the next take opened gets a new one.

use std::path::PathBuf;

use takewise_core::{self as tw, Project};
use takewise_media::DeviceLookup;

use super::RecordingWindow;

/// Why the window asks to be opened again after `takewise film`.
const FILM_REPLACED: &str =
    "The project now uses another render of the film; close and open this window again to play it.";

/// The film the player has open, as it was when it was opened.
#[derive(Debug, Clone)]
pub(super) struct OpenFilm {
    path: PathBuf,
    /// The project's `[film_info]` then.
    info: tw::FilmInfo,
    /// The file's size and modification time then; none when they could
    /// not be read.
    stamp: Option<(u64, String)>,
}

impl OpenFilm {
    /// The project's film, just opened.
    pub(super) fn opened(project: &Project) -> Self {
        let path = project.film_path();
        let stamp = crate::film::stamp_io(&path).ok();
        Self {
            path,
            info: project.config.film_info.clone(),
            stamp,
        }
    }

    /// Whether `project` still names this film: the same path, and either
    /// the `[film_info]` it had when the film was opened or one measured
    /// on the very file that was opened (a render written over the old
    /// one before the window opened, accepted with `takewise film` since).
    pub(super) fn is_named_by(&self, project: &Project) -> bool {
        let info = &project.config.film_info;
        self.path == project.film_path()
            && (tw::film::same_film(&self.info, info)
                || self.stamp.as_ref().is_some_and(|(size, modified)| {
                    *size == info.size && *modified == info.modified
                }))
    }
}

/// The first pause before the film is opened again after a reopen that
/// failed; it doubles with every further failure.
pub const FIRST_RETRY_S: f64 = 1.0;

/// The longest pause between two reopens.
pub const LAST_RETRY_S: f64 = 16.0;

/// How long a film may load before it counts as failed; longer with
/// every attempt, up to [`LONGEST_LOAD_S`], so a film slow to load gets
/// there.
pub const LOAD_TIMEOUT_S: f64 = 10.0;

/// The longest a film is given to load.
pub const LONGEST_LOAD_S: f64 = 30.0;

/// How long a reopened film must stay fine after its warm-up before its
/// recovery counts as over and the pauses start again from the first.
pub const SETTLED_S: f64 = 2.0;

/// The output a film plays through, as the window has set it: the applied
/// device's UID and confirmed CoreAudio object, or, on the system output,
/// the default device's object (a silent switch of the default is another
/// identity). Another identity means another device: a player opened on
/// the one before is replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The applied device's UID; `None` on the system output.
    uid: Option<String>,
    /// Its CoreAudio object, once confirmed (the default's, on the system
    /// output, once read).
    object: Option<u32>,
}

/// Why the film is not open.
#[derive(Debug, Clone)]
enum Why {
    /// Its player failed (or did not load in time), for this reason.
    Failed(String),
    /// It could not be opened again, for this reason.
    NotOpened(String),
}

impl Why {
    fn reason(&self) -> &str {
        match self {
            Why::Failed(reason) | Why::NotOpened(reason) => reason,
        }
    }
}

/// What could not start on the film.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// P, Shift+P or Space.
    Start,
    /// A playback that waited for it, or was stopped for it.
    Playback,
}

/// The project's film in the window: the facts its notice and status are
/// rendered from.
#[derive(Debug, Clone)]
pub struct Recovery {
    /// Whether the film is set aside (its player failed, or the beep film
    /// is in its place) and waits to be opened again.
    set_aside: bool,
    /// When it is tried next.
    retry_at: f64,
    /// The pause after the next reopen that fails.
    backoff_s: f64,
    /// Reopens since it last settled.
    attempts: u32,
    /// When it was opened, and on which output, until it is ready.
    loading: Option<(f64, Identity)>,
    /// When a reopened film, ready and warmed up, counts as recovered.
    settles_at: Option<f64>,
    /// The output it was last tried on.
    tried: Option<Identity>,
    /// The route as this tick saw it, once, for both what recovery does
    /// and what it says: whether it is usable, and its identity (only
    /// read while a film is set aside or recovering).
    route: Option<(bool, Identity)>,
    /// When its route was marked to be read afresh after a failure: until
    /// a later tick has read it, it is said to be opening again.
    rereading_since: Option<f64>,
    /// Why it is not open, until it is ready again.
    why: Option<Why>,
    /// What could not start on it, until it is ready.
    refused: Option<Refused>,
}

impl Default for Recovery {
    fn default() -> Self {
        Self {
            set_aside: false,
            retry_at: 0.0,
            backoff_s: FIRST_RETRY_S,
            attempts: 0,
            loading: None,
            settles_at: None,
            tried: None,
            route: None,
            rereading_since: None,
            why: None,
            refused: None,
        }
    }
}

impl Recovery {
    /// A failed player, or the beep film, is in the film's place: it is
    /// opened again as soon as its output can be used.
    fn set_aside(&mut self) {
        self.set_aside = true;
        self.retry_at = f64::NEG_INFINITY;
        self.loading = None;
        self.settles_at = None;
    }

    /// The beep film takes the film's place: nothing of an earlier failure
    /// is said for it, and its load has its own deadline.
    pub fn set_aside_for_beeps(&mut self) {
        self.set_aside();
        self.why = None;
        self.refused = None;
    }

    /// The film was just opened (on `identity`): it counts as failed if it
    /// does not load in time.
    pub fn opened(&mut self, now: f64, identity: Identity) {
        self.loading = Some((now, identity));
    }

    /// A playback that waited for the film could not start.
    pub fn playback_refused(&mut self) {
        self.refused = Some(Refused::Playback);
    }

    /// The pause before the next try, and the one after it.
    fn back_off(&mut self, now: f64) {
        self.retry_at = now + self.backoff_s;
        self.backoff_s = (self.backoff_s * 2.0).min(LAST_RETRY_S);
    }
}

impl RecordingWindow {
    /// Once per tick: stops a playback, a take or a calibration whose
    /// player failed, and opens the film again once that can work.
    pub(super) fn recover_players(&mut self, now: f64) {
        self.settle_film(now);
        self.observe_route(now);
        // A failed player set aside is not asked again every tick.
        let failure = (!self.film.set_aside || self.calibrating.is_some())
            .then(|| self.media.film_failure())
            .flatten()
            .or_else(|| self.watch_film_load(now));
        if let Some(reason) = failure {
            if self.calibrating.is_some() {
                let why =
                    format!("the beeps' player stopped ({reason}); press Space to measure again");
                self.cancel_calibration();
                if let Some(setup) = &mut self.setup {
                    setup.latency = Some(Err(why.clone()));
                }
                self.message = Some(format!("Latency calibration cancelled: {why}."));
                return;
            }
            if !self.film.set_aside {
                self.lose_film(reason, now);
            }
        } else if self.recorder.is_playing()
            && let Some(reason) = self.media.take_failure()
        {
            self.recorder.stop_playback(&mut *self.media);
            self.message = Some(format!(
                "The playback stopped: the take stopped ({reason}). Play it again."
            ));
        }
        if self.film.set_aside && self.calibrating.is_none() && !self.recorder.is_recording() {
            self.reopen_film(now);
        }
        self.recorder.hold_film(self.film.set_aside);
    }

    /// The output the window has set, as recovery tells devices apart (see
    /// [`Identity`]); `default` is the system output's object.
    fn route_identity(&self, default: DeviceLookup) -> Identity {
        match &self.applied_device {
            Some((uid, object)) if self.film_output.is_some() => Identity {
                uid: Some(uid.clone()),
                object: *object,
            },
            _ => Identity {
                uid: None,
                object: match default {
                    DeviceLookup::Present(object) => Some(object),
                    DeviceLookup::Absent | DeviceLookup::Unknown => None,
                },
            },
        }
    }

    /// The output the film is opened on now (one read of the system
    /// output's object, when it is in use).
    pub(super) fn current_identity(&self) -> Identity {
        let default = if self.film_output.is_none() {
            self.media.default_output_object()
        } else {
            DeviceLookup::Unknown
        };
        self.route_identity(default)
    }

    /// Takes this tick's one look at the route, while a film is set aside
    /// or recovering (nothing is read during an ordinary first load).
    fn observe_route(&mut self, now: f64) {
        if !self.film.set_aside && self.film.attempts == 0 {
            self.film.route = None;
            return;
        }
        let default = if self.film_output.is_none() {
            self.media.default_output_object()
        } else {
            DeviceLookup::Unknown
        };
        let usable = self.route_usable(default);
        self.film.route = Some((usable, self.route_identity(default)));
        if self.film.rereading_since.is_some_and(|since| now > since) {
            self.film.rereading_since = None;
        }
    }

    /// Whether the route is usable, as this tick saw it.
    fn seen_usable(&self) -> bool {
        self.film.route.as_ref().is_some_and(|(usable, _)| *usable)
    }

    /// While a film loads. Once it is ready, what it said goes. A reopened film still loading whose
    /// output was replaced (another confirmed identity) is set aside, to
    /// be opened again at once, without counting as an attempt. Returns
    /// why it counts as failed when it took too long.
    fn watch_film_load(&mut self, now: f64) -> Option<String> {
        if !self.film.set_aside && self.media.film_ready() {
            self.film.refused = None;
        }
        let (since, identity) = self.film.loading.clone()?;
        // Another output, even as it turns ready: opened again on it.
        if self.film.attempts > 0
            && let Some((_, current)) = &self.film.route
            && current.object.is_some()
            && *current != identity
        {
            self.film.attempts -= 1;
            self.put_film_aside();
            return None;
        }
        if self.media.film_ready() {
            // On the system output, accepted only on a default that is
            // there: one that appears later may be another device, and is
            // then checked as above.
            let unread = identity.uid.is_none()
                && self
                    .film
                    .route
                    .as_ref()
                    .is_some_and(|(_, current)| current.object.is_none());
            if self.film.attempts > 0 && unread {
                return None;
            }
            self.film.loading = None;
            self.film.why = None;
            return None;
        }
        let timeout = (LOAD_TIMEOUT_S * f64::from(self.film.attempts.max(1))).min(LONGEST_LOAD_S);
        (now - since > timeout).then(|| "it did not load".to_owned())
    }

    /// A reopened film, ready and warmed up, counts as recovered once it has
    /// stayed fine for [`SETTLED_S`]: the next failure is a first one again.
    fn settle_film(&mut self, now: f64) {
        if let Some(at) = self.film.settles_at {
            if now >= at {
                self.film = Recovery::default();
            }
        } else if self.film.attempts > 0
            && self.film.loading.is_none()
            && !self.film.set_aside
            && self.recorder.warmed_up()
        {
            self.film.settles_at = Some(now + SETTLED_S);
        }
    }

    /// Sets the film aside: a playback queued or playing on it stops (said
    /// in the film's status), and the recorder treats it as not ready.
    fn put_film_aside(&mut self) {
        if self.recorder.is_playing() {
            self.recorder.stop_playback(&mut *self.media);
            self.film.refused = Some(Refused::Playback);
        }
        self.media.film_pause();
        self.film.set_aside();
        self.recorder.hold_film(true);
    }

    /// The film's player failed: stops what plays on it, sets it aside,
    /// and has the route confirmed afresh before it is opened again.
    fn lose_film(&mut self, reason: String, now: f64) {
        // Opened again on a usable output, and failed too: the file, or
        // an output that keeps going away.
        let again = self.film.attempts > 0;
        let what = if again {
            format!("could not play ({reason})")
        } else {
            format!("stopped ({reason})")
        };
        if self.recorder.is_recording() {
            let why = format!("The take stopped early: the film {what}.");
            let update = self.recorder.abort_take(&mut *self.media, &self.dir, why);
            self.show(update);
            self.pause_checks_while_recording();
        }
        if self.recorder.is_playing() {
            self.recorder.stop_playback(&mut *self.media);
            self.message = Some(format!(
                "The playback stopped: the film {what}. Play it again once it is back."
            ));
        }
        self.put_film_aside();
        self.film.why = Some(Why::Failed(reason));
        if again {
            self.film.back_off(now);
        }
        // Its device may have just gone: not reopened on the confirmation
        // from before.
        self.reconfirm_route();
        self.film.rereading_since = Some(now);
        if let Some((usable, _)) = &mut self.film.route {
            *usable = false;
        }
    }

    /// Opens the film again, as the window opens it, once the route is
    /// usable and its device confirmed: at once when another output is set,
    /// else when the pause is over. Right before it opens, a device's
    /// object is read once more: one that went meanwhile (its notice may
    /// come late) is read afresh by the route check instead.
    fn reopen_film(&mut self, now: f64) {
        let Some((true, identity)) = self.film.route.clone() else {
            return;
        };
        if identity.object.is_none() {
            return;
        }
        let changed = self.film.tried.as_ref() != Some(&identity);
        if now < self.film.retry_at && !changed {
            return;
        }
        if let (Some(uid), Some(object)) = (&identity.uid, identity.object)
            && self.media.output_object(uid) != DeviceLookup::Present(object)
        {
            self.reconfirm_route();
            self.film.route = Some((false, identity));
            return;
        }
        self.film.tried = Some(identity.clone());
        self.film.attempts += 1;
        match self.restore_film() {
            Ok(()) => {
                self.film.set_aside = false;
                self.film.opened(now, identity);
            }
            Err(err) => {
                self.film.why = Some(Why::NotOpened(err));
                self.film.back_off(now);
            }
        }
    }

    /// After a calibration: the project's film back now if its output can
    /// be used, or on a later tick once it can.
    pub(super) fn reopen_film_now(&mut self) {
        self.film.set_aside_for_beeps();
        let now = self.media.host_now_s();
        self.observe_route(now);
        self.reopen_film(now);
        self.recorder.hold_film(self.film.set_aside);
    }

    /// What the film's recovery says now, rendered from its facts.
    pub(super) fn film_note(&self, now: f64) -> Option<String> {
        let why = self.film.why.as_ref()?;
        let reason = why.reason();
        if !self.seen_usable() && self.film.rereading_since.is_none() {
            return Some(format!(
                "The film stopped ({reason}); Takewise opens it again once its output is back."
            ));
        }
        if !self.film.set_aside {
            return Some(format!(
                "The film stopped ({reason}); Takewise is opening it again."
            ));
        }
        let pause = (self.film.retry_at - now).ceil().max(1.0);
        Some(match why {
            Why::NotOpened(_) => format!(
                "Could not open the film again ({reason}); Takewise tries again in {pause:.0} s and when the output changes."
            ),
            Why::Failed(_) if self.film.attempts > 0 => format!(
                "The film could not play ({reason}); Takewise tries again in {pause:.0} s and when the output changes. If this goes on, check the film file."
            ),
            Why::Failed(_) => format!("The film stopped ({reason}); Takewise opens it again."),
        })
    }

    /// When the project now has another film than the one playing
    /// (`takewise film` pointed it at a new render, or measured a changed
    /// one again): the window keeps the film it opened until it is opened
    /// again, so it asks to be reopened, and no take starts meanwhile
    /// (see [`RecordingWindow::blocked`]): it would be timed against the
    /// new cut while the old picture plays.
    /// Re-reads the project when the film it names on disk is not the one
    /// this window last read, so a take never starts on a render that
    /// `takewise film` replaced before the file watcher reported it.
    pub(super) fn notice_film_change(&mut self) {
        let Ok(on_disk) = Project::open(&self.dir) else {
            return;
        };
        let config = &self.project.config;
        if on_disk.config.film != config.film
            || !tw::film::same_film(&on_disk.config.film_info, &config.film_info)
        {
            self.reload();
        }
    }

    pub(super) fn film_replaced_note(&self) -> Option<&'static str> {
        (!self.film_played.is_named_by(&self.project)).then_some(FILM_REPLACED)
    }

    /// Why what was refused could not start on the film, while it is not
    /// ready, rendered from its facts.
    pub(super) fn film_status(&self) -> Option<String> {
        let refused = self.film.refused?;
        let again = match refused {
            Refused::Start => "try again",
            Refused::Playback => "play it again",
        };
        // The notice says why and when: the status does not repeat it.
        if self.film.why.is_some() {
            return Some(format!("The film is not ready; {again} once it is back."));
        }
        let why = if self.film.set_aside {
            "it is being opened again".to_owned()
        } else if let Some(reason) = self.media.film_failure() {
            reason
        } else if !self.media.film_ready() {
            "still loading".to_owned()
        } else {
            return None;
        };
        Some(format!(
            "The film is not ready ({why}); {again} in a moment."
        ))
    }

    /// Whether a playback or a take cannot start on the film now (said in
    /// the film's status): it is set aside or failed, or (`loading`, for a
    /// take, whose film must start at once) it is still loading.
    pub(super) fn film_refused(&mut self, loading: bool) -> bool {
        let refused = self.film.set_aside
            || self.media.film_failure().is_some()
            || (loading && !self.media.film_ready());
        self.film.refused = refused.then_some(Refused::Start);
        refused
    }
}
