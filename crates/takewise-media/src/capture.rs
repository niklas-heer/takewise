//! Capture one channel of a named input device with cpal.
//!
//! Input only: no output stream is ever opened, so the microphone is never
//! monitored.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use cpal::traits::{DeviceTrait as _, StreamTrait as _};
use takewise_core::takes::FULL_SCALE;

use crate::devices::find_input;
use crate::error::{Error, Result};

/// The rate every capture runs at.
pub const CAPTURE_RATE: u32 = 48_000;

/// One buffer of captured audio.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureBuffer {
    /// Host time (seconds of the host clock, as [`crate::host_time_now_s`])
    /// of the buffer's first sample as CoreAudio stamped it, *before* input
    /// latency is removed: subtract [`crate::input_latency_s`] (the core's
    /// `sample_film_time` does).
    pub host_time_s: f64,
    /// One channel at [`CAPTURE_RATE`], in the 24-bit range.
    pub samples: Vec<i32>,
}

/// A running capture. Buffers arrive on CoreAudio's thread and are sent to
/// the `sink` given to [`Capture::start`]; dropping or stopping the capture
/// ends them.
pub struct Capture {
    stream: cpal::Stream,
    error: Arc<Mutex<Option<String>>>,
}

impl Capture {
    /// Starts capturing `channel` (1-based) of the input device named
    /// `device` at 48 kHz.
    pub fn start(device: &str, channel: u16, sink: Sender<CaptureBuffer>) -> Result<Self> {
        let input = find_input(device)?;
        let supported = input
            .supported_input_configs()?
            .filter(|c| c.min_sample_rate() <= CAPTURE_RATE && c.max_sample_rate() >= CAPTURE_RATE)
            .filter(|c| c.sample_format() == cpal::SampleFormat::F32)
            .max_by_key(|c| c.channels())
            .ok_or_else(|| Error::No48k(device.to_owned()))?
            .with_sample_rate(CAPTURE_RATE);
        let channels = supported.channels();
        if channel == 0 || channel > channels {
            return Err(Error::Channel {
                device: device.to_owned(),
                channel,
                channels,
            });
        }
        let index = usize::from(channel - 1);
        let stride = usize::from(channels);
        let error = Arc::new(Mutex::new(None));
        let error_sink = Arc::clone(&error);
        let stream = input.build_input_stream(
            supported.config(),
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                // The callback instant is the AUHAL input timestamp
                // (mHostTime) of the first frame. cpal's `capture` instant
                // is not used: it leaves out the stream latency.
                let host_time_s = info.timestamp().callback.as_nanos() as f64 / 1e9;
                let samples = data
                    .iter()
                    .skip(index)
                    .step_by(stride)
                    .map(|&s| to_24_bit(s))
                    .collect();
                // A closed receiver means the owner is gone; nothing to do.
                let _ = sink.send(CaptureBuffer {
                    host_time_s,
                    samples,
                });
            },
            move |err: cpal::Error| {
                // Overloads and other glitches only lose some audio, which
                // the gap in the buffers' host times shows; the take goes on.
                if ends_capture(err.kind()) {
                    *error_sink.lock().unwrap_or_else(PoisonError::into_inner) =
                        Some(err.to_string());
                }
            },
            Some(Duration::from_secs(2)),
        )?;
        stream.play()?;
        Ok(Self { stream, error })
    }

    /// The error that ended the stream (the device was unplugged, or its
    /// sample rate or format changed), if any. Buffers stop arriving after
    /// it. Overloads (xruns) are not errors here: they leave a gap in the
    /// buffers' host times.
    pub fn last_error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Stops the capture. Buffers already sent stay in the receiver; drain
    /// it after this returns.
    pub fn stop(self) {
        let _ = self.stream.pause();
        drop(self.stream);
    }
}

/// Whether a stream error ends the capture: the device is gone, or its
/// sample rate or format changed under the stream.
fn ends_capture(kind: cpal::ErrorKind) -> bool {
    matches!(
        kind,
        cpal::ErrorKind::DeviceNotAvailable
            | cpal::ErrorKind::StreamInvalidated
            | cpal::ErrorKind::UnsupportedConfig
    )
}

#[allow(clippy::cast_possible_truncation)]
fn to_24_bit(sample: f32) -> i32 {
    (f64::from(sample.clamp(-1.0, 1.0)) * f64::from(FULL_SCALE)).round() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_lost_device_or_format_ends_a_capture() {
        use cpal::ErrorKind;
        for kind in [
            ErrorKind::DeviceNotAvailable,
            ErrorKind::StreamInvalidated,
            ErrorKind::UnsupportedConfig,
        ] {
            assert!(ends_capture(kind), "{kind:?}");
        }
        for kind in [
            ErrorKind::Xrun,
            ErrorKind::DeviceChanged,
            ErrorKind::RealtimeDenied,
            ErrorKind::BackendError,
        ] {
            assert!(!ends_capture(kind), "{kind:?}");
        }
    }

    #[test]
    fn converts_to_the_24_bit_range() {
        assert_eq!(to_24_bit(0.0), 0);
        assert_eq!(to_24_bit(1.0), FULL_SCALE);
        assert_eq!(to_24_bit(-1.0), -FULL_SCALE);
        assert_eq!(to_24_bit(2.0), FULL_SCALE);
        assert_eq!(to_24_bit(0.5), 4_194_304);
    }
}
