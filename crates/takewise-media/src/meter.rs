//! Live peak levels of every channel of an input device, for the audio
//! panel while no take records: the channel that carries the microphone is
//! the one that moves.
//!
//! Input only, like [`crate::Capture`]: nothing is played back.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait as _, StreamTrait as _};

use crate::devices::find_input;
use crate::error::{Error, Result};

/// How far back [`ChannelMeter::levels`] looks.
pub const METER_WINDOW: Duration = Duration::from_millis(100);

/// Per-channel peaks of the buffers that arrived within the window.
type Recent = VecDeque<(Instant, Vec<f32>)>;

/// A running meter on every channel of one input device, at the device's
/// current sample rate (it need not be 48 kHz). Dropping it stops the
/// stream.
pub struct ChannelMeter {
    _stream: cpal::Stream,
    recent: Arc<Mutex<Recent>>,
    channels: u16,
}

impl ChannelMeter {
    /// Starts metering every channel of the input device named `device`.
    pub fn start(device: &str) -> Result<Self> {
        let input = find_input(device)?;
        let rate = input.default_input_config()?.sample_rate();
        let supported = input
            .supported_input_configs()?
            .filter(|c| c.min_sample_rate() <= rate && c.max_sample_rate() >= rate)
            .filter(|c| c.sample_format() == cpal::SampleFormat::F32)
            .max_by_key(|c| c.channels())
            .ok_or_else(|| Error::Audio(format!("{device:?} offers no float input")))?
            .with_sample_rate(rate);
        let channels = supported.channels();
        let recent: Arc<Mutex<Recent>> = Arc::new(Mutex::new(VecDeque::new()));
        let sink = Arc::clone(&recent);
        let stream = input.build_input_stream(
            supported.config(),
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let peaks = channel_peaks(data, channels);
                let now = Instant::now();
                // Never block CoreAudio's thread: a reader holding the lock
                // costs this buffer's peaks, which the next one replaces.
                if let Ok(mut recent) = sink.try_lock() {
                    recent.push_back((now, peaks));
                    prune(&mut recent, now);
                }
            },
            // A lost device leaves the levels empty, which the panel shows.
            |_: cpal::Error| {},
            Some(Duration::from_secs(2)),
        )?;
        stream.play()?;
        Ok(Self {
            _stream: stream,
            recent,
            channels,
        })
    }

    /// The number of channels metered.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Peak level of each channel in dBFS over the last [`METER_WINDOW`];
    /// `-inf` for silence or when no buffer arrived.
    pub fn levels(&self) -> Vec<f32> {
        let recent = self.recent.lock().unwrap_or_else(PoisonError::into_inner);
        levels_since(&recent, Instant::now(), self.channels)
    }
}

/// The linear peak of each channel of an interleaved buffer.
fn channel_peaks(data: &[f32], channels: u16) -> Vec<f32> {
    let mut peaks = vec![0.0_f32; usize::from(channels)];
    if channels == 0 {
        return peaks;
    }
    for frame in data.chunks(usize::from(channels)) {
        for (peak, sample) in peaks.iter_mut().zip(frame) {
            *peak = peak.max(sample.abs());
        }
    }
    peaks
}

/// Drops entries older than the window.
fn prune(recent: &mut Recent, now: Instant) {
    while recent
        .front()
        .is_some_and(|(at, _)| now.saturating_duration_since(*at) > METER_WINDOW)
    {
        recent.pop_front();
    }
}

/// Peak of each channel in dBFS over the entries within the window before
/// `now`.
fn levels_since(recent: &Recent, now: Instant, channels: u16) -> Vec<f32> {
    let mut peaks = vec![0.0_f32; usize::from(channels)];
    for (_, entry) in recent
        .iter()
        .filter(|(at, _)| now.saturating_duration_since(*at) <= METER_WINDOW)
    {
        for (peak, value) in peaks.iter_mut().zip(entry) {
            *peak = peak.max(*value);
        }
    }
    peaks.into_iter().map(to_dbfs).collect()
}

fn to_dbfs(peak: f32) -> f32 {
    if peak > 0.0 {
        20.0 * peak.min(1.0).log10()
    } else {
        f32::NEG_INFINITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_each_channel_of_an_interleaved_buffer() {
        let data = [0.1, -0.5, -0.3, 0.2, 0.05, 0.0];
        assert_eq!(channel_peaks(&data, 2), vec![0.3, 0.5]);
        assert_eq!(channel_peaks(&data, 3), vec![0.2, 0.5, 0.3]);
        assert_eq!(channel_peaks(&[], 2), vec![0.0, 0.0]);
    }

    #[test]
    fn levels_cover_only_the_window() {
        let now = Instant::now();
        let old = now.checked_sub(Duration::from_millis(300)).unwrap();
        let fresh = now.checked_sub(Duration::from_millis(50)).unwrap();
        let mut recent: Recent = VecDeque::new();
        recent.push_back((old, vec![1.0, 1.0]));
        recent.push_back((fresh, vec![0.5, 0.0]));
        let levels = levels_since(&recent, now, 2);
        assert!((levels[0] + 6.02).abs() < 0.01, "{levels:?}");
        assert_eq!(levels[1], f32::NEG_INFINITY);

        prune(&mut recent, now);
        assert_eq!(recent.len(), 1, "the old entry is dropped");
    }
}
