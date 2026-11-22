//! The recording session's rules: when the window may record.

use crate::takes::PartialTake;

/// The sample rate every take is recorded at.
pub const RECORD_RATE: u32 = 48_000;

/// Where the recording window is in a take.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionState {
    Idle,
    /// The film plays the pre-roll; recording begins at `until_host_s`.
    CountIn {
        until_host_s: f64,
    },
    Recording {
        line: String,
        partial: PartialTake,
    },
    /// Recording is not possible; the message says why and what to do.
    Blocked(String),
}

/// The state a session starts in, given whether the input can record.
pub fn can_record(input_ok: Result<(), String>) -> SessionState {
    match input_ok {
        Ok(()) => SessionState::Idle,
        Err(message) => SessionState::Blocked(message),
    }
}

/// An input device as the window sees it: its name and current sample rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableInput {
    pub name: String,
    pub sample_rate: u32,
}

/// Whether the configured input device can record: chosen, connected and
/// running at 48 kHz. The error names the problem and the available devices.
pub fn check_input(configured: Option<&str>, available: &[AvailableInput]) -> Result<(), String> {
    let names = if available.is_empty() {
        "no input devices are connected".to_owned()
    } else {
        let list: Vec<String> = available
            .iter()
            .map(|input| format!("{} ({} Hz)", input.name, input.sample_rate))
            .collect();
        format!("available: {}", list.join(", "))
    };
    let Some(name) = configured else {
        return Err(format!(
            "No input device chosen; set [input] device in project.toml ({names})."
        ));
    };
    let Some(input) = available.iter().find(|input| input.name == name) else {
        return Err(format!("Input device {name:?} is not connected ({names})."));
    };
    if input.sample_rate != RECORD_RATE {
        return Err(format!(
            "Input device {name:?} runs at {} Hz; set it to 48 kHz in Audio MIDI Setup ({names}).",
            input.sample_rate
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str, sample_rate: u32) -> AvailableInput {
        AvailableInput {
            name: name.to_owned(),
            sample_rate,
        }
    }

    #[test]
    fn record_blocked_without_device() {
        let available = [input("MacBook Air Microphone", 48_000)];

        let missing = check_input(Some("RODECaster Duo Chat"), &available);
        let SessionState::Blocked(message) = can_record(missing) else {
            panic!("a missing device must block recording");
        };
        assert!(message.contains("RODECaster Duo Chat"), "{message}");
        assert!(message.contains("MacBook Air Microphone"), "{message}");

        let unset = check_input(None, &available);
        assert!(matches!(can_record(unset), SessionState::Blocked(_)));

        let slow = check_input(
            Some("MacBook Air Microphone"),
            &[input("MacBook Air Microphone", 44_100)],
        );
        let SessionState::Blocked(message) = can_record(slow) else {
            panic!("44.1 kHz must block recording");
        };
        assert!(message.contains("44100 Hz"), "{message}");

        let none = check_input(Some("Any"), &[]);
        let SessionState::Blocked(message) = can_record(none) else {
            panic!("no devices must block recording");
        };
        assert!(message.contains("no input devices"), "{message}");

        let ok = check_input(Some("MacBook Air Microphone"), &available);
        assert_eq!(can_record(ok), SessionState::Idle);
    }
}
