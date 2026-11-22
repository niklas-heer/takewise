//! Device properties from CoreAudio: latencies, transport and sample rate.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectPropertyScope, AudioObjectPropertySelector, kAudioDevicePropertyLatency,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertySafetyOffset,
    kAudioDevicePropertyStreams, kAudioDevicePropertyTransportType,
    kAudioDeviceTransportTypeBluetooth, kAudioDeviceTransportTypeBluetoothLE,
    kAudioDeviceTransportTypeBuiltIn, kAudioDeviceTransportTypeUSB,
    kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput, kAudioObjectSystemObject,
    kAudioStreamPropertyLatency,
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
