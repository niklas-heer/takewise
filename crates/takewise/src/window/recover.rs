//! Players that failed. macOS fails a playing `AVPlayerItem` whose output
//! device goes away (status Failed, AVFoundation error -11800), and the
//! item never plays again, even once the device is back and set again
//! (see the smoke test `players_after_output_loss`): only a new player
//! does, and only once the device is there (one opened while it is gone
//! fails the same way).
//!
//! So when the film's player fails, a playback on it stops, and so does a
//! take (its film no longer moves, so it would never reach its end); the
//! take keeps what it recorded. The film is then opened again as the
//! window opens it (its output, level and warm-up) whenever the route
//! check finds the output usable: the configured device set and
//! confirmed, or a system output there, as `output.rs` keeps and sets the
//! route (recovery only reads it). It is tried at once, again at once
//! whenever the route or the devices change or P, Shift+P or Space is
//! pressed, and otherwise after a growing pause ([`FIRST_RETRY_S`] up to
//! [`LAST_RETRY_S`]). Whether the
//! file or the output is at fault is judged by the outcome: a film opened
//! again on a usable output that fails too is said to be unplayable, with
//! the player's reason, and tried again less and less often. A film that
//! does not load in time counts as failed.
//!
//! What it says has its own notice ([`Recovery::note`]), shown beside the
//! window's message: no take result or playback message is replaced, and
//! the notice goes once the film is ready again. A take player that failed
//! needs nothing more: the next take opened gets a new one.

use super::RecordingWindow;

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

/// How long a reopened film loads before P, Shift+P, Space or a device
/// change gives it up early and opens it again (one opened as its output
/// went away may neither load nor fail).
pub const EARLY_RETRY_S: f64 = 3.0;

/// How long a reopened film must stay ready (its warm-up done) before its
/// recovery counts as over and the pauses start again from the first.
pub const SETTLED_S: f64 = 2.0;

/// How often the output is looked at while it is not usable, between the
/// changes that are reported.
const POLL_S: f64 = 1.0;

/// The project's film in the window.
#[derive(Debug, Clone)]
pub struct Recovery {
    /// Whether the film is set aside (its player failed, or the beep film
    /// is in its place) and waits to be opened again.
    set_aside: bool,
    /// When it is tried next.
    retry_at: f64,
    /// The pause after the next reopen that fails.
    backoff_s: f64,
    /// Reopens since it was last ready.
    attempts: u32,
    /// When it was opened, and the devices then, until it is ready.
    loading_since: Option<(f64, u64)>,
    /// P, Shift+P or Space was pressed while a reopened film loads.
    nudged: bool,
    /// When a reopened film that is ready counts as recovered.
    settles_at: Option<f64>,
    /// The refusal said for P, Shift+P or Space, taken back once the
    /// film is ready.
    refusal: Option<String>,
    /// The devices and the route the window has set when it was last
    /// tried: a change tries it again at once.
    seen: Option<(u64, Option<String>, Option<u32>)>,
    /// Why its player failed, while it waits.
    reason: Option<String>,
    /// What its recovery says, beside the window's message.
    pub note: Option<String>,
}

impl Default for Recovery {
    fn default() -> Self {
        Self {
            set_aside: false,
            retry_at: 0.0,
            backoff_s: FIRST_RETRY_S,
            attempts: 0,
            loading_since: None,
            nudged: false,
            settles_at: None,
            refusal: None,
            seen: None,
            reason: None,
            note: None,
        }
    }
}

impl Recovery {
    /// The beep film, or a failed player, is in the film's place: it is
    /// opened again as soon as its output can be used.
    pub fn set_aside(&mut self) {
        self.set_aside = true;
        self.retry_at = f64::NEG_INFINITY;
        self.seen = None;
        // The beep film has its own deadline.
        self.loading_since = None;
        self.settles_at = None;
    }

    /// The film was just opened (with the devices at `generation`): it
    /// counts as failed if it does not load in time.
    pub fn opened(&mut self, now: f64, generation: u64) {
        self.loading_since = Some((now, generation));
        self.nudged = false;
    }

    /// Whether it waits for a film to load.
    #[cfg(test)]
    pub fn loading(&self) -> bool {
        self.loading_since.is_some()
    }

    /// P, Shift+P or Space: a film set aside is tried again at the next
    /// tick, whatever the pause; a reopened one still loading may be given
    /// up early (see [`EARLY_RETRY_S`]).
    pub fn nudge(&mut self) {
        if self.set_aside {
            self.retry_at = f64::NEG_INFINITY;
        } else if self.loading_since.is_some() {
            self.nudged = true;
        }
    }

    /// The pause before the next try, and the one after it.
    fn back_off(&mut self, now: f64) -> f64 {
        let pause = self.backoff_s;
        self.retry_at = now + pause;
        self.backoff_s = (pause * 2.0).min(LAST_RETRY_S);
        pause
    }
}

impl RecordingWindow {
    /// Once per tick: stops a playback, a take or a calibration whose
    /// player failed, and opens the film again once that can work.
    pub(super) fn recover_players(&mut self, now: f64) {
        // A failed player set aside is not asked again every tick.
        let mut failure = (!self.film.set_aside || self.calibrating.is_some())
            .then(|| self.media.film_failure())
            .flatten();
        if failure.is_none() {
            failure = self.watch_film_load(now);
        }
        if let Some(reason) = &failure {
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
    }

    /// While a film loads: once it is ready, its notice and any refusal go;
    /// once it has stayed ready past its warm-up, its recovery is over.
    /// Returns why it counts as failed when it took too long, or, reopened,
    /// was given up early.
    fn watch_film_load(&mut self, now: f64) -> Option<String> {
        if let Some(at) = self.film.settles_at
            && now >= at
        {
            self.film = Recovery::default();
            return None;
        }
        let (since, generation) = self.film.loading_since?;
        if self.media.film_ready() {
            self.film.loading_since = None;
            self.film.note = None;
            if self.message.is_some() && self.message == self.film.refusal {
                self.message = None;
            }
            self.film.refusal = None;
            if self.film.attempts > 0 {
                self.film.settles_at = Some(now + SETTLED_S);
            }
            return None;
        }
        let waited = now - since;
        // A key pressed within the first seconds asks nothing yet.
        if waited <= EARLY_RETRY_S {
            self.film.nudged = false;
        }
        let timeout = (LOAD_TIMEOUT_S * f64::from(self.film.attempts.max(1))).min(LONGEST_LOAD_S);
        let early = self.film.attempts > 0
            && waited > EARLY_RETRY_S
            && (self.film.nudged || generation != self.media.device_generation());
        (waited > timeout || early).then(|| "it did not load".to_owned())
    }

    /// The film's player failed: stops what plays on it, and sets it aside
    /// to be opened again.
    fn lose_film(&mut self, reason: &str, now: f64) {
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
        self.film.set_aside = true;
        self.film.loading_since = None;
        self.film.settles_at = None;
        self.film.reason = Some(reason.to_owned());
        self.film.note = Some(if again {
            let pause = self.film.back_off(now);
            format!(
                "The film could not play ({reason}); Takewise tries again in {pause:.0} s and when the output changes. If this goes on, check the film file."
            )
        } else {
            self.film.retry_at = now;
            format!("The film stopped ({reason}); Takewise opens it again.")
        });
    }

    /// Opens the film again, as the window opens it, when it is due (or
    /// the route or the devices changed) and its output can be used: on
    /// the route the window has set and confirmed, never on one missing or
    /// unconfirmed, where a new player would fail at once.
    pub(super) fn reopen_film(&mut self, now: f64) {
        let seen = (
            self.media.device_generation(),
            self.film_output.clone(),
            self.applied_device.as_ref().and_then(|(_, object)| *object),
        );
        if now < self.film.retry_at && self.film.seen.as_ref() == Some(&seen) {
            return;
        }
        self.film.seen = Some(seen);
        if !self.route_usable() {
            self.film.retry_at = self.film.retry_at.max(now + POLL_S);
            if let Some(reason) = &self.film.reason {
                self.film.note = Some(format!(
                    "The film stopped ({reason}); Takewise opens it again once its output is back."
                ));
            }
            return;
        }
        self.film.attempts += 1;
        match self.restore_film() {
            Ok(()) => {
                self.film.set_aside = false;
                let generation = self.media.device_generation();
                self.film.opened(now, generation);
            }
            Err(err) => {
                let pause = self.film.back_off(now);
                self.film.note = Some(format!(
                    "Could not open the film again ({err}); Takewise tries again in {pause:.0} s and when the output changes."
                ));
            }
        }
    }

    /// After a calibration: the project's film back now if its output can
    /// be used, or on a later tick once it can.
    pub(super) fn reopen_film_now(&mut self) {
        self.film.set_aside();
        let now = self.media.host_now_s();
        self.reopen_film(now);
    }

    /// Why a playback or a take cannot start on the film now, if it
    /// cannot: it is set aside (it is then tried again at the next tick)
    /// or failed, or (`loading`, for a take, whose film must start at
    /// once) it is still loading.
    pub(super) fn film_problem(&mut self, loading: bool) -> Option<String> {
        self.film.nudge();
        let why = if self.film.set_aside {
            match (&self.film.reason, self.route_usable()) {
                (Some(reason), false) => format!("{reason}; it waits for its output"),
                (None, false) => "it waits for its output".to_owned(),
                (Some(reason), true) => format!("{reason}; it is being opened again"),
                (None, true) => "it is being opened again".to_owned(),
            }
        } else if let Some(reason) = self.media.film_failure() {
            reason
        } else if loading && !self.media.film_ready() {
            "still loading".to_owned()
        } else {
            return None;
        };
        let refusal = format!("The film is not ready ({why}); try again in a moment.");
        self.film.refusal = Some(refusal.clone());
        Some(refusal)
    }
}
