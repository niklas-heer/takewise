//! Timing maps between host time, film time and a line's slot.
//!
//! The player's timebase maps host time (seconds of the host clock) to film
//! time. Capture buffers carry the host time of their first sample; each
//! sample is placed on the film with the timebase that was valid when it was
//! captured, so a seek or restart while a buffer is in flight never shifts it.

use crate::script::Script;

/// A linear host-to-film mapping, as read from the player's timebase.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timebase {
    /// Film seconds per host second: 1.0 while playing, 0.0 while paused.
    pub rate: f64,
    pub film_anchor_s: f64,
    pub host_anchor_s: f64,
}

impl Timebase {
    /// Film seconds at host time `host_s`.
    pub fn film_at(&self, host_s: f64) -> f64 {
        self.film_anchor_s + self.rate * (host_s - self.host_anchor_s)
    }
}

/// The timebases a take ran under, each with the host time it took effect.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimebaseHistory {
    entries: Vec<(f64, Timebase)>,
}

impl TimebaseHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that `tb` holds from host time `at_host_s` on. Entries may be
    /// recorded out of order; they are kept sorted by `at_host_s`.
    pub fn record(&mut self, at_host_s: f64, tb: Timebase) {
        let idx = self.entries.partition_point(|(at, _)| *at <= at_host_s);
        self.entries.insert(idx, (at_host_s, tb));
    }

    /// Film time at `host_s` under the timebase valid at that host time;
    /// `None` before the first recorded timebase.
    pub fn film_at(&self, host_s: f64) -> Option<f64> {
        self.timebase_at(host_s).map(|tb| tb.film_at(host_s))
    }

    /// The timebase valid at `host_s`; `None` before the first one.
    pub fn timebase_at(&self, host_s: f64) -> Option<Timebase> {
        self.entry_at(host_s).map(|(_, tb)| tb)
    }

    /// The timebase valid at `host_s` and the host time it took effect.
    pub fn entry_at(&self, host_s: f64) -> Option<(f64, Timebase)> {
        let idx = self.entries.partition_point(|(at, _)| *at <= host_s);
        self.entries.get(idx.checked_sub(1)?).copied()
    }

    /// The timebase valid just before `host_s`: the one a change taking
    /// effect at `host_s` ends.
    pub fn timebase_before(&self, host_s: f64) -> Option<Timebase> {
        let idx = self.entries.partition_point(|(at, _)| *at < host_s);
        self.entries.get(idx.checked_sub(1)?).map(|(_, tb)| *tb)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Film time of sample `index` in a capture buffer whose first sample has
/// the callback host time `buffer_host_s`: the film time the narrator heard
/// when the sound reached the microphone.
///
/// `input_latency_s` is the device's input latency plus safety offset plus
/// stream latency; `offset_ms` is the calibrated `latency_offset_ms`.
pub fn sample_film_time(
    history: &TimebaseHistory,
    buffer_host_s: f64,
    index: usize,
    sample_rate: u32,
    input_latency_s: f64,
    offset_ms: f64,
) -> Option<f64> {
    let host_s = buffer_host_s + index as f64 / f64::from(sample_rate) - input_latency_s;
    history
        .film_at(host_s)
        .map(|film| film + offset_ms / 1000.0)
}

/// How far contiguous audio drifts from the film: the largest distance, in
/// film seconds, between where `history` places host times in
/// `from_host_s..to_host_s` and where audio placed at `from_host_s` and
/// running at rate 1 places them. A change taking effect at `to_host_s` or
/// later is not counted, so measuring up to the film's end leaves out the
/// player halting there. Zero when the film played steadily (or before any
/// timebase).
pub fn film_shift_s(history: &TimebaseHistory, from_host_s: f64, to_host_s: f64) -> f64 {
    let Some(start) = history.film_at(from_host_s) else {
        return 0.0;
    };
    let shift = |film: f64, host: f64| (film - (start + (host - from_host_s))).abs();
    let mut max = history
        .timebase_before(to_host_s)
        .map_or(0.0, |tb| shift(tb.film_at(to_host_s), to_host_s));
    for (idx, (at, tb)) in history.entries.iter().enumerate() {
        if *at <= from_host_s || *at >= to_host_s {
            continue;
        }
        // Both sides of the change: the new mapping, and the one it ends.
        max = max.max(shift(tb.film_at(*at), *at));
        if let Some((_, before)) = idx.checked_sub(1).and_then(|i| history.entries.get(i)) {
            max = max.max(shift(before.film_at(*at), *at));
        }
    }
    max
}

/// How far a buffer may start from where the previous one ended before the
/// difference counts as a gap.
pub const GAP_TOLERANCE_S: f64 = 0.001;

/// A stretch of captured audio that never arrived and was filled with
/// silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dropout {
    /// Index of the first silent sample in the take.
    pub at_sample: u64,
    /// How many samples were missing.
    pub missing: u64,
}

/// Places capture buffers end to end. A buffer that starts later than the
/// previous one ended (by its host time, beyond [`GAP_TOLERANCE_S`]) had
/// audio lost before it, such as after an overload: the gap is filled with
/// silence so later audio keeps its timing, and counted.
#[derive(Debug, Clone)]
pub struct Assembler {
    sample_rate: u32,
    first_host_s: Option<f64>,
    next_host_s: Option<f64>,
    samples: u64,
    dropouts: Vec<Dropout>,
}

impl Assembler {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            first_host_s: None,
            next_host_s: None,
            samples: 0,
            dropouts: Vec::new(),
        }
    }

    /// Takes a buffer of `len` samples whose first sample has host time
    /// `host_s`; returns how many samples of silence go before it.
    pub fn push(&mut self, host_s: f64, len: usize) -> usize {
        let rate = f64::from(self.sample_rate);
        let mut silence = 0;
        match self.next_host_s {
            None => self.first_host_s = Some(host_s),
            Some(next) if host_s - next > GAP_TOLERANCE_S => {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let missing = ((host_s - next) * rate).round() as usize;
                self.dropouts.push(Dropout {
                    at_sample: self.samples,
                    missing: missing as u64,
                });
                self.samples += missing as u64;
                silence = missing;
            }
            Some(_) => {}
        }
        self.samples += len as u64;
        self.next_host_s = Some(host_s + len as f64 / rate);
        silence
    }

    /// Host time of the first sample, once a buffer arrived.
    pub fn first_host_s(&self) -> Option<f64> {
        self.first_host_s
    }

    /// Samples placed so far, silence included.
    pub fn samples(&self) -> u64 {
        self.samples
    }

    pub fn dropouts(&self) -> &[Dropout] {
        &self.dropouts
    }
}

/// How one take plays the film and when it stops.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordPlan {
    /// Film time the film starts playing from.
    pub film_from_s: f64,
    /// How long the first frame is held before the film plays, when the
    /// line starts less than the pre-roll into the film.
    pub hold_first_frame_s: f64,
    /// Film time at which capture stops on its own.
    pub capture_stop_film_s: f64,
}

/// Plans a take of a line starting at `line_start` whose slot ends at
/// `slot_end`.
pub fn plan_take(line_start: f64, slot_end: f64, preroll_s: f64, tail_s: f64) -> RecordPlan {
    RecordPlan {
        film_from_s: (line_start - preroll_s).max(0.0),
        hold_first_frame_s: (preroll_s - line_start).max(0.0),
        capture_stop_film_s: slot_end + tail_s,
    }
}

/// The take's `line_offset_s`: film time of its first sample relative to
/// the line's start. Negative when capture began before the line (always,
/// since capture starts with the pre-roll).
pub fn line_offset_s(first_sample_film_s: f64, line_start: f64) -> f64 {
    first_sample_film_s - line_start
}

/// How many characters of `text` the pace guide fills at `now_film_s`: an
/// even pace by character count across the slot, clamped to the text.
pub fn pace_fill(text: &str, slot_start: f64, slot_end: f64, now_film_s: f64) -> usize {
    let total = text.chars().count();
    let span = slot_end - slot_start;
    if span.is_nan() || span <= 0.0 {
        return if now_film_s >= slot_start { total } else { 0 };
    }
    let fraction = ((now_film_s - slot_start) / span).clamp(0.0, 1.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let filled = (fraction * total as f64).round() as usize;
    filled.min(total)
}

/// Where **M** may put line `idx`'s start: between the previous line's
/// start (or the film's start) and the line's own slot end, a millisecond
/// inside each bound.
pub fn clamp_mark(script: &Script, idx: usize, film_s: f64, film_duration: f64) -> f64 {
    let lower = idx
        .checked_sub(1)
        .and_then(|prev| script.lines.get(prev))
        .map_or(0.0, |prev| prev.start + 0.001);
    let upper = script.slot_end(idx, film_duration) - 0.001;
    if upper < lower {
        return lower;
    }
    film_s.clamp(lower, upper)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::Line;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    const PLAYING_FROM_ZERO: Timebase = Timebase {
        rate: 1.0,
        film_anchor_s: 0.0,
        host_anchor_s: 0.0,
    };

    #[test]
    fn film_shift_measures_a_stall_and_a_jump() {
        let mut history = TimebaseHistory::new();
        history.record(f64::NEG_INFINITY, PLAYING_FROM_ZERO);
        // Steady playback: no shift.
        assert!(close(film_shift_s(&history, 1.0, 5.0), 0.0));
        // Stalled at film 3.0 from host 3.0 to 3.1, then playing on.
        history.record(
            3.0,
            Timebase {
                rate: 0.0,
                film_anchor_s: 3.0,
                host_anchor_s: 3.0,
            },
        );
        history.record(
            3.1,
            Timebase {
                rate: 1.0,
                film_anchor_s: 3.0,
                host_anchor_s: 3.1,
            },
        );
        assert!(close(film_shift_s(&history, 1.0, 5.0), 0.1));
        assert!(close(film_shift_s(&history, 1.0, 3.05), 0.05), "mid-stall");
        assert!(close(film_shift_s(&history, 1.0, 2.0), 0.0), "before it");
        // A jump forward by 0.3 at host 4.0.
        history.record(
            4.0,
            Timebase {
                rate: 1.0,
                film_anchor_s: 4.2,
                host_anchor_s: 4.0,
            },
        );
        assert!(close(film_shift_s(&history, 1.0, 5.0), 0.2));
        assert!(close(film_shift_s(&TimebaseHistory::new(), 1.0, 5.0), 0.0));
    }

    #[test]
    fn film_shift_ends_before_its_end() {
        let mut history = TimebaseHistory::new();
        history.record(f64::NEG_INFINITY, PLAYING_FROM_ZERO);
        // Halted at film 20.0, seen 12 ms after the film reached it.
        let halted = Timebase {
            rate: 0.0,
            film_anchor_s: 20.0,
            host_anchor_s: 20.0,
        };
        history.record(20.012, halted);
        assert!(
            close(film_shift_s(&history, 1.0, 20.0), 0.0),
            "up to the end"
        );
        assert!(
            close(film_shift_s(&history, 1.0, 20.012), 0.0),
            "the halt is left out"
        );
        assert!(close(film_shift_s(&history, 1.0, 20.1), 0.1));
        assert_eq!(history.entry_at(20.05), Some((20.012, halted)));
        assert_eq!(history.timebase_before(20.012), Some(PLAYING_FROM_ZERO));
        assert_eq!(history.timebase_at(20.012), Some(halted));
    }

    #[test]
    fn maps_with_timebase_valid_at_capture() {
        let mut history = TimebaseHistory::new();
        // Playing film 0 at host 0, then a seek to film 30 at host 10.
        history.record(0.0, PLAYING_FROM_ZERO);
        history.record(
            10.0,
            Timebase {
                rate: 1.0,
                film_anchor_s: 30.0,
                host_anchor_s: 10.0,
            },
        );
        let before = history.film_at(9.99).unwrap();
        assert!(close(before, 9.99), "{before}");
        let after = history.film_at(10.5).unwrap();
        assert!(close(after, 30.5), "{after}");
        // A buffer whose callback came after the seek but whose samples were
        // captured before it still maps with the old timebase.
        let film = sample_film_time(&history, 10.02, 0, 48_000, 0.03, 0.0).unwrap();
        assert!(close(film, 9.99), "{film}");
    }

    #[test]
    fn history_is_empty_before_the_first_timebase() {
        let mut history = TimebaseHistory::new();
        assert_eq!(history.film_at(1.0), None);
        history.record(2.0, PLAYING_FROM_ZERO);
        assert_eq!(history.film_at(1.0), None);
        assert!(history.film_at(2.0).is_some());
    }

    #[test]
    fn history_sorts_out_of_order_records() {
        let mut history = TimebaseHistory::new();
        let paused = Timebase {
            rate: 0.0,
            film_anchor_s: 7.0,
            host_anchor_s: 5.0,
        };
        history.record(5.0, paused);
        history.record(0.0, PLAYING_FROM_ZERO);
        assert!(close(history.film_at(4.0).unwrap(), 4.0));
        assert!(close(history.film_at(6.0).unwrap(), 7.0));
    }

    #[test]
    fn sample_film_time_applies_latency_and_offset() {
        let mut history = TimebaseHistory::new();
        history.record(0.0, PLAYING_FROM_ZERO);
        let film = sample_film_time(&history, 5.0, 480, 48_000, 0.051, -31.5).unwrap();
        let expected = 5.0 + 0.01 - 0.051 - 0.0315;
        assert!(close(film, expected), "{film} != {expected}");
    }

    #[test]
    fn first_line_holds_first_frame() {
        let plan = plan_take(0.0, 3.0, 2.0, 1.5);
        assert_eq!(plan.film_from_s, 0.0);
        assert_eq!(plan.hold_first_frame_s, 2.0);
        assert_eq!(plan.capture_stop_film_s, 4.5);

        // A line 0.5 s in holds for the rest of the pre-roll.
        let plan = plan_take(0.5, 3.0, 2.0, 1.5);
        assert_eq!(plan.film_from_s, 0.0);
        assert_eq!(plan.hold_first_frame_s, 1.5);

        // Capture starts with the count-in, so the first sample lies before
        // film zero and the offset is the whole pre-roll before the line.
        let mut history = TimebaseHistory::new();
        // Film 0 starts at host 102 (count-in started at host 100).
        history.record(
            100.0,
            Timebase {
                rate: 1.0,
                film_anchor_s: 0.0,
                host_anchor_s: 102.0,
            },
        );
        let first = sample_film_time(&history, 100.0, 0, 48_000, 0.0, 0.0).unwrap();
        assert!(close(line_offset_s(first, 0.0), -2.0));
    }

    #[test]
    fn plan_take_regular_line() {
        let plan = plan_take(10.0, 14.0, 2.0, 1.5);
        assert_eq!(
            plan,
            RecordPlan {
                film_from_s: 8.0,
                hold_first_frame_s: 0.0,
                capture_stop_film_s: 15.5,
            }
        );
        assert!(close(line_offset_s(7.95, 10.0), -2.05));
    }

    #[test]
    fn pace_fill_bounds() {
        let text = "Every family has a story."; // 25 characters
        assert_eq!(pace_fill(text, 10.0, 14.0, 9.0), 0);
        assert_eq!(pace_fill(text, 10.0, 14.0, 10.0), 0);
        assert_eq!(pace_fill(text, 10.0, 14.0, 14.0), 25);
        assert_eq!(pace_fill(text, 10.0, 14.0, 20.0), 25);
        let half = pace_fill(text, 10.0, 14.0, 12.0);
        assert!((12..=13).contains(&half), "{half}");
        // Characters, not bytes.
        assert_eq!(pace_fill("Grüße", 0.0, 1.0, 1.0), 5);
        // A degenerate slot fills at its start.
        assert_eq!(pace_fill(text, 5.0, 5.0, 4.0), 0);
        assert_eq!(pace_fill(text, 5.0, 5.0, 5.0), 25);
    }

    fn line(id: &str, start: f64, end: Option<f64>) -> Line {
        Line {
            id: id.to_owned(),
            start,
            end,
            text: "words".to_owned(),
            note: None,
        }
    }

    #[test]
    fn clamp_mark_between_neighbours() {
        let script = Script {
            lines: vec![
                line("a", 1.0, None),
                line("b", 5.0, None),
                line("c", 9.0, Some(12.0)),
            ],
        };
        // Inside the bounds: unchanged.
        assert!(close(clamp_mark(&script, 1, 6.0, 20.0), 6.0));
        // Before the previous line's start.
        assert!(close(clamp_mark(&script, 1, 0.5, 20.0), 1.001));
        // Past its own slot end (the next line's start).
        assert!(close(clamp_mark(&script, 1, 9.5, 20.0), 8.999));
        // The first line may go to the film's start, not below.
        assert!(close(clamp_mark(&script, 0, -1.0, 20.0), 0.0));
        // The last line is bounded by its explicit end.
        assert!(close(clamp_mark(&script, 2, 15.0, 20.0), 11.999));
    }

    #[test]
    fn assembler_keeps_contiguous_buffers_together() {
        let mut gaps = Assembler::new(48_000);
        assert_eq!(gaps.push(10.0, 480), 0);
        // 0.5 ms of timestamp jitter is no gap.
        assert_eq!(gaps.push(10.0105, 480), 0);
        assert_eq!(gaps.push(10.0195, 480), 0);
        assert_eq!(gaps.samples(), 1440);
        assert_eq!(gaps.dropouts(), &[]);
        assert_eq!(gaps.first_host_s(), Some(10.0));
    }

    #[test]
    fn assembler_fills_gaps_with_silence() {
        let mut gaps = Assembler::new(48_000);
        gaps.push(10.0, 480);
        // The next buffer was due at 10.01 but came 25 ms later.
        assert_eq!(gaps.push(10.035, 480), 1200);
        assert_eq!(gaps.samples(), 480 + 1200 + 480);
        assert_eq!(
            gaps.dropouts(),
            &[Dropout {
                at_sample: 480,
                missing: 1200
            }]
        );
        // Timing continues from the late buffer.
        assert_eq!(gaps.push(10.045, 480), 0);
        assert_eq!(gaps.dropouts().len(), 1);
    }
}
