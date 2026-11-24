//! The takes of the current line with their effective verdicts and the
//! reason for each ("checking…" while a check runs): a hollow star for the
//! proposed take once its check passed or asks for listening, a filled one
//! for the chosen take.

use gpui_kit::{
    AnyElement, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _, px, rgb,
};
use takewise_core::{LineState, TakeState, Verdict};

use super::colors;

/// What the list says about a take, and in which color.
pub fn label(take: &TakeState, checking: bool) -> (String, u32) {
    if take.missing {
        return ("file missing".to_owned(), colors::RECORD);
    }
    if checking {
        return ("checking…".to_owned(), colors::DIM);
    }
    let stale = if take.stale_check { " (stale)" } else { "" };
    match take.effective {
        None => ("unchecked".to_owned(), colors::DIM),
        Some(Verdict::Keep) => (format!("keep{stale}"), colors::GOOD),
        Some(Verdict::Listen) => (format!("listen{stale}"), colors::WARN),
        Some(Verdict::Retake) => (format!("retake{stale}"), colors::RECORD),
    }
}

/// The star beside a take: filled for the chosen take, hollow for the
/// proposed one only while it is acceptable (`keep` or `listen`, not being
/// checked). A proposed `retake` or unchecked take stays unmarked here; the
/// ranking, and `status --json`'s `proposed`, still name it.
pub fn star(line: &LineState, take: &TakeState, checking: bool) -> &'static str {
    let chosen = line.chosen.as_ref().map(|(n, _)| *n);
    if chosen == Some(take.take) {
        "★"
    } else if line.proposed == Some(take.take)
        && !checking
        && matches!(take.effective, Some(Verdict::Keep | Verdict::Listen))
    {
        "☆"
    } else {
        ""
    }
}

/// `checking`: the line's takes whose check is running or queued;
/// `clipped`: those whose samples reached the clipping threshold.
pub fn takes(
    line: Option<&LineState>,
    selected: Option<u32>,
    checking: &[u32],
    clipped: &[u32],
) -> AnyElement {
    let Some(line) = line else {
        return div().into_any_element();
    };
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
            let checking = checking.contains(&take.take);
            let (text, color) = label(take, checking);
            let reason = take.reason.clone().filter(|_| !checking && !take.missing);
            let star = star(line, take, checking);
            div()
                .id(("take", take.take as usize))
                .px(px(14.))
                .py(px(5.))
                .text_size(px(14.))
                .when(selected == Some(take.take), |el| {
                    el.bg(rgb(colors::SELECTED))
                })
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_between()
                        .child(
                            div()
                                .flex()
                                .gap(px(10.))
                                .child(format!("{}", take.take))
                                .child(div().text_color(rgb(color)).child(text))
                                .when(clipped.contains(&take.take), |el| {
                                    el.child(div().text_color(rgb(colors::RECORD)).child("clipped"))
                                }),
                        )
                        .child(div().text_color(rgb(colors::ACCENT)).child(star)),
                )
                .when_some(reason, |el, reason| {
                    el.child(
                        div()
                            .pl(px(22.))
                            .text_size(px(12.))
                            .text_color(rgb(colors::DIM))
                            .child(reason),
                    )
                })
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(n: u32, effective: Option<Verdict>) -> TakeState {
        TakeState {
            take: n,
            file: format!("takes/a/{n:03}.wav"),
            line_offset_s: 0.0,
            effective,
            verdict_source: effective.map(|_| "check".to_owned()),
            reason: None,
            stale_check: false,
            speech: None,
            speech_onset: None,
            missing: false,
            recorded_against_old_text: false,
            clipped: None,
        }
    }

    fn line(takes: Vec<TakeState>, proposed: u32, chosen: Option<u32>) -> LineState {
        LineState {
            id: "a".to_owned(),
            text: "A line.".to_owned(),
            start: 1.0,
            end: 4.0,
            passage: "a".to_owned(),
            takes,
            chosen: chosen.map(|n| (n, "narrator".to_owned())),
            proposed: Some(proposed),
        }
    }

    #[test]
    fn the_hollow_star_marks_only_an_acceptable_proposed_take() {
        for (verdict, star_shown) in [
            (Some(Verdict::Keep), true),
            (Some(Verdict::Listen), true),
            (Some(Verdict::Retake), false),
            (None, false),
        ] {
            let l = line(vec![take(1, verdict)], 1, None);
            let expected = if star_shown { "☆" } else { "" };
            assert_eq!(star(&l, &l.takes[0], false), expected, "{verdict:?}");
            assert_eq!(star(&l, &l.takes[0], true), "", "checking {verdict:?}");
        }
    }

    #[test]
    fn a_chosen_take_keeps_its_filled_star() {
        let l = line(
            vec![take(1, Some(Verdict::Retake)), take(2, Some(Verdict::Keep))],
            2,
            Some(1),
        );
        assert_eq!(star(&l, &l.takes[0], false), "★");
        assert_eq!(star(&l, &l.takes[0], true), "★");
        assert_eq!(star(&l, &l.takes[1], false), "☆");
    }
}
