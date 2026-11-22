//! The film, drawn from the player's latest frame with GPUI's `surface()`,
//! letterboxed, with the count-in and the recording mark on top.

use gpui_kit::{
    AnyElement, IntoElement, ParentElement as _, Styled as _, div, prelude::FluentBuilder as _, px,
    rgb, surface,
};
use takewise_media::Frame;

use super::colors;

pub fn video(frame: Option<Frame>, count_in: Option<u32>, recording: bool) -> AnyElement {
    let picture = match frame {
        // `surface` letterboxes (object fit "contain").
        Some(frame) => surface(frame).size_full().into_any_element(),
        None => div().size_full().into_any_element(),
    };
    div()
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
                    .text_size(px(160.))
                    .text_color(rgb(colors::TEXT))
                    .child(n.to_string()),
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
