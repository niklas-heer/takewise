//! The setup check (C): the devices, the narrator's level and voice, the
//! room's noise, and whether the latency calibration fits (see
//! `docs/design.md`, "Setup check"). It is offered, never forced: when a
//! project has no `[setup]` or the devices differ from it, the window says
//! so and C opens it; it never opens during a take. Its captures are clips
//! of the meter's capture, never takes. The latency step runs the loopback
//! calibration in the window (see `latency.rs`) when the project has none
//! for the devices in use.

use gpui_kit::{
    AnyElement, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px, rgb,
};
use takewise_core::setup::{RoomRating, RoomReport, VoiceReport, room_report, voice_report};
use takewise_core::{self as tw, RECORD_RATE, SetupInfo};
use takewise_media::{OutputSource, Transport};

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
    /// Why the room tone or `[setup]` could not be saved: the step stays
    /// until a new room recording saves.
    pub save_error: Option<String>,
    /// What went wrong last, in words.
    pub error: Option<String>,
    /// The latency step's outcome: what was stored, or why nothing was.
    pub latency: Option<Result<String, String>>,
}

impl Setup {
    fn new() -> Self {
        Self {
            step: Step::Devices,
            listening_until: None,
            voice: None,
            room: None,
            save_error: None,
            error: None,
            latency: None,
        }
    }
}

/// Why the latency calibration should run on `route`, if it should: the
/// project has none, or it was measured on other devices or another buffer
/// size (its offset is then not applied).
pub fn calibration_needed(config: &tw::ProjectConfig, route: &tw::Route) -> Option<String> {
    if config.calibration.is_none() {
        return Some("the latency is not calibrated for these devices".to_owned());
    }
    if super::recorder::calibration_matches(config, route) {
        return None;
    }
    Some(match config.input.latency_offset_ms {
        0 => "the latency calibration does not match these devices".to_owned(),
        offset => format!(
            "the latency calibration does not match these devices, so takes are placed without its {offset} ms offset"
        ),
    })
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
        self.cancel_calibration();
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
        if setup.step == Step::Latency {
            self.start_calibration();
            return;
        }
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
    /// and the latency step is shown only when the calibration is missing
    /// or does not match the devices in use.
    pub(super) fn setup_next(&mut self) {
        let stale = calibration_needed(&self.project.config, &self.current_route()).is_some();
        let calibrating = self.calibrating.is_some();
        let Some(setup) = &mut self.setup else {
            return;
        };
        if setup.listening_until.is_some() || calibrating {
            return;
        }
        setup.error = None;
        match setup.step {
            Step::Devices => setup.step = Step::Voice,
            Step::Voice if setup.voice.as_ref().is_some_and(|v| !heard_voice(v)) => {
                setup.error = Some(silent_channel(self.project.config.input.channel));
            }
            Step::Voice if setup.voice.is_some() => setup.step = Step::Room,
            Step::Voice => {
                setup.error = Some("Read the sentence first: press Space.".to_owned());
            }
            Step::Room if setup.save_error.is_some() => {
                setup.error = Some(
                    "The setup check is not saved; fix the problem above, then press Space to record the room again."
                        .to_owned(),
                );
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
        let calibrated = self
            .setup
            .take()
            .and_then(|s| s.latency)
            .is_some_and(|l| l.is_ok());
        let needed = calibration_needed(&self.project.config, &self.current_route());
        self.message = Some(match (calibrated, needed) {
            (true, _) => "Setup check saved; the latency is calibrated.".to_owned(),
            (false, Some(_)) => {
                "Setup check saved; the latency is not calibrated for these devices yet (C to run it)."
                    .to_owned()
            }
            (false, None) => "Setup check saved.".to_owned(),
        });
    }

    /// Once per frame, after the recorder's tick: analyzes a finished
    /// capture.
    pub(super) fn tick_setup(&mut self, now: f64) {
        self.tick_calibration();
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
                let saved = self.save_setup(&samples, &room);
                if let Some(setup) = &mut self.setup {
                    setup.save_error = saved.err();
                }
            }
            Step::Devices | Step::Latency => {}
        }
    }

    /// Keeps the room tone, then writes `[setup]`; `[setup]` is not
    /// written without its room tone. The error says what was not saved.
    fn save_setup(&mut self, samples: &[i32], room: &RoomReport) -> Result<(), String> {
        let route = self.current_route();
        let Some(input) = route.input.clone() else {
            return Err("Not saved: no input device is chosen.".to_owned());
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
        if let Err(err) =
            tw::room::save_room_tone(&self.dir, samples, RECORD_RATE, now, &room_input)
        {
            return Err(format!("Could not keep the room tone: {err}"));
        }
        let info = SetupInfo {
            // The local date, as the narrator's day is meant.
            date: jiff::ZonedDateTime::now().date().to_string(),
            input,
            channel: route.channel,
            output,
            room_floor_dbfs: (!room.gated).then_some(room.floor_dbfs),
            speech_to_noise_db: room.snr_db,
        };
        let written = tw::set_setup(&self.dir, &info)
            .map_err(|err| format!("Could not save the setup check: {err}"));
        self.reload();
        self.check_inputs();
        written
    }

    /// Why the setup check should run, if it should: none ran, the devices
    /// changed since, or the latency calibration does not match the devices
    /// in use. Never during a take.
    pub(super) fn update_setup_offer(&mut self) {
        if self.recorder.is_recording() {
            return;
        }
        let config = &self.project.config;
        let route = self.current_route();
        let mut reasons: Vec<String> = tw::setup_warning(config, &route).into_iter().collect();
        // A full setup check records the room anyway; otherwise the room
        // changes from day to day, so today's room tone is asked for.
        if reasons.is_empty() {
            let tones = tw::room::room_tones(&self.dir).unwrap_or_default();
            let now = jiff::ZonedDateTime::now();
            if !room_tone_today(&tones, &route, now.date(), now.time_zone()) {
                reasons.push("no room tone recorded today with these devices".to_owned());
            }
        }
        reasons.extend(calibration_needed(config, &route));
        self.setup_offer = (!reasons.is_empty()).then(|| reasons.join("; "));
    }

    /// A warning about the output for recording: speakers leak into the
    /// take, Bluetooth adds delay. The Mac's headphone jack is fine; when the
    /// built-in output does not say which it plays through, the warning is
    /// conditional.
    pub(super) fn output_warning(&self) -> Option<&'static str> {
        let name = self.current_route().output?;
        let output = self.media.outputs().into_iter().find(|o| o.name == name)?;
        match (output.transport, output.output_source) {
            (Transport::Bluetooth, _) => Some(
                "Bluetooth headphones delay the film's sound and are not supported for recording; use wired headphones.",
            ),
            (_, OutputSource::Speakers) => {
                Some("Film sound from speakers will leak into your takes; use wired headphones.")
            }
            (Transport::BuiltIn, OutputSource::Unknown) => Some(
                "If this is the Mac's speakers, film sound will leak into your takes; use wired headphones.",
            ),
            (Transport::BuiltIn | Transport::Usb | Transport::Other, _) => None,
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
                    let channel = self.project.config.input.channel;
                    body.extend(
                        voice_lines(voice, channel)
                            .into_iter()
                            .map(|(t, c)| line(t, c)),
                    );
                }
            }
            Step::Room => {
                body.push(line(
                    "Press Space and stay quiet for five seconds.".to_owned(),
                    colors::DIM,
                ));
                if let Some(room) = &setup.room {
                    let saved = setup.save_error.is_none();
                    body.extend(room_lines(room, saved).into_iter().map(|(t, c)| line(t, c)));
                }
                if let Some(error) = &setup.save_error {
                    body.push(line(error.clone(), colors::RECORD));
                }
            }
            Step::Latency => body.extend(self.latency_body(setup)),
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
            Step::Voice => (
                "2 · Voice",
                "Space listen · Enter next · A audio panel · Esc close",
            ),
            Step::Room => ("3 · Room", "Space listen · Enter next · Esc close"),
            Step::Latency => ("4 · Latency", "Space calibrate · Enter done · Esc close"),
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

/// Whether one of `tones` was recorded on `today` (in `tz`, the local
/// time zone) with `route`'s input, channel and output.
pub fn room_tone_today(
    tones: &[tw::room::RoomTone],
    route: &tw::Route,
    today: jiff::civil::Date,
    tz: &jiff::tz::TimeZone,
) -> bool {
    tones.iter().any(|tone| {
        tz.to_datetime(tone.recorded_at).date() == today
            && route.input.as_deref() == Some(tone.input.as_str())
            && route.channel == tone.channel
            && route.output.as_deref() == Some(tone.output.as_str())
    })
}

/// A line of text in `color`.
pub(super) fn line(text: String, color: u32) -> AnyElement {
    div().text_color(rgb(color)).child(text).into_any_element()
}

/// Whether the voice step heard a voice at all: nothing louder than the
/// voiced threshold means the channel carries no voice.
pub fn heard_voice(voice: &VoiceReport) -> bool {
    voice.speech_rms_dbfs > tw::speech::SILENCE_DBFS
}

/// What the voice step says when it heard nothing on `channel`.
fn silent_channel(channel: u16) -> String {
    format!(
        "No voice heard on channel {channel} — check the channel in the audio panel (A), then press Space to read again."
    )
}

/// The voice step's report in words, with a color each. A silent channel
/// gets no gain suggestion: it says to check the channel instead.
fn voice_lines(voice: &VoiceReport, channel: u16) -> Vec<(String, u32)> {
    if !heard_voice(voice) {
        return vec![(silent_channel(channel), colors::WARN)];
    }
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

/// The room step's report in words, with a color each; `saved` says
/// whether the room tone and `[setup]` were saved.
fn room_lines(room: &RoomReport, saved: bool) -> Vec<(String, u32)> {
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
    if saved {
        lines.push((
            "Saved as room tone. Press Enter to continue.".to_owned(),
            colors::DIM,
        ));
    }
    lines
}
