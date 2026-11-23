//! The film, drawn from the player's latest frame with GPUI's `surface()`,
//! letterboxed, with the count-in and the recording mark on top.

use gpui_kit::{
    AnyElement, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, div,
    prelude::FluentBuilder as _, px, rgb, rgba, surface,
};
use takewise_media::Frame;

use super::colors;

/// The plate behind the count-in's digit: its side, and black at 60 %.
const COUNT_IN_PLATE_PX: f32 = 220.0;
const COUNT_IN_PLATE: u32 = 0x0000_0099;

pub fn video(frame: Option<Frame>, count_in: Option<u32>, recording: bool) -> AnyElement {
    let picture = match frame {
        // `surface` letterboxes (object fit "contain").
        Some(frame) => surface(frame).size_full().into_any_element(),
        None => div().size_full().into_any_element(),
    };
    div()
        .debug_selector(|| "film".to_owned())
        .flex_1()
        .min_h(px(0.))
        .relative()
        .bg(rgb(0x0000_0000))
        .child(picture)
        .when_some(count_in, |el, n| {
            el.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        // On a dark plate, so the digit reads over a light
                        // film as well as over a dark one.
                        div()
                            .size(px(COUNT_IN_PLATE_PX))
                            .rounded(px(36.))
                            .bg(rgba(COUNT_IN_PLATE))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(160.))
                            .text_color(rgb(colors::TEXT))
                            .child(n.to_string()),
                    ),
            )
        })
        .when(recording, |el| {
            el.child(
                div()
                    .absolute()
                    .top(px(14.))
                    .left(px(16.))
                    .px(px(10.))
                    .py(px(4.))
                    .rounded(px(6.))
                    .bg(rgb(colors::RECORD))
                    .text_color(rgb(0x00ff_ffff))
                    .text_size(px(14.))
                    .child("● REC"),
            )
        })
        .into_any_element()
}
