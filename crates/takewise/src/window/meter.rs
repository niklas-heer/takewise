//! The input meter: the level with a 1.5 s peak hold over the −18..−6 dBFS
//! target band, and the near-clipping light that stays red until K or the
//! next take. Also the small per-channel meters of the audio panel.

use gpui_kit::{
    AnyElement, IntoElement, ParentElement as _, Styled as _, div, prelude::FluentBuilder as _, px,
    relative, rgb,
};

use super::colors;

/// The meter's floor.
pub const FLOOR_DBFS: f64 = -60.0;
/// How long the peak hold mark stays before it falls to the level.
pub const HOLD_S: f64 = 1.5;
/// The light turns red at this peak.
pub const NEAR_CLIP_DBFS: f64 = -1.0;
/// A take whose samples reached this peak is marked "clipped" (the verdict
/// rule's threshold).
pub const CLIPPED_DBFS: f64 = -0.1;
/// The shaded target band for speech peaks.
pub const TARGET_BAND_DBFS: (f64, f64) = (-18.0, -6.0);

/// The meter's memory between frames: the held peak and the latched light.
#[derive(Debug, Default)]
pub struct PeakMeter {
    /// The held peak in dBFS and the host time it was reached.
    hold: Option<(f64, f64)>,
    near_clip: bool,
}

impl PeakMeter {
    /// Takes in one frame: `level` is the level shown now, `peak` the
    /// highest peak since the last frame (or of the take so far), both in
    /// dBFS.
    pub fn feed(&mut self, level: Option<f64>, peak: Option<f64>, now: f64) {
        if let Some(level) = level.filter(|l| l.is_finite()) {
            let expired = self.hold.is_none_or(|(_, at)| now - at >= HOLD_S);
            if expired || self.hold.is_some_and(|(held, _)| level >= held) {
                self.hold = Some((level, now));
            }
        }
        if level.into_iter().chain(peak).any(|p| p >= NEAR_CLIP_DBFS) {
            self.near_clip = true;
        }
    }

    /// The held peak, while it holds.
    pub fn hold_dbfs(&self, now: f64) -> Option<f64> {
        self.hold
            .filter(|(_, at)| now - at < HOLD_S)
            .map(|(held, _)| held)
    }

    /// Whether the light is red.
    pub fn near_clip(&self) -> bool {
        self.near_clip
    }

    /// Turns the light off (K, or a new take).
    pub fn clear(&mut self) {
        self.near_clip = false;
    }
}

/// Where `dbfs` sits on the meter, from 0 (floor) to 1 (full scale).
pub fn fraction(dbfs: f64) -> f32 {
    if !dbfs.is_finite() {
        return 0.0;
    }
    #[allow(clippy::cast_possible_truncation)]
    let f = ((dbfs - FLOOR_DBFS) / -FLOOR_DBFS).clamp(0.0, 1.0) as f32;
    f
}

fn level_color(dbfs: f64) -> u32 {
    if dbfs >= NEAR_CLIP_DBFS {
        colors::RECORD
    } else if dbfs > TARGET_BAND_DBFS.1 {
        colors::WARN
    } else {
        colors::GOOD
    }
}

/// A horizontal meter `width` wide: the target band shaded, the level as a
/// bar and the held peak as a thin mark.
pub fn bar(level: Option<f64>, hold: Option<f64>, width: f32, height: f32) -> AnyElement {
    let level = level.unwrap_or(f64::NEG_INFINITY);
    let (low, high) = TARGET_BAND_DBFS;
    let band_from = fraction(low);
    let band_to = fraction(high);
    div()
        .relative()
        .w(px(width))
        .h(px(height))
        .rounded(px(height / 2.0))
        .bg(rgb(colors::SELECTED))
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .top_0()
                .h_full()
                .left(relative(band_from))
                .w(relative(band_to - band_from))
                .bg(rgb(0x0026_3a30)),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .h_full()
                .w(relative(fraction(level)))
                .bg(rgb(level_color(level))),
        )
        .when_some(hold.filter(|h| h.is_finite()), |el, held| {
            el.child(
                div()
                    .absolute()
                    .top_0()
                    .h_full()
                    .left(relative(fraction(held)))
                    .ml(px(-1.))
                    .w(px(2.))
                    .bg(rgb(level_color(held))),
            )
        })
        .into_any_element()
}

/// The main meter: the bar, "mic", and the near-clipping light.
pub fn main_meter(level: Option<f64>, hold: Option<f64>, near_clip: bool) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .child(bar(level, hold, 140., 8.))
        .child("mic")
        .child(div().size(px(10.)).rounded_full().bg(rgb(if near_clip {
            colors::RECORD
        } else {
            colors::SELECTED
        })))
        .when(near_clip, |el| {
            el.child(
                div()
                    .text_color(rgb(colors::RECORD))
                    .child("near clipping · K clears"),
            )
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_keeps_the_peak_for_one_and_a_half_seconds() {
        let mut meter = PeakMeter::default();
        meter.feed(Some(-10.0), None, 100.0);
        meter.feed(Some(-30.0), None, 100.5);
        assert_eq!(meter.hold_dbfs(100.5), Some(-10.0));
        meter.feed(Some(-30.0), None, 101.6);
        assert_eq!(meter.hold_dbfs(101.6), Some(-30.0), "the hold fell");
        meter.feed(Some(-5.0), None, 101.7);
        assert_eq!(
            meter.hold_dbfs(101.7),
            Some(-5.0),
            "a higher peak replaces it"
        );
    }

    #[test]
    fn light_latches_at_minus_one_dbfs() {
        let mut meter = PeakMeter::default();
        meter.feed(Some(-1.5), None, 100.0);
        assert!(!meter.near_clip());
        meter.feed(Some(-20.0), Some(-0.5), 100.1);
        assert!(meter.near_clip(), "a peak between frames counts");
        meter.feed(Some(-40.0), Some(-40.0), 105.0);
        assert!(meter.near_clip(), "stays red");
        meter.clear();
        assert!(!meter.near_clip());
    }

    #[test]
    fn fraction_spans_floor_to_full_scale() {
        assert!(fraction(f64::NEG_INFINITY).abs() < f32::EPSILON);
        assert!(fraction(-60.0).abs() < f32::EPSILON);
        assert!((fraction(-30.0) - 0.5).abs() < 1e-6);
        assert!((fraction(3.0) - 1.0).abs() < f32::EPSILON);
    }
}
