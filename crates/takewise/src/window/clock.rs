//! The film's clock during a take and during calibration: one routine that
//! schedules the film once capture runs and then follows the timebase the
//! player really runs on, so calibration measures the window's own path.
//!
//! The order matters: capture starts first and the film is scheduled only
//! once the first buffer arrived, at least [`START_LEAD_S`] ahead, so a slow
//! input device never eats into the pre-roll. Until the scheduled start the
//! scheduled mapping holds (the count-in runs backwards from it); from then
//! on the player's own timebase is read back. The first read of a playing
//! player replaces the scheduled mapping, count-in included, so a late start
//! moves the whole take; a player still stopped is not read until it plays.
//! Every later departure (a stall, a rate change) is recorded from the
//! moment it is seen. This history places the captured samples, for a take
//! and for the calibration alike.

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
    /// Host time from which the current start's scheduled mapping holds.
    scheduled_from_s: f64,
    /// Whether the player was read playing since the current start, and
    /// whether a probe saw it moving (which places nothing).
    read_back: bool,
    moved: bool,
    /// The mapping a start would have scheduled, when the player refused to
    /// play: it places the take, though the film never follows it.
    assumed: Option<Timebase>,
}

impl FilmClock {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the film was scheduled (and not stopped since).
    pub fn started(&self) -> bool {
        self.start_host_s.is_some()
    }

    /// Whether the player was read back playing since the current start:
    /// the film really ran, not only was scheduled. A start still to come,
    /// or a player that stalled, has not.
    pub fn ran(&self) -> bool {
        self.start_host_s.is_some() && (self.read_back || self.moved)
    }

    /// Whether samples can be placed: the film was scheduled, or a refused
    /// start's mapping is assumed.
    pub fn placed(&self) -> bool {
        !self.history.is_empty()
    }

    /// Records the mapping [`FilmClock::start`] would have scheduled at
    /// `now`, for a player that refused to play: the take is placed by it,
    /// and [`FilmClock::unplayed_lateness_s`] says how far it ran ahead of
    /// the film that never moved. Only before any start.
    pub fn assume(&mut self, now: f64, from_s: f64, hold_s: f64) -> Timebase {
        let tb = Timebase {
            rate: 1.0,
            film_anchor_s: from_s,
            host_anchor_s: now + START_LEAD_S + hold_s,
        };
        self.history.record(f64::NEG_INFINITY, tb);
        self.assumed = Some(tb);
        tb
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
        self.scheduled_from_s = from;
        self.start_host_s = Some(tb.host_anchor_s);
        self.read_back = false;
        self.moved = false;
        Ok(tb)
    }

    /// Reads the player's timebase once the film plays and records it when
    /// it departs from the mapping in effect. The first read of a playing
    /// player after a start replaces the scheduled mapping wherever it held
    /// (for the first start also the count-in before it: contiguous audio
    /// follows the film as it really started); a stopped one is ignored
    /// until the player plays. Later reads (stalls, rate changes) take
    /// effect from now. Call once per frame.
    pub fn track(&mut self, media: &dyn Media) {
        let Some(start) = self.start_host_s else {
            return;
        };
        let now = media.host_now_s();
        if now < start + READ_BACK_AFTER_S {
            return;
        }
        let actual = media.film_timebase();
        if !self.read_back && actual.rate <= 0.0 {
            return;
        }
        let changed = self.history.timebase_at(now).is_none_or(|expected| {
            (actual.film_at(now) - expected.film_at(now)).abs() > POSITION_TOLERANCE_S
                || (actual.rate - expected.rate).abs() > RATE_TOLERANCE
        });
        if changed {
            let from = if self.read_back {
                now
            } else {
                self.scheduled_from_s
            };
            self.history.record(from, actual);
        }
        self.read_back = true;
    }

    /// Asks the player, at a decision (calling a start off, stopping a
    /// take), whether the film moves, from its start on and without
    /// waiting [`READ_BACK_AFTER_S`]: one that moved counts as played. It
    /// places nothing: the mapping is read by [`FilmClock::track`] only.
    pub fn probe(&mut self, media: &dyn Media) {
        if self
            .start_host_s
            .is_some_and(|start| media.host_now_s() >= start)
            && media.film_timebase().rate > 0.0
        {
            self.moved = true;
        }
    }

    /// How far the film is behind its schedule at `host_s` when the player
    /// was never read playing since the current start, although it should
    /// have been: the scheduled mapping then still places the take, off by
    /// at least this much. `None` once it played, or before it was due. For
    /// an assumed mapping, how far it ran past its start.
    pub fn unplayed_lateness_s(&self, media: &dyn Media, host_s: f64) -> Option<f64> {
        if let Some(tb) = self.assumed {
            return Some((tb.film_at(host_s) - tb.film_anchor_s).max(0.0));
        }
        let start = self.start_host_s?;
        if self.read_back || host_s < start + READ_BACK_AFTER_S {
            return None;
        }
        let scheduled = self.history.timebase_at(host_s)?;
        Some((scheduled.film_at(host_s) - media.film_timebase().film_at(host_s)).abs())
    }

    /// How long past the current start the player has not been read
    /// playing; `None` once it played, or before the start.
    pub fn overdue_s(&self, host_s: f64) -> Option<f64> {
        let start = self.start_host_s?;
        (!self.read_back && host_s >= start).then_some(host_s - start)
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
