//! Players that failed. macOS fails a playing `AVPlayerItem` whose output
//! device goes away (status Failed, AVFoundation error -11800), and the
//! item never plays again, even once the device is back and set again
//! (see the smoke test `players_after_output_loss`): only a new player
//! does, and only once the device is there (one opened while it is gone
//! fails the same way). So a failed film is reopened once its output is
//! there again, at most every [`REOPEN_EVERY_S`], as the window opens it
//! (its output, level and warm-up); a playback on
//! a failed player stops and says so; the next take opened gets a new
//! player anyway. During a take nothing is reopened: its film's output
//! went away, which the take notes, and the film is reopened after it.

use super::RecordingWindow;

/// How often a film that keeps failing is reopened.
pub const REOPEN_EVERY_S: f64 = 2.0;

impl RecordingWindow {
    /// Once per tick: stops a playback whose film or take failed, and
    /// reopens a failed film between takes.
    pub(super) fn recover_players(&mut self, now: f64) {
        let film = self.media.film_failure();
        let take = self
            .media
            .take_failure()
            .filter(|_| self.recorder.is_playing());
        if film.is_none() && take.is_none() {
            return;
        }
        if self.recorder.is_recording() {
            return;
        }
        if self.recorder.is_playing() {
            self.recorder.stop_playback(&mut *self.media);
            let what = if film.is_some() { "film" } else { "take" };
            self.message = Some(format!(
                "The playback stopped: the {what} could not play on after its output went away. Play it again."
            ));
        }
        // Not while the output is missing: a player opened then fails too.
        if film.is_none() || self.output_missing.is_some() || now < self.film_reopen_at {
            return;
        }
        self.film_reopen_at = now + REOPEN_EVERY_S;
        if self.calibrating.is_some() {
            // The beep film failed: the measurement ends, and the project's
            // film comes back.
            self.cancel_calibration();
            self.message = Some(
                "Latency calibration cancelled: the output went away while measuring; press Space to measure again."
                    .to_owned(),
            );
            return;
        }
        if !self.restore_film() {
            let said = self.message.take().unwrap_or_default();
            self.message = Some(format!(
                "{said}. The film stopped playing after its output went away; Takewise tries again every {REOPEN_EVERY_S:.0} s."
            ));
        }
    }
}
