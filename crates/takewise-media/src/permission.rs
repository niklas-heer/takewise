//! The microphone permission as macOS reports it for this process.
//!
//! macOS attributes it to the process's responsible app: inside
//! `Takewise.app` (started with `open`) that is Takewise; a bare binary run
//! from a terminal gets the terminal's permission instead.

use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
use takewise_core::MicPermission;

/// The microphone permission of this process. Asks nothing: no prompt.
pub fn microphone_permission() -> MicPermission {
    // SAFETY: AVMediaTypeAudio is an immutable framework constant, set
    // whenever AVFoundation is linked.
    let Some(audio) = (unsafe { AVMediaTypeAudio }) else {
        return MicPermission::NotDetermined;
    };
    // SAFETY: a class method that only reads the TCC state, callable from
    // any thread; the media type is audio, which it accepts.
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) };
    match status {
        AVAuthorizationStatus::Authorized => MicPermission::Granted,
        AVAuthorizationStatus::Denied => MicPermission::Denied,
        AVAuthorizationStatus::Restricted => MicPermission::Restricted,
        _ => MicPermission::NotDetermined,
    }
}
