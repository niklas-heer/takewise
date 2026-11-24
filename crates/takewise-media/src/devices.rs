//! Audio devices as cpal and CoreAudio see them.

use cpal::traits::{DeviceTrait as _, HostTrait as _};

use crate::error::{Error, Result};
use crate::latency::{output_source, transport};

/// How a device is connected. Bluetooth output is not supported for
/// recording: its latency is long and varies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    BuiltIn,
    Usb,
    Bluetooth,
    Other,
}

impl Transport {
    /// A short lowercase name for output: `built-in`, `usb`, `bluetooth`,
    /// `other`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::Usb => "usb",
            Self::Bluetooth => "bluetooth",
            Self::Other => "other",
        }
    }
}

/// What an output plays through now, from its current CoreAudio data
/// source. The Mac's built-in output reports `'ispk'` for its speakers and
/// `'hdpn'` for the headphone jack; most other devices report no data
/// source at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputSource {
    Speakers,
    Headphones,
    Unknown,
}

/// An audio device: its name, current sample rate and channel count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub transport: Transport,
    /// For an output, what it plays through now; [`OutputSource::Unknown`]
    /// for inputs.
    pub output_source: OutputSource,
}

/// The input devices with a usable input configuration. Devices that report
/// none are left out.
pub fn input_devices() -> Result<Vec<DeviceInfo>> {
    let host = cpal::default_host();
    let mut out = Vec::new();
    for device in host.input_devices()? {
        if let Ok(info) = info(&device, true) {
            out.push(info);
        }
    }
    Ok(out)
}

/// The output devices, where the film (and later the monitor) can play.
/// Devices that report no output configuration are left out.
pub fn output_devices() -> Result<Vec<DeviceInfo>> {
    let host = cpal::default_host();
    let mut out = Vec::new();
    for device in host.output_devices()? {
        if let Ok(info) = info(&device, false) {
            out.push(info);
        }
    }
    Ok(out)
}

/// The system's default input device.
pub fn default_input() -> Result<DeviceInfo> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or(Error::NoDefaultDevice("input"))?;
    info(&device, true)
}

/// The system's default output device, where the film plays.
pub fn default_output() -> Result<DeviceInfo> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or(Error::NoDefaultDevice("output"))?;
    info(&device, false)
}

/// The system's default output device: its name and CoreAudio UID.
pub fn default_output_uid() -> Result<(String, String)> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or(Error::NoDefaultDevice("output"))?;
    Ok((name(&device)?, uid(&device)?))
}

fn info(device: &cpal::Device, input: bool) -> Result<DeviceInfo> {
    let name = name(device)?;
    let config = if input {
        device.default_input_config()?
    } else {
        device.default_output_config()?
    };
    let uid = uid(device).ok();
    Ok(DeviceInfo {
        name,
        sample_rate: config.sample_rate(),
        channels: config.channels(),
        transport: uid.as_deref().map_or(Transport::Other, transport),
        output_source: match uid.as_deref() {
            Some(uid) if !input => output_source(uid),
            _ => OutputSource::Unknown,
        },
    })
}

fn name(device: &cpal::Device) -> Result<String> {
    Ok(device.description()?.name().to_owned())
}

/// The input device with exactly this name.
pub(crate) fn find_input(name_wanted: &str) -> Result<cpal::Device> {
    for device in cpal::default_host().input_devices()? {
        if name(&device).is_ok_and(|name| name == name_wanted) {
            return Ok(device);
        }
    }
    Err(Error::NoInputDevice(name_wanted.to_owned()))
}

/// The output device with exactly this name.
pub(crate) fn find_output(name_wanted: &str) -> Result<cpal::Device> {
    for device in cpal::default_host().output_devices()? {
        if name(&device).is_ok_and(|name| name == name_wanted) {
            return Ok(device);
        }
    }
    Err(Error::NoOutputDevice(name_wanted.to_owned()))
}

/// The CoreAudio UID of the output device with exactly this name: what
/// [`crate::FilmPlayer::set_output_uid`] and
/// [`crate::TakePlayer::set_output_uid`] take.
pub fn output_uid(name: &str) -> Result<String> {
    uid(&find_output(name)?)
}

/// The device's CoreAudio UID.
pub(crate) fn uid(device: &cpal::Device) -> Result<String> {
    Ok(device.id()?.id().to_owned())
}
