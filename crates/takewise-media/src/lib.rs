//! Takewise's macOS media bridges: the film and take players (AVPlayer),
//! video frames for GPUI's `surface()`, microphone capture (cpal) and the
//! CoreAudio latencies that place captured samples on the film.
//!
//! Threading: [`FilmPlayer`] and [`TakePlayer`] wrap AVPlayer, which
//! objc2 marks main-thread-only; create and call them on the main thread.
//! [`Capture`] delivers [`CaptureBuffer`]s from CoreAudio's thread through
//! the `Sender` it is given. Everything else may run on any thread.
//!
//! The microphone is never monitored: capture opens an input stream only.

// The one crate allowed `unsafe`: it bridges to AVFoundation, CoreMedia,
// CoreVideo and CoreAudio. Every `unsafe` block carries a SAFETY comment.
#![allow(unsafe_code)]
#![warn(clippy::undocumented_unsafe_blocks)]

mod capture;
mod devices;
mod error;
mod latency;
mod player;
mod video;

pub use capture::{CAPTURE_RATE, Capture, CaptureBuffer};
pub use devices::{DeviceInfo, Transport, default_input, default_output, input_devices};
pub use error::{Error, Result};
pub use latency::input_latency_s;
pub use player::{FilmPlayer, TakePlayer, host_time_now_s, pump_run_loop};
pub use takewise_core::Timebase;

/// A decoded video frame (NV12 full range, IOSurface-backed) for GPUI's
/// `surface()`, which takes exactly this type.
pub type Frame = objc2_core_foundation::CFRetained<objc2_core_video::CVPixelBuffer>;
