//! The prompter: film time, the slot running out, the input level, the
//! current line large with the pace guide filling it, the next line dimmed,
//! and any message or reason recording is blocked.

use gpui_kit::base::input::{Input, InputState};
use gpui_kit::{
    AnyElement, Entity, HighlightStyle, IntoElement, ParentElement as _, Styled as _, StyledText,
    div, prelude::FluentBuilder as _, px, relative, rgb,
};
use takewise_core::{LineState, pace_fill};

use super::colors;

/// The level meter's floor.
const METER_FLOOR_DB: f64 = -60.0;

pub struct Prompter<'a> {
    pub film_s: f64,
    pub line: Option<&'a LineState>,
    pub note: Option<&'a str>,
    pub next: Option<&'a str>,
    pub level_dbfs: Option<f64>,
    pub recording: bool,
    pub blocked: Option<&'a str>,
    pub message: Option<&'a str>,
    pub editor: Option<&'a Entity<InputState>>,
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
        let slot = self.line.map(|line| {
            let span = line.end - line.start;
            let progress = if span > 0.0 {
                (self.film_s - line.start) / span
            } else {
                0.0
            };
            (span, progress)
        });
        let level = self.level_dbfs.unwrap_or(f64::NEG_INFINITY);
        let level_fraction = if level.is_finite() {
            (level - METER_FLOOR_DB) / -METER_FLOOR_DB
        } else {
            0.0
        };
        let level_color = if level > -1.0 {
            colors::RECORD
        } else if level > -6.0 {
            colors::WARN
        } else {
            colors::GOOD
        };
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
            .child(bar(level_fraction, 120., level_color))
            .child("mic")
            .into_any_element()
    }

    pub fn render(self) -> AnyElement {
        let status = self.status();
        let (filled, text) = self.line.map_or((0, String::new()), |line| {
            let chars = pace_fill(&line.text, line.start, line.end, self.film_s);
            (byte_index(&line.text, chars), line.text.clone())
        });

        let current = match self.editor {
            Some(editor) => div()
                .px(px(8.))
                .py(px(6.))
                .rounded(px(6.))
                .border_1()
                .border_color(rgb(colors::ACCENT))
                .text_size(px(30.))
                .child(Input::new(editor))
                .into_any_element(),
            None => div()
                .text_size(px(34.))
                .line_height(px(44.))
                .text_color(rgb(colors::FAINT))
                .child(StyledText::new(text).with_highlights([(
                    0..filled,
                    HighlightStyle {
                        color: Some(rgb(colors::TEXT).into()),
                        ..HighlightStyle::default()
                    },
                )]))
                .into_any_element(),
        };

        div()
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
            .into_any_element()
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
    fn byte_index_counts_characters() {
        assert_eq!(byte_index("héllo", 2), 3);
        assert_eq!(byte_index("abc", 10), 3);
    }
}
