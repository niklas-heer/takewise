//! The line list: number, id, chosen mark and take count.

use gpui_kit::{
    AnyElement, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px, rgb,
};
use takewise_core::State;

use super::colors;

pub fn lines(state: &State, current: usize, recording: Option<&str>) -> AnyElement {
    div()
        .id("lines")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
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
            div()
                .id(("line", idx))
                .flex()
                .flex_row()
                .justify_between()
                .px(px(14.))
                .py(px(5.))
                .text_size(px(14.))
                .when(idx == current, |el| el.bg(rgb(colors::SELECTED)))
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
