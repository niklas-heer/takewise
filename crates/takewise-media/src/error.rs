use std::path::PathBuf;

/// What went wrong in a media bridge.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no input device named {0:?}")]
    NoInputDevice(String),
    #[error("no default {0} device")]
    NoDefaultDevice(&'static str),
    #[error("{0:?} cannot record 48 kHz audio")]
    No48k(String),
    #[error("channel {channel} is out of range 1..={channels} on {device:?}")]
    Channel {
        device: String,
        channel: u16,
        channels: u16,
    },
    #[error("CoreAudio did not report {what} for {device:?}")]
    CoreAudio { device: String, what: &'static str },
    #[error("audio: {0}")]
    Audio(String),
    #[error("{} does not exist", .0.display())]
    MissingFile(PathBuf),
    #[error("players run on the main thread only")]
    NotMainThread,
    #[error("the player has not loaded its media yet")]
    NotReady,
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<cpal::Error> for Error {
    fn from(err: cpal::Error) -> Self {
        Self::Audio(err.to_string())
    }
}
