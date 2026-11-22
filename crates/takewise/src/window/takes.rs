//! The takes of the current line with their effective verdicts: a hollow
//! star for the proposed take, a filled one for the chosen take.

use gpui_kit::{
    AnyElement, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px, rgb,
};
use takewise_core::{LineState, TakeState, Verdict};

use super::colors;

fn verdict(take: &TakeState) -> (String, u32) {
    if take.missing {
        return ("file missing".to_owned(), colors::RECORD);
    }
    let stale = if take.stale_check { " (stale)" } else { "" };
    match take.effective {
        None => ("unchecked".to_owned(), colors::DIM),
        Some(Verdict::Keep) => (format!("keep{stale}"), colors::GOOD),
        Some(Verdict::Listen) => (format!("listen{stale}"), colors::WARN),
        Some(Verdict::Retake) => (format!("retake{stale}"), colors::RECORD),
    }
}

pub fn takes(line: Option<&LineState>, selected: Option<u32>) -> AnyElement {
    let Some(line) = line else {
        return div().into_any_element();
    };
    let chosen = line.chosen.as_ref().map(|(n, _)| *n);
    div()
        .id("takes")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .border_t_1()
        .border_color(rgb(colors::SELECTED))
        .py(px(8.))
        .child(
            div()
                .px(px(14.))
                .pb(px(6.))
                .text_size(px(13.))
                .text_color(rgb(colors::DIM))
                .child(format!("takes of {}", line.id)),
        )
        .when(line.takes.is_empty(), |el| {
            el.child(
                div()
                    .px(px(14.))
                    .text_size(px(14.))
                    .text_color(rgb(colors::FAINT))
                    .child("none yet — press Space"),
            )
        })
        .children(line.takes.iter().map(|take| {
            let (text, color) = verdict(take);
            let star = if chosen == Some(take.take) {
                "★"
            } else if line.proposed == Some(take.take) {
                "☆"
            } else {
                ""
            };
            div()
                .id(("take", take.take as usize))
                .flex()
                .flex_row()
                .justify_between()
                .px(px(14.))
                .py(px(5.))
                .text_size(px(14.))
                .when(selected == Some(take.take), |el| {
                    el.bg(rgb(colors::SELECTED))
                })
                .child(
                    div()
                        .flex()
                        .gap(px(10.))
                        .child(format!("{}", take.take))
                        .child(div().text_color(rgb(color)).child(text)),
                )
                .child(div().text_color(rgb(colors::ACCENT)).child(star))
        }))
        .into_any_element()
}
