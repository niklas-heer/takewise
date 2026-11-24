//! Takewise's macOS media bridges: the film and take players (AVPlayer),
//! video frames for GPUI's `surface()`, microphone capture (cpal) and the
//! CoreAudio latencies that place captured samples on the film. Also the
//! one AppKit call the window needs beyond GPUI: the standard About panel.
//!
//! Threading: [`FilmPlayer`] and [`TakePlayer`] wrap AVPlayer, which
//! objc2 marks main-thread-only; create and call them on the main thread.
//! [`Capture`] delivers [`CaptureBuffer`]s from CoreAudio's thread through
//! the `Sender` it is given. Everything else may run on any thread.
//!
//! The microphone is never monitored: capture and the channel meter open
//! input streams only, and [`Monitor`] is a seam that refuses until
//! software monitoring has been proven on real hardware.

// The one crate allowed `unsafe`: it bridges to AVFoundation, CoreMedia,
// CoreVideo and CoreAudio. Every `unsafe` block carries a SAFETY comment.
#![allow(unsafe_code)]
#![warn(clippy::undocumented_unsafe_blocks)]

mod about;
mod capture;
mod devices;
mod error;
mod latency;
mod meter;
mod monitor;
mod permission;
mod player;
mod video;
mod watch;

pub use about::{About, show_about_panel};
pub use capture::{CAPTURE_RATE, Capture, CaptureBuffer};
pub use devices::{
    DeviceInfo, OutputSource, Transport, default_input, default_output, default_output_uid,
    input_devices, output_devices, output_uid,
};
pub use error::{Error, Result};
pub use latency::DeviceLookup;
pub use latency::{buffer_frames, input_latency_s, set_nominal_sample_rate};
pub use meter::{ChannelMeter, METER_WINDOW};
pub use monitor::Monitor;
pub use permission::microphone_permission;
pub use player::{FilmPlayer, TakePlayer, host_time_now_s, pump_run_loop};
pub use takewise_core::Timebase;
pub use watch::{default_output_object, device_generation, output_object, watching_devices};

/// A decoded video frame (NV12 full range, IOSurface-backed) for GPUI's
/// `surface()`, which takes exactly this type.
pub type Frame = objc2_core_foundation::CFRetained<objc2_core_video::CVPixelBuffer>;
