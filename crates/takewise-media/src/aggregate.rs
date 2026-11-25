//! A private aggregate output device, for the hand-run smoke tests: an
//! output that can be made to vanish and come back under the same UID,
//! as headphones do when they are switched off and on.

use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_audio::{
    AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice, AudioObjectID,
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceMainSubDeviceKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceSubDeviceListKey,
    kAudioAggregateDeviceUIDKey, kAudioSubDeviceUIDKey,
};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

use crate::error::{Error, Result};

fn key(key: &std::ffi::CStr) -> Retained<NSString> {
    NSString::from_str(&key.to_string_lossy())
}

/// Creates a private aggregate device with this UID and name over the
/// output device `sub_uid`; returns its CoreAudio object. Only this
/// process sees it.
pub fn create_aggregate(uid: &str, name: &str, sub_uid: &str) -> Result<AudioObjectID> {
    let sub: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_retained_objects(
        &[&*key(kAudioSubDeviceUIDKey)],
        &[Retained::into_super(Retained::into_super(
            NSString::from_str(sub_uid),
        ))],
    );
    let subs = NSArray::from_retained_slice(&[Retained::into_super(Retained::into_super(sub))]);
    let keys = [
        key(kAudioAggregateDeviceUIDKey),
        key(kAudioAggregateDeviceNameKey),
        key(kAudioAggregateDeviceIsPrivateKey),
        key(kAudioAggregateDeviceSubDeviceListKey),
        key(kAudioAggregateDeviceMainSubDeviceKey),
    ];
    let objects: [Retained<AnyObject>; 5] = [
        Retained::into_super(Retained::into_super(NSString::from_str(uid))),
        Retained::into_super(Retained::into_super(NSString::from_str(name))),
        Retained::into_super(Retained::into_super(Retained::into_super(
            NSNumber::new_i32(1),
        ))),
        Retained::into_super(Retained::into_super(subs)),
        Retained::into_super(Retained::into_super(NSString::from_str(sub_uid))),
    ];
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let description = NSDictionary::from_retained_objects(&key_refs, &objects);
    let mut id: AudioObjectID = 0;
    // SAFETY: an NSDictionary is toll-free bridged to CFDictionary, and
    // this one holds the keys and value types CoreAudio documents for an
    // aggregate device; `id` is a valid out pointer.
    let status = unsafe {
        let cf = &*(Retained::as_ptr(&description).cast::<CFDictionary>());
        AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut id))
    };
    if status == 0 && id != 0 {
        Ok(id)
    } else {
        Err(Error::Audio(format!(
            "could not create the aggregate device ({status})"
        )))
    }
}

/// Destroys an aggregate device made by [`create_aggregate`].
pub fn destroy_aggregate(id: AudioObjectID) -> Result<()> {
    // SAFETY: `id` names an aggregate device this process created.
    let status = unsafe { AudioHardwareDestroyAggregateDevice(id) };
    if status == 0 {
        Ok(())
    } else {
        Err(Error::Audio(format!(
            "could not destroy the aggregate device ({status})"
        )))
    }
}
