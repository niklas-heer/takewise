//! The prompter: film time, the slot running out, the input meter, the
//! current line large with the pace guide filling it, the next line dimmed,
//! and any message or reason recording is blocked.
//!
//! Up to three rows of the line show at full size. A longer line scrolls
//! smoothly so the row with the pace fill's current word stays in the
//! middle; the first and last rows hold still, and a retake starts from the
//! top.

use gpui_kit::base::input::{Input, InputState};
use gpui_kit::{
    AnyElement, Entity, HighlightStyle, IntoElement, ParentElement as _, Styled as _, StyledText,
    TextLayout, div, prelude::FluentBuilder as _, px, relative, rgb,
};
use takewise_core::pace_fill;

use super::{colors, meter};

/// Rows of the current line shown at full size.
pub const VISIBLE_ROWS: usize = 3;
/// The current line's row height in pixels.
const ROW_PX: f32 = 44.0;
/// How long one scroll step eases.
pub const SCROLL_S: f64 = 0.2;

/// How many rows to scroll a line of `rows` rows shown `visible` rows at a
/// time, so that `row_of_current` sits on the middle row: none while the
/// current word is in the first rows, at most `rows - visible` once it
/// reaches the last ones.
pub fn scroll_offset(row_of_current: usize, rows: usize, visible: usize) -> f32 {
    if rows <= visible || visible == 0 {
        return 0.0;
    }
    let middle = (visible - 1) / 2;
    #[allow(clippy::cast_precision_loss)]
    let offset = row_of_current.saturating_sub(middle).min(rows - visible) as f32;
    offset
}

/// The scroll position, easing between targets.
#[derive(Debug, Default, Clone, Copy)]
pub struct Scroll {
    from: f32,
    to: f32,
    started_s: f64,
}

impl Scroll {
    /// The offset in rows at host time `now`.
    pub fn at(&self, now: f64) -> f32 {
        let t = ((now - self.started_s) / SCROLL_S).clamp(0.0, 1.0);
        // Ease out (cubic).
        #[allow(clippy::cast_possible_truncation)]
        let eased = (1.0 - (1.0 - t).powi(3)) as f32;
        self.from + (self.to - self.from) * eased
    }

    /// Eases toward `target` from where the scroll is now. With `forward`
    /// (during a take) it never moves back.
    pub fn aim(&mut self, target: f32, now: f64, forward: bool) {
        let target = if forward { target.max(self.to) } else { target };
        if (target - self.to).abs() > f32::EPSILON {
            self.from = self.at(now);
            self.to = target;
            self.started_s = now;
        }
    }

    /// Back to the top at once (a new take or another line).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// The row of the line's laid-out text that holds the character at
/// `byte` (the last one filled), and how many rows there are; `None` until
/// the text has been laid out.
pub fn current_row(layout: &TextLayout, text: &str, filled: usize) -> Option<(usize, usize)> {
    let last = text[..filled.min(text.len())]
        .char_indices()
        .last()
        .map_or(0, |(i, _)| i);
    let line_height = f32::from(layout.line_height());
    if line_height <= 0.0 {
        return None;
    }
    let bounds = layout.bounds();
    let position = layout.position_for_index(last)?;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let row = ((f32::from(position.y - bounds.top()) / line_height)
        .floor()
        .max(0.0)) as usize;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rows = ((f32::from(bounds.size.height) / line_height)
        .round()
        .max(1.0)) as usize;
    Some((row, rows.max(row + 1)))
}

/// The byte index in the reading's text up to which the pace guide has
/// filled it at film time `film_s`.
pub fn filled_bytes(reading: Reading<'_>, film_s: f64) -> usize {
    let chars = pace_fill(reading.text, reading.start, reading.end, film_s);
    byte_index(reading.text, chars)
}

/// The line to read and its slot.
#[derive(Debug, Clone, Copy)]
pub struct Reading<'a> {
    pub text: &'a str,
    pub start: f64,
    pub end: f64,
}

pub struct Prompter<'a> {
    pub film_s: f64,
    /// The line as a take started with it while it records, else the
    /// current line.
    pub reading: Option<Reading<'a>>,
    pub note: Option<&'a str>,
    pub next: Option<&'a str>,
    /// The input level and the held peak in dBFS.
    pub level_dbfs: Option<f64>,
    pub hold_dbfs: Option<f64>,
    pub near_clip: bool,
    pub recording: bool,
    pub blocked: Option<&'a str>,
    pub message: Option<&'a str>,
    pub editor: Option<&'a Entity<InputState>>,
    /// How many rows the current line is scrolled.
    pub scroll_rows: f32,
}

/// `m:ss.s`; negative times (before the film) get a minus sign.
pub fn timecode(secs: f64) -> String {
    let sign = if secs < 0.0 { "-" } else { "" };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let tenths = (secs.abs() * 10.0).round() as u64;
    format!(
        "{sign}{}:{:02}.{}",
        tenths / 600,
        (tenths / 10) % 60,
        tenths % 10
    )
}

/// The byte index after the first `chars` characters of `text`.
fn byte_index(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(i, _)| i)
}

fn bar(fraction: f64, width: f32, color: u32) -> AnyElement {
    #[allow(clippy::cast_possible_truncation)]
    let fill = fraction.clamp(0.0, 1.0) as f32;
    div()
        .w(px(width))
        .h(px(8.))
        .rounded(px(4.))
        .bg(rgb(colors::SELECTED))
        .child(
            div()
                .h_full()
                .w(relative(fill))
                .rounded(px(4.))
                .bg(rgb(color)),
        )
        .into_any_element()
}

impl Prompter<'_> {
    /// Film time, the slot bar and the level meter.
    fn status(&self) -> AnyElement {
        let slot = self.reading.map(|line| {
            let span = line.end - line.start;
            let progress = if span > 0.0 {
                (self.film_s - line.start) / span
            } else {
                0.0
            };
            (span, progress)
        });
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(14.))
            .text_size(px(15.))
            .text_color(rgb(colors::DIM))
            .child(
                div()
                    .text_color(rgb(if self.recording {
                        colors::RECORD
                    } else {
                        colors::TEXT
                    }))
                    .child(timecode(self.film_s)),
            )
            .when_some(slot, |el, (span, progress)| {
                el.child(bar(progress, 260., colors::ACCENT))
                    .child(format!("slot {span:.1} s"))
            })
            .child(div().flex_1())
            .child(meter::main_meter(
                self.level_dbfs,
                self.hold_dbfs,
                self.near_clip,
            ))
            .into_any_element()
    }

    /// The prompter, and the layout of the current line's text (read on
    /// the next frame to find the row of the current word).
    pub fn render(self) -> (AnyElement, Option<TextLayout>) {
        let status = self.status();
        let (filled, text) = self.reading.map_or((0, String::new()), |line| {
            (filled_bytes(line, self.film_s), line.text.to_owned())
        });
        let mut layout = None;

        let current = if let Some(editor) = self.editor {
            div()
                .px(px(8.))
                .py(px(6.))
                .rounded(px(6.))
                .border_1()
                .border_color(rgb(colors::ACCENT))
                .text_size(px(30.))
                .child(Input::new(editor))
                .into_any_element()
        } else {
            let styled = StyledText::new(text).with_highlights([(
                0..filled,
                HighlightStyle {
                    color: Some(rgb(colors::TEXT).into()),
                    ..HighlightStyle::default()
                },
            )]);
            layout = Some(styled.layout().clone());
            #[allow(clippy::cast_precision_loss)]
            let window_px = ROW_PX * VISIBLE_ROWS as f32;
            div()
                .max_h(px(window_px))
                .overflow_hidden()
                .child(
                    div()
                        .relative()
                        .top(px(-self.scroll_rows * ROW_PX))
                        .text_size(px(34.))
                        .line_height(px(ROW_PX))
                        .text_color(rgb(colors::FAINT))
                        .child(styled),
                )
                .into_any_element()
        };

        let element = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .px(px(28.))
            .py(px(18.))
            .min_h(px(250.))
            .child(status)
            .when_some(self.blocked, |el, blocked| {
                el.child(
                    div()
                        .px(px(12.))
                        .py(px(8.))
                        .rounded(px(6.))
                        .bg(rgb(0x003a_1517))
                        .text_color(rgb(colors::RECORD))
                        .text_size(px(15.))
                        .child(format!("Recording is off. {blocked}")),
                )
            })
            .child(current)
            .when_some(self.note, |el, note| {
                el.child(
                    div()
                        .text_size(px(15.))
                        .italic()
                        .text_color(rgb(colors::DIM))
                        .child(note.to_owned()),
                )
            })
            .when_some(self.next, |el, next| {
                el.child(
                    div()
                        .text_size(px(18.))
                        .text_color(rgb(colors::FAINT))
                        .child(format!("next: {next}")),
                )
            })
            .when_some(self.message, |el, message| {
                el.child(
                    div()
                        .text_size(px(14.))
                        .text_color(rgb(colors::WARN))
                        .child(message.to_owned()),
                )
            })
            .into_any_element();
        (element, layout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timecode_formats_minutes_seconds_tenths() {
        assert_eq!(timecode(0.0), "0:00.0");
        assert_eq!(timecode(5.24), "0:05.2");
        assert_eq!(timecode(65.0), "1:05.0");
        assert_eq!(timecode(-1.5), "-0:01.5");
    }

    #[test]
    fn scroll_is_monotonic() {
        let offsets: Vec<f32> = (0..5).map(|row| scroll_offset(row, 5, 3)).collect();
        assert_eq!(offsets, [0.0, 0.0, 1.0, 2.0, 2.0]);
        assert!(offsets.windows(2).all(|w| w[1] >= w[0]));
        // The eased position never goes back while the targets rise.
        let mut scroll = Scroll::default();
        let mut last = 0.0;
        for (i, target) in offsets.iter().enumerate() {
            let at = i as f64 * 0.1;
            scroll.aim(*target, at, true);
            for step in 0..10 {
                let now = at + f64::from(step) * 0.01;
                let value = scroll.at(now);
                assert!(value >= last - 1e-6, "went back from {last} to {value}");
                last = value;
            }
        }
        // During a take a lower target (a relayout) does not pull it back.
        scroll.aim(0.0, 1.0, true);
        assert!((scroll.at(2.0) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn first_rows_hold_still() {
        assert!(scroll_offset(0, 5, 3).abs() < f32::EPSILON);
        assert!(scroll_offset(1, 5, 3).abs() < f32::EPSILON);
        // Three rows or fewer never scroll.
        for row in 0..3 {
            assert!(scroll_offset(row, 3, 3).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn last_rows_hold_still() {
        assert!((scroll_offset(3, 5, 3) - 2.0).abs() < f32::EPSILON);
        assert!((scroll_offset(4, 5, 3) - 2.0).abs() < f32::EPSILON);
        assert!((scroll_offset(9, 5, 3) - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn retake_resets_scroll() {
        let mut scroll = Scroll::default();
        scroll.aim(2.0, 10.0, true);
        assert!((scroll.at(10.0 + SCROLL_S) - 2.0).abs() < 1e-6);
        assert!(scroll.at(10.0 + SCROLL_S / 2.0) < 2.0, "it eases");
        scroll.reset();
        assert!(
            scroll.at(11.0).abs() < f32::EPSILON,
            "back to the top at once"
        );
    }

    #[test]
    fn byte_index_counts_characters() {
        assert_eq!(byte_index("héllo", 2), 3);
        assert_eq!(byte_index("abc", 10), 3);
    }
}
