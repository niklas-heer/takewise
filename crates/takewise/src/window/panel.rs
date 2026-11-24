//! The audio panel (A): input, channel and output pickers, a live meter per
//! input channel, the monitor switch with the reason it is off, the
//! direct-monitoring hint, and device problems in plain words with a
//! "Switch to 48 kHz" button. The levels are always on screen, under the
//! takes (see `levels`).
//!
//! Device and route changes apply between takes only; during a take the
//! pickers are disabled.

use gpui_kit::base::{Select, Switch};
use gpui_kit::{
    AnyElement, AppContext as _, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px, rgb,
};
use takewise_core::{self as tw, AvailableInput, RECORD_RATE, Route};

use super::{RecordingWindow, colors, meter};

/// How long to wait before reopening a channel meter that failed.
const METER_RETRY_S: f64 = 2.0;

/// The hint shown next to the monitor switch, always: CoreAudio cannot tell
/// whether an interface monitors in hardware.
pub const DIRECT_MONITORING_HINT: &str = "If your interface has direct monitoring, use it instead and leave this off, or you will hear yourself twice.";

/// Why the configured input cannot record, in plain words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputIssue {
    NotChosen,
    Missing(String),
    WrongRate { name: String, rate: u32 },
}

impl InputIssue {
    pub fn message(&self) -> String {
        match self {
            Self::NotChosen => "Pick the input your microphone is connected to.".to_owned(),
            Self::Missing(name) => {
                format!("{name} is not connected. Connect it or pick another input.")
            }
            Self::WrongRate { name, rate } => format!(
                "{name} runs at {} kHz; Takewise records at 48 kHz.",
                f64::from(*rate) / 1000.0
            ),
        }
    }
}

/// What keeps the configured input from recording, if anything.
pub fn input_issue(configured: Option<&str>, inputs: &[AvailableInput]) -> Option<InputIssue> {
    let Some(name) = configured else {
        return Some(InputIssue::NotChosen);
    };
    match inputs.iter().find(|input| input.name == name) {
        None => Some(InputIssue::Missing(name.to_owned())),
        Some(input) if input.sample_rate != RECORD_RATE => Some(InputIssue::WrongRate {
            name: name.to_owned(),
            rate: input.sample_rate,
        }),
        Some(_) => None,
    }
}

/// What the window says about the latency calibration on `route`, if
/// anything: that takes are placed without the offset because the
/// calibration does not match these devices, or why it is stale.
pub fn calibration_note(config: &tw::ProjectConfig, route: &Route) -> Option<String> {
    let offset = config.input.latency_offset_ms;
    let stale = tw::calibration_stale(config, route);
    if offset != 0 && super::recorder::effective_latency_offset(config, route) == 0 {
        let why = stale.unwrap_or_else(|| {
            "the input's buffer size or the output in use is not known".to_owned()
        });
        return Some(format!(
            "calibration does not match these devices, so takes are placed without its {offset} ms offset ({why})"
        ));
    }
    stale
}

/// Which picker shows its options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Picker {
    Input,
    Output,
}

/// The panel's state in the window.
pub struct Panel {
    pub open: bool,
    pub picker: Option<Picker>,
    /// The input issue last seen; a new one opens the panel by itself.
    pub issue: Option<InputIssue>,
    /// The device whose channels are metered.
    metering: Option<String>,
    meter_retry_at: f64,
    /// Why the latency calibration is stale, if it is.
    pub calibration: Option<String>,
    /// That the latency in use was copied from an earlier measurement, if
    /// it was, and that a Bluetooth output may need measuring again (see
    /// `remembered.rs`).
    pub remembered: Option<String>,
}

impl Panel {
    pub fn new() -> Self {
        Self {
            open: false,
            picker: None,
            issue: None,
            metering: None,
            meter_retry_at: 0.0,
            calibration: None,
            remembered: None,
        }
    }
}

/// A tooltip's text.
pub(super) struct Tip(pub SharedString);

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.))
            .py(px(4.))
            .rounded(px(4.))
            .bg(rgb(colors::SELECTED))
            .text_size(px(12.))
            .text_color(rgb(colors::TEXT))
            .child(self.0.clone())
    }
}

const BETWEEN_TAKES: &str = "Device changes apply between takes.";

impl RecordingWindow {
    /// Whether the pickers take changes now: not during a take.
    pub(super) fn pickers_enabled(&self) -> bool {
        !self.recorder.is_recording()
    }

    /// Saves a route change to `project.toml` and as the default for new
    /// projects. Refused during a take. Returns `None` when nothing was
    /// saved, else what to add to the message: a latency calibration in
    /// progress is cancelled, since its result would mix two routes.
    fn change_route(
        &mut self,
        input: Option<(&str, u16)>,
        output: Option<&str>,
    ) -> Option<&'static str> {
        if !self.pickers_enabled() {
            self.message = Some(format!("{BETWEEN_TAKES} Stop the take first."));
            return None;
        }
        if let Err(err) = tw::set_route(&self.dir, input, output) {
            self.message = Some(format!("Could not save the devices: {err}"));
            return None;
        }
        self.reload();
        let cancelled = self.cancel_calibration_if_rerouted();
        if let Some(path) = &self.defaults_path {
            // The defaults are a convenience; failing to save them is quiet.
            #[cfg(test)]
            super::start::isolated::assert_temporary(path);
            let _ = tw::route::save_defaults_to(path, &Route::of(&self.project.config));
        }
        self.panel.picker = None;
        self.check_inputs();
        Some(if cancelled {
            " Latency calibration cancelled: the devices changed."
        } else {
            ""
        })
    }

    /// Records from the input device `name` (its first channel when the
    /// device changes).
    pub(super) fn choose_input(&mut self, name: &str) {
        let config = &self.project.config.input;
        let channel = if config.device.as_deref() == Some(name) {
            config.channel
        } else {
            1
        };
        if let Some(note) = self.change_route(Some((name, channel)), None) {
            self.message = Some(format!("Recording from {name}, channel {channel}.{note}"));
        }
    }

    /// Records from `channel` (1-based) of the current input.
    pub(super) fn choose_channel(&mut self, channel: u16) {
        let Some(device) = self.project.config.input.device.clone() else {
            return;
        };
        if let Some(note) = self.change_route(Some((&device, channel)), None) {
            self.message = Some(format!("Recording from {device}, channel {channel}.{note}"));
        }
    }

    /// Plays the film and the takes (and later the monitor) through `name`.
    pub(super) fn choose_output(&mut self, name: &str) {
        let Some(note) = self.change_route(None, Some(name)) else {
            return;
        };
        let said = match self.media.set_output(Some(name)) {
            Ok(()) => {
                self.film_output = Some(name.to_owned());
                self.check_inputs();
                format!("The film and your takes play through {name}.")
            }
            Err(err) => {
                // A playback goes on where it was only if nothing changed;
                // the narrator asked to hear it elsewhere, so it stops.
                let stopped = if self.recorder.is_playing() {
                    self.recorder.stop_playback(&mut *self.media);
                    " Playback stopped."
                } else {
                    ""
                };
                format!("Saved {name} as the output, but cannot play there: {err}.{stopped}")
            }
        };
        self.message = Some(format!("{said}{note}"));
    }

    /// Plays the film and the takes through the configured output, if there
    /// is one; says so when it is missing.
    pub(super) fn apply_output(&mut self) {
        let Some(name) = self.project.config.output.device.clone() else {
            return;
        };
        if let Err(err) = self.media.set_output(Some(&name)) {
            self.film_output = None;
            let fallback = self
                .media
                .default_output()
                .unwrap_or_else(|| "the system output".to_owned());
            self.message = Some(format!(
                "{name} is not connected ({err}); the film and your takes play through {fallback}."
            ));
        } else {
            self.film_output = Some(name);
        }
        self.check_inputs();
    }

    /// Switches the input device to 48 kHz; recording stays off when it
    /// cannot, with CoreAudio's reason in words.
    pub(super) fn switch_to_48k(&mut self) {
        if !self.pickers_enabled() {
            return;
        }
        let Some(device) = self.project.config.input.device.clone() else {
            return;
        };
        self.message = Some(match self.media.set_nominal_rate(&device, RECORD_RATE) {
            Ok(()) => format!("{device} now runs at 48 kHz."),
            Err(err) => format!("{err}. Recording stays off until the input runs at 48 kHz."),
        });
        self.check_inputs();
    }

    /// H: monitoring is toggled between takes only; until it is available,
    /// says why it is off.
    pub(super) fn toggle_monitor(&mut self) {
        if self.recorder.is_recording() {
            return;
        }
        if let Some(reason) = self.media.monitor_unavailable() {
            self.message = Some(format!("Monitoring is {reason}. {DIRECT_MONITORING_HINT}"));
        }
    }

    /// Once per frame: keeps the channel meter on the current input while
    /// the panel is open and no take records.
    pub(super) fn tick_panel(&mut self, now: f64) {
        let input = self.project.config.input.device.clone();
        let want = input
            .filter(|_| self.panel.open && !self.recorder.is_recording())
            .filter(|device| self.inputs.iter().any(|i| &i.name == device));
        if want == self.panel.metering {
            return;
        }
        if self.panel.metering.take().is_some() {
            self.media.stop_channel_meter();
        }
        if let Some(device) = want.filter(|_| now >= self.panel.meter_retry_at) {
            match self.media.start_channel_meter(&device) {
                Ok(()) => self.panel.metering = Some(device),
                Err(_) => self.panel.meter_retry_at = now + METER_RETRY_S,
            }
        }
    }

    /// Notes the input issue; a new one opens the panel by itself.
    pub(super) fn note_issue(&mut self, issue: Option<InputIssue>) {
        if issue.is_some() && issue != self.panel.issue {
            self.panel.open = true;
        }
        self.panel.issue = issue;
    }

    /// The route as it plays now, for the calibration check: the output the
    /// film really plays through (the system default when none was set, or
    /// when the chosen one could not be used or is gone); the buffer is the
    /// input's current one.
    pub(super) fn current_route(&self) -> Route {
        let mut route = Route::of(&self.project.config);
        let connected = self.media.outputs();
        route.output = self
            .film_output
            .clone()
            .filter(|name| connected.iter().any(|o| &o.name == name))
            .or_else(|| self.media.default_output());
        route.buffer_frames = route
            .input
            .as_deref()
            .and_then(|device| self.media.buffer_frames(device));
        route
    }

    pub(super) fn render_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        let enabled = self.pickers_enabled();
        let config = &self.project.config;
        let input = config.input.device.clone();
        let output = config.output.device.clone();
        let default_output = self.media.default_output();
        let levels = self.media.channel_levels();

        let inputs: Vec<String> = self.inputs.iter().map(|i| i.name.clone()).collect();
        let outputs: Vec<String> = self.media.outputs().into_iter().map(|d| d.name).collect();
        let input_label = input
            .clone()
            .unwrap_or_else(|| "Choose an input".to_owned());
        let output_label = output.clone().unwrap_or_else(|| {
            default_output.as_ref().map_or_else(
                || "System output".to_owned(),
                |d| format!("System output ({d})"),
            )
        });

        div()
            .id("audio-panel")
            .w(px(320.))
            .h_full()
            .flex()
            .flex_col()
            .gap(px(12.))
            .px(px(16.))
            .py(px(14.))
            .bg(rgb(colors::PANEL))
            .border_r_1()
            .border_color(rgb(colors::SELECTED))
            .overflow_y_scroll()
            .text_size(px(13.))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .child(div().text_size(px(15.)).child("Audio"))
                    .child(div().text_color(rgb(colors::DIM)).child("A closes")),
            )
            .child(self.picker(
                Picker::Input,
                "Input",
                &input_label,
                inputs,
                input.as_deref(),
                enabled,
                cx,
            ))
            .when_some(self.panel.issue.clone(), |el, issue| {
                let rate = matches!(issue, InputIssue::WrongRate { .. });
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .child(div().text_color(rgb(colors::WARN)).child(issue.message()))
                        .when(rate, |el| {
                            el.child(button("switch-48k", "Switch to 48 kHz", enabled).on_click(
                                cx.listener(|view, _, _, cx| {
                                    view.switch_to_48k();
                                    cx.notify();
                                }),
                            ))
                        }),
                )
            })
            .child(self.channels(&levels, enabled, cx))
            .child(self.picker(
                Picker::Output,
                "Output",
                &output_label,
                outputs,
                output.as_deref(),
                enabled,
                cx,
            ))
            .child(self.monitor_switch())
            .children(self.latency_notes())
            .into_any_element()
    }

    /// What the panel says about the latency: why its calibration is
    /// stale, and that it came from an earlier measurement.
    fn latency_notes(&self) -> Vec<AnyElement> {
        let stale = self.panel.calibration.as_ref().map(|stale| {
            div()
                .text_color(rgb(colors::WARN))
                .child(format!("Latency: {stale}."))
                .into_any_element()
        });
        let remembered = self.panel.remembered.clone().map(|note| {
            div()
                .text_color(rgb(colors::DIM))
                .child(note)
                .into_any_element()
        });
        stale.into_iter().chain(remembered).collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn picker(
        &self,
        which: Picker,
        label: &'static str,
        current: &str,
        options: Vec<String>,
        selected: Option<&str>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = enabled && self.panel.picker == Some(which);
        let id = match which {
            Picker::Input => "input",
            Picker::Output => "output",
        };
        let trigger = div()
            .id((id, 0_usize))
            .px(px(10.))
            .py(px(6.))
            .rounded(px(6.))
            .bg(rgb(colors::SELECTED))
            .text_color(rgb(if enabled { colors::TEXT } else { colors::FAINT }))
            .flex()
            .justify_between()
            .child(current.to_owned())
            .child(if open { "▴" } else { "▾" })
            .when(enabled, |el| {
                el.cursor_pointer()
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.panel.picker = if view.panel.picker == Some(which) {
                            None
                        } else {
                            Some(which)
                        };
                        cx.notify();
                    }))
            })
            .when(!enabled, |el| {
                el.tooltip(|_, cx| cx.new(|_| Tip(BETWEEN_TAKES.into())).into())
            });
        let list = div()
            .flex()
            .flex_col()
            .children(options.into_iter().enumerate().map(|(i, name)| {
                let chosen = selected == Some(name.as_str());
                let pick = name.clone();
                div()
                    .id((id, i + 1))
                    .px(px(10.))
                    .py(px(5.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .when(chosen, |el| el.text_color(rgb(colors::ACCENT)))
                    .hover(|style| style.bg(rgb(colors::SELECTED)))
                    .child(name)
                    .on_click(cx.listener(move |view, _, _, cx| {
                        match which {
                            Picker::Input => view.choose_input(&pick),
                            Picker::Output => view.choose_output(&pick),
                        }
                        cx.notify();
                    }))
            }));
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(div().text_color(rgb(colors::DIM)).child(label))
            .child(
                Select::new(id)
                    .open(open)
                    .disabled(!enabled)
                    .accessibility_label(label)
                    .accessibility_value(current.to_owned())
                    .child(trigger)
                    .when(open, |el| el.child(list)),
            )
            .into_any_element()
    }

    /// One meter per channel of the input; the chosen channel is marked and
    /// a click picks another.
    fn channels(&self, levels: &[f32], enabled: bool, cx: &mut Context<Self>) -> AnyElement {
        let chosen = self.project.config.input.channel;
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .text_color(rgb(colors::DIM))
                    .child("Channel (speak: the one that moves carries your mic)"),
            )
            .when(levels.is_empty(), |el| {
                el.child(
                    div()
                        .text_color(rgb(colors::FAINT))
                        .child("no channel levels (input not connected)"),
                )
            })
            .children(levels.iter().enumerate().map(|(i, level)| {
                #[allow(clippy::cast_possible_truncation)]
                let channel = (i + 1) as u16;
                let selected = channel == chosen;
                div()
                    .id(("channel", i))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(6.))
                    .py(px(3.))
                    .rounded(px(4.))
                    .when(selected, |el| el.bg(rgb(colors::SELECTED)))
                    .child(div().w(px(16.)).child(channel.to_string()))
                    .child(meter::bar(Some(f64::from(*level)), None, 200., 6.))
                    .when(enabled && !selected, |el| {
                        el.cursor_pointer()
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.choose_channel(channel);
                                cx.notify();
                            }))
                    })
                    .when(!enabled, |el| {
                        el.tooltip(|_, cx| cx.new(|_| Tip(BETWEEN_TAKES.into())).into())
                    })
            }))
            .into_any_element()
    }

    /// The monitor switch: off and disabled with the reason while
    /// monitoring is unavailable, and the direct-monitoring hint.
    fn monitor_switch(&self) -> AnyElement {
        let reason = self.media.monitor_unavailable();
        let disabled = reason.is_some();
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        Switch::new("monitor")
                            .checked(false)
                            .disabled(disabled)
                            .accessibility_label("Monitor my voice (H)")
                            .w(px(30.))
                            .h(px(16.))
                            .rounded_full()
                            .bg(rgb(colors::SELECTED))
                            .when(disabled, |el| el.opacity(0.4))
                            .child(
                                div()
                                    .size(px(12.))
                                    .m(px(2.))
                                    .rounded_full()
                                    .bg(rgb(colors::DIM)),
                            ),
                    )
                    .child("Hear my voice (H)"),
            )
            .when_some(reason, |el, reason| {
                el.child(
                    div()
                        .text_color(rgb(colors::DIM))
                        .child(format!("Off: {reason}.")),
                )
            })
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(colors::DIM))
                    .child(DIRECT_MONITORING_HINT),
            )
            .into_any_element()
    }
}

fn button(
    id: &'static str,
    label: &'static str,
    enabled: bool,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .px(px(10.))
        .py(px(5.))
        .rounded(px(6.))
        .bg(rgb(if enabled {
            colors::ACCENT
        } else {
            colors::SELECTED
        }))
        .text_color(rgb(colors::BACKGROUND))
        .when(enabled, |el| el.cursor_pointer())
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str, rate: u32) -> AvailableInput {
        AvailableInput {
            name: name.to_owned(),
            sample_rate: rate,
        }
    }

    #[test]
    fn input_issues_in_plain_words() {
        let inputs = [input("Duo", 48_000), input("MacBook Mic", 44_100)];
        assert_eq!(input_issue(Some("Duo"), &inputs), None);
        let missing = input_issue(Some("RODECaster Duo"), &inputs).unwrap();
        assert_eq!(
            missing.message(),
            "RODECaster Duo is not connected. Connect it or pick another input."
        );
        let rate = input_issue(Some("MacBook Mic"), &inputs).unwrap();
        assert_eq!(
            rate.message(),
            "MacBook Mic runs at 44.1 kHz; Takewise records at 48 kHz."
        );
        assert_eq!(input_issue(None, &inputs), Some(InputIssue::NotChosen));
    }
}
