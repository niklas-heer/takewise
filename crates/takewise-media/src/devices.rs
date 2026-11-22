//! Audio devices as cpal and CoreAudio see them.

use cpal::traits::{DeviceTrait as _, HostTrait as _};

use crate::error::{Error, Result};
use crate::latency::transport;

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

/// An audio device: its name, current sample rate and channel count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub transport: Transport,
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

fn info(device: &cpal::Device, input: bool) -> Result<DeviceInfo> {
    let name = name(device)?;
    let config = if input {
        device.default_input_config()?
    } else {
        device.default_output_config()?
    };
    Ok(DeviceInfo {
        name,
        sample_rate: config.sample_rate(),
        channels: config.channels(),
        transport: uid(device).map_or(Transport::Other, |uid| transport(&uid)),
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

/// The device's CoreAudio UID.
pub(crate) fn uid(device: &cpal::Device) -> Result<String> {
    Ok(device.id()?.id().to_owned())
}
