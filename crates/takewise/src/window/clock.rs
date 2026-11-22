//! The film's clock during a take and during calibration: one routine that
//! schedules the film once capture runs and then follows the timebase the
//! player really runs on, so calibration measures the window's own path.
//!
//! The order matters: capture starts first and the film is scheduled only
//! once the first buffer arrived, at least [`START_LEAD_S`] ahead, so a slow
//! input device never eats into the pre-roll. Until the scheduled start the
//! scheduled mapping holds (the count-in runs backwards from it); from then
//! on the player's own timebase is read back, and every departure from the
//! mapping in effect (a late start, a stall, a rate change) is recorded in
//! the history that places the captured samples.

use takewise_core::{Timebase, TimebaseHistory};

use super::media::Media;

/// How far ahead the film's start is scheduled, so the player is set up
/// before it.
pub const START_LEAD_S: f64 = 0.3;
/// How long after the scheduled start the player's timebase is first read
/// back: it only switches from stopped to playing at the start.
const READ_BACK_AFTER_S: f64 = 0.05;
/// A film position this far from the mapping in effect is a change.
const POSITION_TOLERANCE_S: f64 = 0.001;
/// A rate this far from the mapping's is a change.
const RATE_TOLERANCE: f64 = 0.001;

#[derive(Debug, Clone, Default)]
pub struct FilmClock {
    history: TimebaseHistory,
    /// Host time of the current start, while the film plays.
    start_host_s: Option<f64>,
    /// Whether the player's timebase was read since the current start.
    read_back: bool,
}

impl FilmClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the film was scheduled (and not stopped since).
    pub fn started(&self) -> bool {
        self.start_host_s.is_some()
    }

    /// Schedules the film to play from `from_s` after holding its frame for
    /// `hold_s`, [`START_LEAD_S`] from now, and records the scheduled
    /// mapping. Call once capture delivers buffers. Returns the scheduled
    /// timebase.
    pub fn start(
        &mut self,
        media: &mut dyn Media,
        from_s: f64,
        hold_s: f64,
    ) -> Result<Timebase, String> {
        let now = media.host_now_s();
        let tb = media.film_play_at(from_s, now + START_LEAD_S + hold_s)?;
        // The first start's mapping also places every sample captured
        // before it: the count-in runs backwards from the start.
        let from = if self.history.is_empty() {
            f64::NEG_INFINITY
        } else {
            now
        };
        self.history.record(from, tb);
        self.start_host_s = Some(tb.host_anchor_s);
        self.read_back = false;
        Ok(tb)
    }

    /// Reads the player's timebase once the film plays and records it when
    /// it departs from the mapping in effect. The first read after a start
    /// takes effect from the start; later ones (stalls, rate changes) from
    /// now. Call once per frame.
    pub fn track(&mut self, media: &dyn Media) {
        let Some(start) = self.start_host_s else {
            return;
        };
        let now = media.host_now_s();
        if now < start + READ_BACK_AFTER_S {
            return;
        }
        let actual = media.film_timebase();
        let changed = self.history.timebase_at(now).is_none_or(|expected| {
            (actual.film_at(now) - expected.film_at(now)).abs() > POSITION_TOLERANCE_S
                || (actual.rate - expected.rate).abs() > RATE_TOLERANCE
        });
        if changed {
            let from = if self.read_back { now } else { start };
            self.history.record(from, actual);
        }
        self.read_back = true;
    }

    /// Pauses the film and records the stop.
    pub fn stop(&mut self, media: &mut dyn Media) {
        media.film_pause();
        self.history
            .record(media.host_now_s(), media.film_timebase());
        self.start_host_s = None;
    }

    /// Film time at `host_s`; `None` before the film was scheduled.
    pub fn film_at(&self, host_s: f64) -> Option<f64> {
        self.history.film_at(host_s)
    }

    /// The mappings that place captured samples on the film.
    pub fn history(&self) -> &TimebaseHistory {
        &self.history
    }
}
