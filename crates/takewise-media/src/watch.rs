//! Device changes as CoreAudio reports them, for the window to react to at
//! once instead of noticing on its next poll: a counter that goes up
//! whenever a device comes or goes or the default output changes.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use objc2_core_audio::{
    AudioObjectAddPropertyListener, AudioObjectID, AudioObjectPropertyAddress,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
};

use crate::latency::{address, device_id};

static GENERATION: AtomicU64 = AtomicU64::new(0);
static LISTENING: OnceLock<bool> = OnceLock::new();

/// How often CoreAudio's devices changed (a device connected or gone, or
/// another default output) since the first call, which starts listening.
/// Compare two readings: a different one means something changed.
pub fn device_generation() -> u64 {
    LISTENING.get_or_init(listen);
    GENERATION.load(Ordering::Acquire)
}

/// Whether CoreAudio accepted the listeners; without them the counter
/// never moves and callers fall back to polling.
pub fn watching_devices() -> bool {
    *LISTENING.get_or_init(listen)
}

/// Whether the device with this CoreAudio UID is connected now.
pub fn output_uid_present(uid: &str) -> bool {
    device_id(uid).is_some()
}

fn listen() -> bool {
    let mut all = true;
    for selector in [
        kAudioHardwarePropertyDevices,
        kAudioHardwarePropertyDefaultOutputDevice,
    ] {
        let addr = address(selector, kAudioObjectPropertyScopeGlobal);
        // SAFETY: the system object exists for the process's lifetime; the
        // address is read during the call; the listener is a plain function
        // that only touches an atomic, and is never removed, so it needs no
        // client data.
        let status = unsafe {
            AudioObjectAddPropertyListener(
                kAudioObjectSystemObject.cast_unsigned(),
                NonNull::from(&addr),
                Some(changed),
                std::ptr::null_mut(),
            )
        };
        all &= status == 0;
    }
    all
}

/// CoreAudio's listener: runs on one of its threads.
unsafe extern "C-unwind" fn changed(
    _: AudioObjectID,
    _: u32,
    _: NonNull<AudioObjectPropertyAddress>,
    _: *mut c_void,
) -> i32 {
    GENERATION.fetch_add(1, Ordering::AcqRel);
    0
}
