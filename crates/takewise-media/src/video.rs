//! Video frames from AVPlayer for GPUI's `surface()` element.

use std::ptr::NonNull;

use objc2::AnyThread as _;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_av_foundation::AVPlayerItemVideoOutput;
use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_media::CMTime;
use objc2_core_video::{
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString};

use crate::Frame;
use crate::player::host_time_now_s;

/// An output delivering NV12 full-range, Metal-compatible, IOSurface-backed
/// pixel buffers, the format GPUI's `surface()` draws.
pub(crate) fn nv12_output() -> Retained<AVPlayerItemVideoOutput> {
    // SAFETY: CoreVideo's key constants are CFStrings that live for the
    // whole program, and CFString is toll-free bridged to NSString, so the
    // borrowed references are valid NSStrings for the call.
    let key = |k: &CFString| -> &NSString { unsafe { &*(&raw const *k).cast::<NSString>() } };
    // SAFETY: reading CoreVideo's immutable key constants.
    let keys = unsafe {
        [
            key(kCVPixelBufferPixelFormatTypeKey),
            key(kCVPixelBufferMetalCompatibilityKey),
            key(kCVPixelBufferIOSurfacePropertiesKey),
        ]
    };
    let format = NSNumber::new_u32(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange);
    let yes = NSNumber::new_bool(true);
    // An empty dictionary asks for IOSurface backing with default options.
    let empty: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::new();
    let values: [&AnyObject; 3] = [&format, &yes, &empty];
    let attrs = NSDictionary::from_slices(&keys, &values);
    // SAFETY: a plain initializer; the attributes dictionary has the
    // documented key and value types.
    unsafe {
        AVPlayerItemVideoOutput::initWithPixelBufferAttributes(
            AVPlayerItemVideoOutput::alloc(),
            Some(&attrs),
        )
    }
}

/// The frame due at the current host time, if the output has a new one.
pub(crate) fn new_frame(output: &AVPlayerItemVideoOutput) -> Option<Frame> {
    // SAFETY: AVPlayerItemVideoOutput calls with the current host time (the
    // clock CACurrentMediaTime reads), as a
    // display link would make them; the caller is on the main thread.
    let buffer = unsafe {
        let time = output.itemTimeForHostTime(host_time_now_s());
        if !output.hasNewPixelBufferForItemTime(time) {
            return None;
        }
        let mut shown = CMTime::new(0, 1);
        output.copyPixelBufferForItemTime_itemTimeForDisplay(time, &raw mut shown)?
    };
    let raw = NonNull::new(Retained::into_raw(buffer).cast())?;
    // SAFETY: `raw` carries the +1 reference the copy returned, moved out of
    // the `Retained`; `CFRetained` takes it over and releases it once.
    Some(unsafe { CFRetained::from_raw(raw) })
}
