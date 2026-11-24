//! The line list: number, id, chosen mark and take count, with a thin bar
//! beside the lines that form one passage.

use gpui_kit::{
    AnyElement, InteractiveElement as _, IntoElement, ParentElement as _, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px, rgb,
};
use takewise_core::{LineState, State};

use super::colors;

/// How far the passage bar stops short of its first line's top and its
/// last line's bottom, so two passages in a row read as two.
const BAR_INSET_PX: f32 = 5.0;

/// Where line `idx` lies in a passage of several lines: whether it is the
/// passage's first line and whether its last; `None` for a line that is a
/// passage of its own, which gets no bar.
pub fn passage_edges(lines: &[LineState], idx: usize) -> Option<(bool, bool)> {
    let line = lines.get(idx)?;
    let same = |other: Option<&LineState>| other.is_some_and(|other| other.passage == line.passage);
    let first = !same(idx.checked_sub(1).and_then(|prev| lines.get(prev)));
    let last = !same(lines.get(idx + 1));
    (!(first && last)).then_some((first, last))
}

/// `scroll` follows the list's scroll position; the window scrolls it to
/// keep the current line in view.
pub fn lines(
    state: &State,
    current: usize,
    recording: Option<&str>,
    scroll: &ScrollHandle,
) -> AnyElement {
    div()
        .id("lines")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .track_scroll(scroll)
        .py(px(8.))
        .children(state.lines.iter().enumerate().map(|(idx, line)| {
            let mark = if recording == Some(line.id.as_str()) {
                "●".to_owned()
            } else if line.chosen.is_some() {
                format!("✓ {}", line.takes.len())
            } else if line.takes.is_empty() {
                "–".to_owned()
            } else {
                line.takes.len().to_string()
            };
            let bar = passage_edges(&state.lines, idx).map(|(first, last)| {
                div()
                    .absolute()
                    .left(px(5.))
                    .top(px(if first { BAR_INSET_PX } else { 0. }))
                    .bottom(px(if last { BAR_INSET_PX } else { 0. }))
                    .w(px(2.))
                    .bg(rgb(colors::FAINT))
            });
            div()
                .id(("line", idx))
                .relative()
                .flex()
                .flex_row()
                .justify_between()
                .px(px(14.))
                .py(px(5.))
                .text_size(px(14.))
                .when(idx == current, |el| el.bg(rgb(colors::SELECTED)))
                .children(bar)
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(
                            div()
                                .text_color(rgb(colors::DIM))
                                .child(format!("{}", idx + 1)),
                        )
                        .child(line.id.clone()),
                )
                .child(
                    div()
                        .text_color(rgb(if line.chosen.is_some() {
                            colors::GOOD
                        } else {
                            colors::DIM
                        }))
                        .child(mark),
                )
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, passage: &str) -> LineState {
        LineState {
            id: id.to_owned(),
            text: "A line.".to_owned(),
            start: 0.0,
            end: 1.0,
            passage: passage.to_owned(),
            takes: Vec::new(),
            chosen: None,
            proposed: None,
        }
    }

    #[test]
    fn the_bar_runs_beside_the_lines_of_a_passage_of_several() {
        let lines = [
            line("a", "a"),
            line("b", "a"),
            line("c", "a"),
            line("d", "d"),
            line("e", "e"),
            line("f", "e"),
        ];
        let edges: Vec<_> = (0..lines.len())
            .map(|idx| passage_edges(&lines, idx))
            .collect();
        assert_eq!(
            edges,
            [
                Some((true, false)),
                Some((false, false)),
                Some((false, true)),
                None,
                Some((true, false)),
                Some((false, true)),
            ]
        );
        assert_eq!(passage_edges(&lines, 6), None);
    }
}
