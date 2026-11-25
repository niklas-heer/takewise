//! Players that failed. macOS fails a playing `AVPlayerItem` whose output
//! device goes away (status Failed, AVFoundation error -11800), and the
//! item never plays again, even once the device is back and set again
//! (see the smoke test `players_after_output_loss`): only a new player
//! does, and only once the device is there (one opened while it is gone
//! fails the same way).
//!
//! So, when the film's player failed because its output went away (the
//! device it was opened on is gone, or back as another device), a
//! playback on it stops, and so does a take (its film no longer moves, so
//! it would never reach its end): the take keeps its audio and says so.
//! The film is then reopened as the window opens it (its output, level
//! and warm-up), only once the route check finds the output usable (the
//! configured device confirmed and set, or a system output there), at
//! most every [`REOPEN_EVERY_S`]. A film that failed for another reason
//! (the file itself) is said once, with the player's reason, and not
//! reopened. A take player that failed needs nothing more: the next take
//! opened gets a new one.

use takewise_media::DeviceLookup;

use super::RecordingWindow;
use super::latency::OutputIdentity;
use super::output::Ready;
use super::recorder;

/// The project's film in the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilmState {
    /// Open (it may still fail).
    Open,
    /// Not open (it failed as its output went away, or the beep film is in
    /// its place): reopened once its output can be used.
    Reopen,
    /// It failed for another reason than its output; that was said.
    Unplayable,
}

/// How often a film that keeps failing is reopened.
pub const REOPEN_EVERY_S: f64 = 2.0;

/// How long a reopened film may load before, its output gone meanwhile,
/// it is reopened again: a player opened as its device went away may
/// neither load nor fail.
pub const RELOAD_AFTER_S: f64 = 3.0;

impl RecordingWindow {
    /// Once per tick: stops a playback or a take whose player failed, and
    /// reopens the project's film once that can work.
    pub(super) fn recover_players(&mut self, now: f64) {
        // Opened on an output not yet confirmed: known once it is.
        if self.film_on == OutputIdentity::Unknown && self.film != FilmState::Reopen {
            self.film_on = self.output_identity();
        }
        let mut film = self.media.film_failure();
        if let Some(since) = self.film_loading_since {
            if film.is_some() || self.media.film_ready() {
                self.film_loading_since = None;
            } else if now - since > RELOAD_AFTER_S && self.film_output_lost() {
                self.film_loading_since = None;
                film = Some("it did not load".to_owned());
            }
        }
        let take = self
            .media
            .take_failure()
            .filter(|_| self.recorder.is_playing());
        if film.is_none() && take.is_none() && self.film != FilmState::Reopen {
            return;
        }
        let output_lost = self.film_output_lost();
        let why = |reason: &str| {
            if output_lost {
                "could not play on after its output went away".to_owned()
            } else {
                format!("stopped playing ({reason})")
            }
        };
        // What stopping a take said, kept with what follows.
        let mut take_said = None;
        if let Some(reason) = &film
            && self.recorder.is_recording()
        {
            let note = format!(
                "The film {}. The take is kept; listen before you choose it.",
                why(reason)
            );
            let update = self.recorder.abort_take(&mut *self.media, &self.dir, note);
            self.show(update);
            self.pause_checks_while_recording();
            take_said = self.message.take();
        }
        let stopped = film
            .as_ref()
            .map(|reason| ("film", reason))
            .or_else(|| take.as_ref().map(|reason| ("take", reason)));
        if let Some((what, reason)) = stopped
            && self.recorder.is_playing()
        {
            self.recorder.stop_playback(&mut *self.media);
            self.message = Some(format!(
                "The playback stopped: the {what} {}. Play it again.",
                why(reason)
            ));
        }
        if let Some(reason) = &film {
            if self.calibrating.is_some() {
                let why = format!(
                    "the beeps' player {}; press Space to measure again",
                    why(reason)
                );
                self.cancel_calibration();
                if let Some(setup) = &mut self.setup {
                    setup.latency = Some(Err(why.clone()));
                }
                self.message = Some(format!("Latency calibration cancelled: {why}."));
                return;
            }
            if output_lost {
                self.film = FilmState::Reopen;
            } else if self.film == FilmState::Open {
                // Not its output: said once, and not reopened.
                self.film = FilmState::Unplayable;
                self.say_film(format!(
                    "The film cannot be played: {reason}. Open the project again, or check the film file."
                ));
            }
        }
        self.message = recorder::join(take_said, self.message.take());
        // Not while the beep film is in its place.
        if self.film == FilmState::Reopen && self.calibrating.is_none() {
            self.reopen_film(now);
        }
    }

    /// Whether the output the film was opened on went away since: it is
    /// gone, back as another device, or cannot be read.
    fn film_output_lost(&self) -> bool {
        match &self.film_on {
            OutputIdentity::Device(uid, object) => {
                self.media.output_object(uid) != DeviceLookup::Present(*object)
            }
            OutputIdentity::System(object) => {
                self.media.default_output_object() != DeviceLookup::Present(*object)
            }
            OutputIdentity::Unknown => true,
        }
    }

    /// Says `text` about the film, alongside a take's result rather than
    /// in its place.
    fn say_film(&mut self, text: String) {
        let kept = self
            .message
            .take()
            .filter(|message| Some(message) == self.take_result.as_ref());
        self.message = recorder::join(kept, Some(text));
    }

    /// Opens the project's film again, as the window opens it, once its
    /// output is usable: the configured device confirmed and set, or a
    /// system output there. A film opened on a missing or unconfirmed
    /// output would fail at once. Tried at most every [`REOPEN_EVERY_S`];
    /// one that could not be opened is said once, and tried again only
    /// once the devices change; once one works, that is taken back.
    pub(super) fn reopen_film(&mut self, now: f64) {
        self.film = FilmState::Reopen;
        let generation = self.media.device_generation();
        if now < self.film_reopen_at
            || self.film_given_up == Some(generation)
            || !self.film_output_usable()
        {
            return;
        }
        self.film_reopen_at = now + REOPEN_EVERY_S;
        match self.restore_film() {
            Ok(()) => {
                self.film_given_up = None;
                self.film_loading_since = Some(now);
                if let Some(warning) = self.film_warning.take()
                    && let Some(message) = self.message.take()
                {
                    let rest = message.replace(&warning, "");
                    let rest = rest.trim();
                    self.message = (!rest.is_empty()).then(|| rest.to_owned());
                }
            }
            Err(err) => {
                self.film_given_up = Some(generation);
                let warning = format!(
                    "Could not reopen the film: {err}. Takewise tries again when an output device changes."
                );
                self.say_film(warning.clone());
                self.film_warning = Some(warning);
            }
        }
    }

    /// After a calibration: the project's film back now if its output can
    /// be used, or on a later tick once it can.
    pub(super) fn reopen_film_now(&mut self) {
        self.film_reopen_at = f64::NEG_INFINITY;
        self.film_given_up = None;
        let now = self.media.host_now_s();
        self.reopen_film(now);
    }

    /// Whether a film opened now would play: the route check passes and
    /// confirms the configured device, or with none configured a system
    /// output exists.
    fn film_output_usable(&mut self) -> bool {
        match self.sync_output() {
            Ok(Ready::Yes) if self.project.config.output.device.is_none() => {
                matches!(self.media.default_output_object(), DeviceLookup::Present(_))
            }
            Ok(Ready::Yes) => true,
            _ => false,
        }
    }

    /// Why a playback or a take cannot start on the film now, if it
    /// cannot: it waits to be opened again, or failed, or (`loading`, for
    /// a take, whose film must start at once) it is still loading.
    pub(super) fn film_problem(&self, loading: bool) -> Option<String> {
        let failure = self.media.film_failure();
        if self.film == FilmState::Reopen || (failure.is_some() && self.film_output_lost()) {
            return Some(
                "The film is opened again once its output can be used; try again in a moment."
                    .to_owned(),
            );
        }
        if let Some(reason) = failure {
            return Some(format!("The film cannot be played: {reason}."));
        }
        (loading && !self.media.film_ready())
            .then(|| "The film is still loading; press Space again in a moment.".to_owned())
    }
}
