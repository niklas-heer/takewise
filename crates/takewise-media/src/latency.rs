//! Device properties from CoreAudio: latencies, transport and sample rate.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectPropertyScope, AudioObjectPropertySelector, AudioObjectSetPropertyData,
    kAudioDevicePermissionsError, kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyLatency,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertySafetyOffset,
    kAudioDevicePropertyStreams, kAudioDevicePropertyTransportType,
    kAudioDeviceTransportTypeBluetooth, kAudioDeviceTransportTypeBluetoothLE,
    kAudioDeviceTransportTypeBuiltIn, kAudioDeviceTransportTypeUSB,
    kAudioDeviceUnsupportedFormatError, kAudioHardwareBadDeviceError,
    kAudioHardwareIllegalOperationError, kAudioHardwareNotRunningError,
    kAudioHardwarePropertyTranslateUIDToDevice, kAudioHardwareUnsupportedOperationError,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioObjectSystemObject, kAudioStreamPropertyLatency,
};
use objc2_core_foundation::CFString;

use crate::devices::{Transport, find_input, uid};
use crate::error::{Error, Result};

/// Input latency of the named device in seconds: device latency plus safety
/// offset plus the first input stream's latency, at the device's nominal
/// sample rate. The capture buffer size is not part of it, because capture
/// timestamps mark a buffer's first frame.
pub fn input_latency_s(device: &str) -> Result<f64> {
    let missing = |what| Error::CoreAudio {
        device: device.to_owned(),
        what,
    };
    let id = device_id(&uid(&find_input(device)?)?).ok_or_else(|| missing("a device id"))?;
    let scope = kAudioObjectPropertyScopeInput;
    let frames = get_u32(id, kAudioDevicePropertyLatency, scope).unwrap_or(0)
        + get_u32(id, kAudioDevicePropertySafetyOffset, scope).unwrap_or(0)
        + first_stream(id, scope)
            .and_then(|stream| {
                get_u32(
                    stream,
                    kAudioStreamPropertyLatency,
                    kAudioObjectPropertyScopeGlobal,
                )
            })
            .unwrap_or(0);
    let rate = nominal_rate(id)
        .filter(|rate| *rate > 0.0)
        .ok_or_else(|| missing("a sample rate"))?;
    Ok(f64::from(frames) / rate)
}

/// The input buffer size of the named device in frames, as CoreAudio runs
/// it now (the device default unless a stream asked for another).
pub fn buffer_frames(device: &str) -> Result<u32> {
    let missing = |what| Error::CoreAudio {
        device: device.to_owned(),
        what,
    };
    let id = device_id(&uid(&find_input(device)?)?).ok_or_else(|| missing("a device id"))?;
    get_u32(
        id,
        kAudioDevicePropertyBufferFrameSize,
        kAudioObjectPropertyScopeInput,
    )
    .ok_or_else(|| missing("a buffer size"))
}

/// How long [`set_nominal_sample_rate`] waits for the device to switch.
const RATE_SWITCH_WAIT: Duration = Duration::from_secs(1);

/// Sets the nominal sample rate of the input device named `device`
/// (`kAudioDevicePropertyNominalSampleRate`), as Audio MIDI Setup does, and
/// waits up to a second for the device to run at it. The error says in
/// words why the device did not switch.
pub fn set_nominal_sample_rate(device: &str, rate: u32) -> Result<()> {
    let id = device_id(&uid(&find_input(device)?)?).ok_or_else(|| Error::CoreAudio {
        device: device.to_owned(),
        what: "a device id",
    })?;
    if nominal_rate(id) == Some(f64::from(rate)) {
        return Ok(());
    }
    let addr = address(
        kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeGlobal,
    );
    let value = f64::from(rate);
    // SAFETY: the nominal sample rate is a Float64; `value` is one and the
    // size says so. CoreAudio copies it during the call.
    let status = unsafe {
        AudioObjectSetPropertyData(
            id,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            u32_size::<f64>(),
            NonNull::from(&value).cast::<c_void>(),
        )
    };
    if status != 0 {
        return Err(rate_error(device, rate, status));
    }
    // The switch completes asynchronously.
    let until = Instant::now() + RATE_SWITCH_WAIT;
    while Instant::now() < until {
        if nominal_rate(id) == Some(value) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(Error::SetRate {
        device: device.to_owned(),
        rate,
        reason: "the device accepted the request but kept its rate; set it in Audio MIDI Setup"
            .to_owned(),
    })
}

/// The error for a CoreAudio status that refused a sample rate switch, in
/// words, with the status code for searching.
fn rate_error(device: &str, rate: u32, status: i32) -> Error {
    #[allow(non_upper_case_globals)]
    let why = match status {
        kAudioDeviceUnsupportedFormatError => "the device does not support that rate",
        kAudioDevicePermissionsError => "another app holds the device exclusively",
        kAudioHardwareNotRunningError => "the audio system is not running",
        kAudioHardwareBadDeviceError => "the device is gone",
        kAudioHardwareIllegalOperationError | kAudioHardwareUnsupportedOperationError => {
            "the device does not let apps change its rate"
        }
        _ => "CoreAudio refused the change",
    };
    Error::SetRate {
        device: device.to_owned(),
        rate,
        reason: format!("{why} (CoreAudio status {})", four_cc(status)),
    }
}

/// A CoreAudio status as its four-character code (`'!dat'`) when it is
/// one, else as a number.
fn four_cc(status: i32) -> String {
    let bytes = status.to_be_bytes();
    if bytes.iter().all(|b| b.is_ascii_graphic()) {
        format!("'{}'", String::from_utf8_lossy(&bytes))
    } else {
        status.to_string()
    }
}

/// How a device is connected, from its CoreAudio transport type.
pub(crate) fn transport(device_uid: &str) -> Transport {
    let Some(id) = device_id(device_uid) else {
        return Transport::Other;
    };
    #[allow(non_upper_case_globals)]
    match get_u32(
        id,
        kAudioDevicePropertyTransportType,
        kAudioObjectPropertyScopeGlobal,
    ) {
        Some(kAudioDeviceTransportTypeBuiltIn) => Transport::BuiltIn,
        Some(kAudioDeviceTransportTypeUSB) => Transport::Usb,
        Some(kAudioDeviceTransportTypeBluetooth | kAudioDeviceTransportTypeBluetoothLE) => {
            Transport::Bluetooth
        }
        _ => Transport::Other,
    }
}

fn address(
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn get_u32(
    id: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> Option<u32> {
    let addr = address(selector, scope);
    let mut value: u32 = 0;
    let mut size = u32_size::<u32>();
    // SAFETY: every selector passed here names a UInt32 property, `value`
    // has room for one, and `size` says so.
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast::<c_void>(),
        )
    };
    (status == 0).then_some(value)
}

fn nominal_rate(id: AudioObjectID) -> Option<f64> {
    let addr = address(
        kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeGlobal,
    );
    let mut value: f64 = 0.0;
    let mut size = u32_size::<f64>();
    // SAFETY: the nominal sample rate is a Float64 and `value` holds one.
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast::<c_void>(),
        )
    };
    (status == 0).then_some(value)
}

/// The CoreAudio object id of the device with this UID.
fn device_id(device_uid: &str) -> Option<AudioObjectID> {
    let cf = CFString::from_str(device_uid);
    let cf_ref: *const CFString = &raw const *cf;
    let addr = address(
        kAudioHardwarePropertyTranslateUIDToDevice,
        kAudioObjectPropertyScopeGlobal,
    );
    let mut id: AudioObjectID = 0;
    let mut size = u32_size::<AudioObjectID>();
    // SAFETY: the qualifier is one CFStringRef (kept alive by `cf` for the
    // call) and the result one AudioObjectID; sizes are given in bytes.
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject.cast_unsigned(),
            NonNull::from(&addr),
            u32_size::<*const CFString>(),
            (&raw const cf_ref).cast::<c_void>(),
            NonNull::from(&mut size),
            NonNull::from(&mut id).cast::<c_void>(),
        )
    };
    (status == 0 && id != 0).then_some(id)
}

fn first_stream(id: AudioObjectID, scope: AudioObjectPropertyScope) -> Option<AudioObjectID> {
    let addr = address(kAudioDevicePropertyStreams, scope);
    let mut streams = [0 as AudioObjectID; 16];
    let mut size = u32_size::<[AudioObjectID; 16]>();
    // SAFETY: an array of AudioStreamIDs; `size` gives its length in bytes
    // and CoreAudio writes at most that many.
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut streams).cast::<c_void>(),
        )
    };
    (status == 0 && size >= u32_size::<AudioObjectID>()).then_some(streams[0])
}

#[allow(clippy::cast_possible_truncation)]
const fn u32_size<T>() -> u32 {
    size_of::<T>() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nominal_rate_error_is_reported() {
        let err = rate_error("Duo", 48_000, kAudioDeviceUnsupportedFormatError).to_string();
        assert_eq!(
            err,
            "\"Duo\" could not switch to 48000 Hz: the device does not support that rate (CoreAudio status '!dat')"
        );
        let err = rate_error("Duo", 48_000, kAudioDevicePermissionsError).to_string();
        assert!(err.contains("another app holds the device"), "{err}");
        let err = rate_error("Duo", 48_000, -50).to_string();
        assert!(
            err.ends_with("CoreAudio refused the change (CoreAudio status -50)"),
            "{err}"
        );
    }
}
