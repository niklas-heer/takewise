//! The levels under the takes, always on screen: how loud the film plays,
//! how loud takes and passages play back (P, Shift+P), and the monitor
//! while monitoring can run. Each is a slider with its value in dB; the
//! film and monitor keys move them too. A change applies at once, also
//! during a take or a playback, and is written to `project.toml` once it
//! rested a moment, between takes: only the levels changed here, so a
//! level an agent changed in the file meanwhile is kept, and heard.

use gpui_kit::base::slider::{SliderEvent, SliderState};
use gpui_kit::base::{Slider, SliderIndicator, SliderThumb, SliderTrack};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    Window, div, px, relative, rgb,
};
use takewise_core::{self as tw, ProjectConfig};

use super::{RecordingWindow, colors, panel::Tip};

/// Film level range in dB (AVPlayer cannot amplify).
pub const FILM_DB: (f64, f64) = (-60.0, 0.0);
/// Voice (playback) level range in dB (AVPlayer cannot amplify).
pub const VOICE_DB: (f64, f64) = (-60.0, 0.0);
/// Monitor level range in dB.
pub const MONITOR_DB: (f64, f64) = (-60.0, 6.0);
/// One key press, and the sliders' step.
pub const STEP_DB: f64 = 1.0;
/// How long a level change waits before it is written to `project.toml`
/// (a dragged slider writes once).
pub const SAVE_DELAY_S: f64 = 0.5;
/// How long a level save that failed waits before it is tried again.
pub const SAVE_RETRY_S: f64 = 2.0;

/// One of the levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// The film's sound: `[record] film_gain_db`.
    Film,
    /// Takes and passages played back: `[record] voice_gain_db`.
    Voice,
    /// The monitor: `[monitor] volume_db`.
    Monitor,
}

impl Level {
    pub const ALL: [Level; 3] = [Level::Film, Level::Voice, Level::Monitor];

    pub fn range(self) -> (f64, f64) {
        match self {
            Self::Film => FILM_DB,
            Self::Voice => VOICE_DB,
            Self::Monitor => MONITOR_DB,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Film => "Film",
            Self::Voice => "Voice",
            Self::Monitor => "Monitor",
        }
    }

    /// `db` as this level can be set: within its range, or `default` when
    /// it is no number at all.
    fn fit(self, db: f64, default: f64) -> f64 {
        let (min, max) = self.range();
        if db.is_finite() {
            db.clamp(min, max)
        } else {
            default
        }
    }

    /// What the level sets, for its tooltip and accessibility label.
    fn about(self) -> &'static str {
        match self {
            Self::Film => "How loud the film plays (− / +)",
            Self::Voice => "How loud your takes play back with P and Shift+P",
            Self::Monitor => "How loud you hear yourself (, / .)",
        }
    }
}

/// Every level, in dB, as set now.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Volumes {
    pub film: f64,
    pub voice: f64,
    pub monitor: f64,
}

impl Volumes {
    /// The levels in `config`, each within its range (a file can say
    /// anything).
    pub fn of(config: &ProjectConfig) -> Self {
        let record = tw::RecordConfig::default();
        let monitor = tw::MonitorConfig::default();
        Self {
            film: Level::Film.fit(config.record.film_gain_db, record.film_gain_db),
            voice: Level::Voice.fit(config.record.voice_gain_db, record.voice_gain_db),
            monitor: Level::Monitor.fit(config.monitor.volume_db, monitor.volume_db),
        }
    }

    pub fn get(self, level: Level) -> f64 {
        match level {
            Level::Film => self.film,
            Level::Voice => self.voice,
            Level::Monitor => self.monitor,
        }
    }

    fn set(&mut self, level: Level, db: f64) {
        match level {
            Level::Film => self.film = db,
            Level::Voice => self.voice = db,
            Level::Monitor => self.monitor = db,
        }
    }

    pub fn write_into(self, config: &mut ProjectConfig) {
        config.record.film_gain_db = self.film;
        config.record.voice_gain_db = self.voice;
        config.monitor.volume_db = self.monitor;
    }
}

/// Level changes that `project.toml` does not have yet.
#[derive(Debug, Default)]
pub struct Unsaved {
    /// The levels changed in the window since they were last written.
    levels: Vec<Level>,
    /// When the last of them changed.
    changed_at: f64,
    /// When a write that failed may be tried again; whether one failed.
    retry_at: f64,
    failed: bool,
}

impl Unsaved {
    fn note(&mut self, level: Level, now: f64) {
        if !self.levels.contains(&level) {
            self.levels.push(level);
        }
        self.changed_at = now;
    }

    pub fn contains(&self, level: Level) -> bool {
        self.levels.contains(&level)
    }
}

/// The sliders' state in the window.
pub struct Levels {
    film: Entity<SliderState>,
    voice: Entity<SliderState>,
    monitor: Entity<SliderState>,
}

impl Levels {
    /// The sliders at `volumes`, and the subscriptions (to keep) that apply
    /// their changes to the window.
    pub fn new(
        volumes: Volumes,
        window: &mut Window,
        cx: &mut Context<RecordingWindow>,
    ) -> (Self, Vec<Subscription>) {
        #[allow(clippy::cast_possible_truncation)]
        let slider = |level: Level, cx: &mut Context<RecordingWindow>| {
            let (min, max) = level.range();
            cx.new(|_| {
                SliderState::new()
                    .min(min as f32)
                    .max(max as f32)
                    .step(STEP_DB as f32)
                    .default_value(volumes.get(level) as f32)
            })
        };
        let levels = Self {
            film: slider(Level::Film, cx),
            voice: slider(Level::Voice, cx),
            monitor: slider(Level::Monitor, cx),
        };
        let subscriptions = [Level::Film, Level::Voice, Level::Monitor]
            .into_iter()
            .map(|level| {
                cx.subscribe_in(
                    levels.slider(level),
                    window,
                    move |view: &mut RecordingWindow, _, event: &SliderEvent, _, cx| {
                        let (SliderEvent::Change(value) | SliderEvent::Release(value)) = event;
                        view.set_level(level, f64::from(value.end()));
                        cx.notify();
                    },
                )
            })
            .collect();
        (levels, subscriptions)
    }

    pub(super) fn slider(&self, level: Level) -> &Entity<SliderState> {
        match level {
            Level::Film => &self.film,
            Level::Voice => &self.voice,
            Level::Monitor => &self.monitor,
        }
    }
}

/// A level as the window shows it: "−12 dB", "0 dB", "+3 dB".
pub fn db_text(db: f64) -> String {
    let rounded = db.round();
    if rounded == 0.0 {
        "0 dB".to_owned()
    } else if rounded < 0.0 {
        format!("−{:.0} dB", -rounded)
    } else {
        format!("+{rounded:.0} dB")
    }
}

impl RecordingWindow {
    /// The levels shown: the monitor's only while monitoring can run.
    pub(super) fn shown_levels(&self) -> Vec<Level> {
        let mut shown = vec![Level::Film, Level::Voice];
        if self.media.monitor_unavailable().is_none() {
            shown.push(Level::Monitor);
        }
        shown
    }

    /// Changes `level` by `delta` dB (a key), clamped to its range, and
    /// moves its slider.
    pub(super) fn step_level(
        &mut self,
        level: Level,
        delta: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (min, max) = level.range();
        let db = (self.volumes.get(level) + delta).clamp(min, max);
        self.set_level(level, db);
        #[allow(clippy::cast_possible_truncation)]
        self.levels
            .slider(level)
            .update(cx, |s, cx| s.set_value(db as f32, window, cx));
        cx.notify();
    }

    /// Sets `level` to `db` (from a key or a slider): the film and a take
    /// playing back change at once, also during a take; `project.toml` is
    /// written shortly after, between takes.
    pub(super) fn set_level(&mut self, level: Level, db: f64) {
        let (min, max) = level.range();
        let db = db.clamp(min, max);
        if (db - self.volumes.get(level)).abs() <= f64::EPSILON {
            return;
        }
        self.volumes.set(level, db);
        match level {
            Level::Film => self.media.film_set_gain_db(db),
            Level::Voice => self.media.take_set_gain_db(db),
            // Applied by the monitor once monitoring can run.
            Level::Monitor => {}
        }
        self.recorder
            .set_playback_levels(self.volumes.film, self.volumes.voice);
        self.volumes.write_into(&mut self.project.config);
        self.unsaved.note(level, self.media.host_now_s());
    }

    /// Writes the levels changed here to `project.toml` once they rested a
    /// moment, never during a take; with `now` at once (the window
    /// closing). A write that fails keeps them, says so once, and is tried
    /// again every [`SAVE_RETRY_S`].
    pub(super) fn save_volumes(&mut self, now: bool) {
        if self.unsaved.levels.is_empty() || self.recorder.is_recording() {
            return;
        }
        let at = self.media.host_now_s();
        let due = at - self.unsaved.changed_at >= SAVE_DELAY_S && at >= self.unsaved.retry_at;
        if !(now || due) {
            return;
        }
        let changed = |level: Level| {
            self.unsaved
                .contains(level)
                .then_some(self.volumes.get(level))
        };
        let volumes = tw::Volumes {
            film_gain_db: changed(Level::Film),
            voice_gain_db: changed(Level::Voice),
            monitor_db: changed(Level::Monitor),
        };
        match tw::set_volumes(&self.dir, volumes) {
            Ok(()) => {
                if self.unsaved.failed {
                    self.message = Some("Saved the levels.".to_owned());
                }
                self.unsaved = Unsaved::default();
            }
            Err(err) => {
                let said = format!("Could not save the levels; trying again: {err}");
                if now {
                    eprintln!("takewise: {said}");
                }
                if !self.unsaved.failed {
                    self.message = Some(said);
                }
                self.unsaved.failed = true;
                self.unsaved.retry_at = at + SAVE_RETRY_S;
            }
        }
    }

    /// After a reload: the levels changed here and not yet saved stay; the
    /// others take the file's values (an agent may have edited them), and
    /// those are heard at once: the film, a take playing, and a playback
    /// about to start.
    pub(super) fn sync_volumes(&mut self) {
        let file = Volumes::of(&self.project.config);
        let before = self.volumes;
        for level in Level::ALL {
            if !self.unsaved.contains(level) {
                self.volumes.set(level, file.get(level));
            }
        }
        self.volumes.write_into(&mut self.project.config);
        if (self.volumes.film - before.film).abs() > f64::EPSILON {
            self.media.film_set_gain_db(self.volumes.film);
        }
        if (self.volumes.voice - before.voice).abs() > f64::EPSILON {
            self.media.take_set_gain_db(self.volumes.voice);
        }
        self.recorder
            .set_playback_levels(self.volumes.film, self.volumes.voice);
    }

    /// The levels block under the takes.
    pub(super) fn render_levels(&self) -> AnyElement {
        div()
            .id("levels")
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(14.))
            .pt(px(8.))
            .pb(px(12.))
            .border_t_1()
            .border_color(rgb(colors::SELECTED))
            .text_size(px(13.))
            .child(div().text_color(rgb(colors::DIM)).child("Levels"))
            .children(
                self.shown_levels()
                    .into_iter()
                    .map(|level| self.level_row(level)),
            )
            .into_any_element()
    }

    fn level_row(&self, level: Level) -> AnyElement {
        let state = self.levels.slider(level);
        let db = self.volumes.get(level);
        let (min, max) = level.range();
        #[allow(clippy::cast_possible_truncation)]
        let fill = ((db - min) / (max - min)).clamp(0.0, 1.0) as f32;
        let about: SharedString = level.about().into();
        div()
            .id(("level", level as usize))
            .flex()
            .items_center()
            .gap(px(10.))
            .tooltip(move |_, cx| cx.new(|_| Tip(about.clone())).into())
            .child(div().w(px(56.)).child(level.label()))
            .child(
                Slider::new(state)
                    .accessibility_label(level.about())
                    .flex_1()
                    .h(px(16.))
                    .child(
                        SliderTrack::new(state)
                            .relative()
                            .w_full()
                            .h(px(16.))
                            .flex()
                            .items_center()
                            .child(
                                SliderIndicator::new(state)
                                    .w_full()
                                    .h(px(4.))
                                    .rounded(px(2.))
                                    .bg(rgb(colors::SELECTED))
                                    .child(
                                        div().h_full().w(relative(fill)).bg(rgb(colors::ACCENT)),
                                    ),
                            )
                            .child(
                                SliderThumb::new(state)
                                    .absolute()
                                    .top(px(2.))
                                    .left(relative(fill))
                                    .ml(px(-6.))
                                    .size(px(12.))
                                    .rounded_full()
                                    .bg(rgb(colors::TEXT)),
                            ),
                    ),
            )
            .child(
                div()
                    .w(px(52.))
                    .flex()
                    .justify_end()
                    .text_color(rgb(colors::DIM))
                    .child(db_text(db)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::db_text;

    #[test]
    fn levels_read_as_whole_decibels() {
        assert_eq!(db_text(-12.0), "−12 dB");
        assert_eq!(db_text(-0.4), "0 dB");
        assert_eq!(db_text(0.0), "0 dB");
        assert_eq!(db_text(6.0), "+6 dB");
    }
}
