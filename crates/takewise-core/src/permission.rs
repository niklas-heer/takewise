//! The microphone permission, which belongs to `Takewise.app`: what it is
//! and what to tell the narrator when it is missing.

use serde::{Deserialize, Serialize};

/// Where the narrator turns the permission on.
pub const MICROPHONE_SETTINGS: &str = "System Settings › Privacy & Security › Microphone";

/// macOS's answer for the microphone (`AVAuthorizationStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MicPermission {
    Granted,
    /// The narrator said no, or turned it off later.
    Denied,
    /// A device-management profile forbids it.
    Restricted,
    /// Not asked yet: the first take asks.
    NotDetermined,
}

impl MicPermission {
    pub fn as_str(self) -> &'static str {
        match self {
            MicPermission::Granted => "granted",
            MicPermission::Denied => "denied",
            MicPermission::Restricted => "restricted",
            MicPermission::NotDetermined => "not-determined",
        }
    }
}

/// What to tell the narrator when recording cannot work; `None` while it
/// can (granted, or not asked yet).
pub fn mic_advice(permission: MicPermission) -> Option<String> {
    match permission {
        MicPermission::Granted | MicPermission::NotDetermined => None,
        MicPermission::Denied => Some(format!(
            "Takewise may not use the microphone. Open {MICROPHONE_SETTINGS}, turn on Takewise, then quit and reopen Takewise."
        )),
        MicPermission::Restricted => Some(format!(
            "This Mac's management profile does not allow Takewise to use the microphone ({MICROPHONE_SETTINGS}); ask its administrator."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_denied_message() {
        let denied = mic_advice(MicPermission::Denied).unwrap();
        assert!(
            denied.contains("System Settings › Privacy & Security › Microphone")
                && denied.contains("turn on Takewise"),
            "{denied}"
        );
        let restricted = mic_advice(MicPermission::Restricted).unwrap();
        assert!(restricted.contains("administrator"), "{restricted}");
        assert_eq!(mic_advice(MicPermission::Granted), None);
        assert_eq!(
            mic_advice(MicPermission::NotDetermined),
            None,
            "the first take asks"
        );
        assert_eq!(
            serde_json::to_value(MicPermission::NotDetermined).unwrap(),
            "not-determined"
        );
    }
}
