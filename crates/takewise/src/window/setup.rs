//! The setup check (C): the devices, the narrator's level and voice, the
//! room's noise, and whether the latency calibration fits (see
//! `docs/design.md`, "Setup check"). It is offered, never forced: when a
//! project has no `[setup]` or the devices differ from it, the window says
//! so and C opens it; it never opens during a take. Its captures are clips
//! of the meter's capture, never takes.

use gpui_kit::{
    AnyElement, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px, rgb,
};
use takewise_core::setup::{RoomRating, RoomReport, VoiceReport, room_report, voice_report};
use takewise_core::{self as tw, RECORD_RATE, SetupInfo};
use takewise_media::Transport;

use super::{RecordingWindow, colors};

/// The sentence the narrator reads in the voice step.
pub const SENTENCE: &str =
    "The quick brown fox jumps over the lazy dog, then naps by the warm fire.";
/// How long the voice and room steps listen.
pub const CAPTURE_S: f64 = 5.0;
/// A capture shorter than this (the input stopped) is not analyzed.
const MIN_CAPTURE_S: f64 = 4.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Devices,
    Voice,
    Room,
    Latency,
}

/// The setup check while it is open.
#[derive(Debug, Clone, PartialEq)]
pub struct Setup {
    pub step: Step,
    /// While the voice or room step listens: when it stops (host seconds).
    pub listening_until: Option<f64>,
    pub voice: Option<VoiceReport>,
    pub room: Option<RoomReport>,
    /// What went wrong last, in words.
    pub error: Option<String>,
}

impl Setup {
    fn new() -> Self {
        Self {
            step: Step::Devices,
            listening_until: None,
            voice: None,
            room: None,
            error: None,
        }
    }
}

impl RecordingWindow {
    /// C: opens the setup check, or closes it when open. Refused during a
    /// take.
    pub(super) fn toggle_setup(&mut self) {
        if self.setup.is_some() {
            self.close_setup();
            return;
        }
        if self.recorder.is_recording() {
            self.message = Some("Finish the take first, then press C.".to_owned());
            return;
        }
        self.recorder.stop_playback(&mut *self.media);
        self.setup = Some(Setup::new());
    }

    pub(super) fn close_setup(&mut self) {
        if self
            .setup
            .take()
            .is_some_and(|s| s.listening_until.is_some())
        {
            self.recorder.take_clip();
        }
    }

    /// Space: listens for the voice or room step.
    pub(super) fn setup_listen(&mut self) {
        let now = self.media.host_now_s();
        let Some(setup) = &mut self.setup else {
            return;
        };
        if setup.listening_until.is_some() || !matches!(setup.step, Step::Voice | Step::Room) {
            return;
        }
        if let Some(problem) = &self.input_problem {
            setup.error = Some(problem.clone());
            return;
        }
        if self.recorder.start_clip() {
            setup.listening_until = Some(now + CAPTURE_S);
            setup.error = None;
        } else {
            setup.error =
                Some("The input is not running yet; wait a moment and press Space again.".into());
        }
    }

    /// Enter: the next step; the room step needs the voice step's reading,
    /// and the latency step is shown only when the calibration is stale.
    pub(super) fn setup_next(&mut self) {
        let stale = self.panel.calibration.is_some();
        let Some(setup) = &mut self.setup else {
            return;
        };
        if setup.listening_until.is_some() {
            return;
        }
        setup.error = None;
        match setup.step {
            Step::Devices => setup.step = Step::Voice,
            Step::Voice if setup.voice.is_some() => setup.step = Step::Room,
            Step::Voice => {
                setup.error = Some("Read the sentence first: press Space.".to_owned());
            }
            Step::Room if setup.room.is_some() && stale => setup.step = Step::Latency,
            Step::Room if setup.room.is_some() => self.finish_setup(),
            Step::Room => {
                setup.error = Some("Record the room first: press Space and stay quiet.".into());
            }
            Step::Latency => self.finish_setup(),
        }
    }

    fn finish_setup(&mut self) {
        self.setup = None;
        self.message = Some("Setup check saved.".to_owned());
    }

    /// Once per frame, after the recorder's tick: analyzes a finished
    /// capture.
    pub(super) fn tick_setup(&mut self, now: f64) {
        let Some(setup) = &mut self.setup else {
            return;
        };
        let Some(until) = setup.listening_until else {
            return;
        };
        if now < until {
            return;
        }
        setup.listening_until = None;
        let samples = self.recorder.take_clip().unwrap_or_default();
        if (samples.len() as f64) < MIN_CAPTURE_S * f64::from(RECORD_RATE) {
            setup.error = Some("The input stopped while listening; press Space again.".into());
            return;
        }
        match setup.step {
            Step::Voice => {
                setup.voice = Some(voice_report(&samples, RECORD_RATE));
                setup.room = None;
            }
            Step::Room => {
                let speech = setup
                    .voice
                    .as_ref()
                    .map_or(tw::speech::SILENCE_DBFS, |v| v.speech_rms_dbfs);
                let room = room_report(&samples, RECORD_RATE, speech);
                setup.room = Some(room.clone());
                self.save_setup(&samples, &room);
            }
            Step::Devices | Step::Latency => {}
        }
    }

    /// Keeps the room tone and writes `[setup]`.
    fn save_setup(&mut self, samples: &[i32], room: &RoomReport) {
        let route = self.current_route();
        let Some(input) = route.input.clone() else {
            return;
        };
        let output = route.output.clone().unwrap_or_default();
        let now = jiff::Timestamp::now();
        let room_input = tw::room::RoomInput {
            input: &input,
            channel: route.channel,
            output: &output,
            floor_dbfs: room.floor_dbfs,
            gated: room.gated,
        };
        let saved = tw::room::save_room_tone(&self.dir, samples, RECORD_RATE, now, &room_input);
        let info = SetupInfo {
            date: now.strftime("%Y-%m-%d").to_string(),
            input,
            channel: route.channel,
            output,
            room_floor_dbfs: (!room.gated).then_some(room.floor_dbfs),
            speech_to_noise_db: room.snr_db,
        };
        let written = tw::set_setup(&self.dir, &info);
        let error = match (saved, written) {
            (Ok(_), Ok(())) => None,
            (Err(err), _) => Some(format!("Could not keep the room tone: {err}")),
            (_, Err(err)) => Some(format!("Could not save the setup check: {err}")),
        };
        self.reload();
        self.check_inputs();
        if let Some(setup) = &mut self.setup {
            setup.error = error;
        }
    }

    /// Why the setup check should run, if it should: none ran, or the
    /// devices changed since. Never during a take.
    pub(super) fn update_setup_offer(&mut self) {
        if self.recorder.is_recording() {
            return;
        }
        self.setup_offer = tw::setup_warning(&self.project.config, &self.current_route());
    }

    /// A warning about the output for recording: speakers leak into the
    /// take, Bluetooth adds delay.
    pub(super) fn output_warning(&self) -> Option<&'static str> {
        let name = self.current_route().output?;
        let output = self.media.outputs().into_iter().find(|o| o.name == name)?;
        match output.transport {
            Transport::BuiltIn => {
                Some("Film sound from speakers will leak into your takes; use wired headphones.")
            }
            Transport::Bluetooth => Some(
                "Bluetooth headphones delay the film's sound and are not supported for recording; use wired headphones.",
            ),
            Transport::Usb | Transport::Other => None,
        }
    }

    /// The open step's content.
    fn setup_body(&self, setup: &Setup) -> Vec<AnyElement> {
        let mut body = Vec::new();
        match setup.step {
            Step::Devices => {
                let route = self.current_route();
                body.push(line(
                    format!(
                        "Input: {} · channel {}",
                        route.input.as_deref().unwrap_or("none chosen"),
                        route.channel
                    ),
                    colors::TEXT,
                ));
                body.push(line(
                    format!(
                        "Output: {}",
                        route.output.as_deref().unwrap_or("system output")
                    ),
                    colors::TEXT,
                ));
                if let Some(problem) = &self.input_problem {
                    body.push(line(problem.clone(), colors::WARN));
                }
                if let Some(warning) = self.output_warning() {
                    body.push(line(warning.to_owned(), colors::WARN));
                }
            }
            Step::Voice => {
                body.push(line(
                    "Read this sentence as you will narrate:".to_owned(),
                    colors::DIM,
                ));
                body.push(div().text_size(px(18.)).child(SENTENCE).into_any_element());
                if let Some(voice) = &setup.voice {
                    body.extend(voice_lines(voice).into_iter().map(|(t, c)| line(t, c)));
                }
            }
            Step::Room => {
                body.push(line(
                    "Press Space and stay quiet for five seconds.".to_owned(),
                    colors::DIM,
                ));
                if let Some(room) = &setup.room {
                    body.extend(room_lines(room).into_iter().map(|(t, c)| line(t, c)));
                }
            }
            Step::Latency => {
                let stale = self.panel.calibration.clone().unwrap_or_default();
                body.push(line(format!("Latency: {stale}."), colors::WARN));
                body.push(line(
                    format!(
                        "Hold your headphones to the mic and run `takewise calibrate {} --write`.",
                        self.dir.display()
                    ),
                    colors::TEXT,
                ));
            }
        }
        if setup.listening_until.is_some() {
            body.push(line("Listening…".to_owned(), colors::RECORD));
        }
        if let Some(error) = &setup.error {
            body.push(line(error.clone(), colors::WARN));
        }
        body
    }

    /// The setup check's panel while it is open.
    pub(super) fn render_setup(&self) -> Option<AnyElement> {
        let setup = self.setup.as_ref()?;
        let (title, keys) = match setup.step {
            Step::Devices => ("1 · Devices", "Enter next · A audio panel · Esc close"),
            Step::Voice => ("2 · Voice", "Space listen · Enter next · Esc close"),
            Step::Room => ("3 · Room", "Space listen · Enter next · Esc close"),
            Step::Latency => ("4 · Latency", "Enter done · Esc close"),
        };
        Some(
            div()
                .id("setup-check")
                .w(px(380.))
                .h_full()
                .flex()
                .flex_col()
                .gap(px(10.))
                .px(px(16.))
                .py(px(14.))
                .bg(rgb(colors::PANEL))
                .border_r_1()
                .border_color(rgb(colors::SELECTED))
                .overflow_y_scroll()
                .text_size(px(13.))
                .child(
                    div()
                        .text_size(px(15.))
                        .child(format!("Setup check · {title}")),
                )
                .children(self.setup_body(setup))
                .child(div().text_color(rgb(colors::DIM)).child(keys))
                .into_any_element(),
        )
    }
}

/// A line of text in `color`.
fn line(text: String, color: u32) -> AnyElement {
    div().text_color(rgb(color)).child(text).into_any_element()
}

/// The voice step's report in words, with a color each.
fn voice_lines(voice: &VoiceReport) -> Vec<(String, u32)> {
    let mut lines = vec![(
        format!(
            "Peak {:.1} dBFS · speech {:.1} dBFS",
            voice.peak_dbfs, voice.speech_rms_dbfs
        ),
        colors::TEXT,
    )];
    if voice.level_fits() {
        lines.push((
            "The level fits. Press Enter to continue.".to_owned(),
            colors::GOOD,
        ));
    } else {
        let direction = if voice.gain_change_db > 0.0 {
            "up"
        } else {
            "down"
        };
        lines.push((
            format!(
                "Turn the gain on your interface {direction} by about {:.0} dB, then press Space to read again (or Enter to go on).",
                voice.gain_change_db.abs()
            ),
            colors::WARN,
        ));
    }
    lines.extend(voice.hints.iter().map(|h| (h.message.clone(), colors::DIM)));
    lines
}

/// The room step's report in words, with a color each.
fn room_lines(room: &RoomReport) -> Vec<(String, u32)> {
    let mut lines = Vec::new();
    match room.snr_db {
        Some(snr) => {
            let (rating, color) = match room.rating {
                RoomRating::Good => ("good", colors::GOOD),
                RoomRating::Acceptable => ("acceptable", colors::WARN),
                RoomRating::Noisy => ("noisy", colors::RECORD),
            };
            lines.push((
                format!(
                    "Room floor {:.1} dBFS · your voice {snr:.0} dB above it: {rating}",
                    room.floor_dbfs
                ),
                color,
            ));
        }
        None => lines.push(("Silent between words (gated).".to_owned(), colors::TEXT)),
    }
    lines.extend(room.tips.iter().map(|t| (t.clone(), colors::DIM)));
    lines.push((
        "Saved as room tone. Press Enter to continue.".to_owned(),
        colors::DIM,
    ));
    lines
}
