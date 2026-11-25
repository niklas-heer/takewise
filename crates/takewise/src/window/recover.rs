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

/// How long a reopened film may load before it counts as failed; longer
/// with every attempt, so a film slow to load gets there.
pub const LOAD_TIMEOUT_S: f64 = 3.0;

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
    /// When it was reopened, until it is ready.
    loading_since: Option<f64>,
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
    }

    /// The film was just opened: it counts as failed if it does not load
    /// in time.
    pub fn opened(&mut self, now: f64) {
        self.loading_since = Some(now);
    }

    /// P, Shift+P or Space on a film set aside: tried again at the next
    /// tick, whatever the pause.
    pub fn nudge(&mut self) {
        if self.set_aside {
            self.retry_at = f64::NEG_INFINITY;
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
        let mut failure = self.media.film_failure();
        if let Some(since) = self.film.loading_since {
            let timeout = LOAD_TIMEOUT_S * f64::from(self.film.attempts.max(1));
            if failure.is_none() && self.media.film_ready() {
                // Back: nothing left to say or retry.
                self.film = Recovery::default();
            } else if failure.is_none() && now - since > timeout {
                failure = Some("it did not load".to_owned());
            }
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
        } else if let Some(reason) = self.media.take_failure()
            && self.recorder.is_playing()
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
            if self.film.attempts == 0
                && let Some(reason) = &self.film.reason
            {
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
                self.film.opened(now);
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
            "it is opened again once its output can be used".to_owned()
        } else if let Some(reason) = self.media.film_failure() {
            reason
        } else if loading && !self.media.film_ready() {
            "still loading".to_owned()
        } else {
            return None;
        };
        Some(format!(
            "The film is not ready ({why}); try again in a moment."
        ))
    }
}
