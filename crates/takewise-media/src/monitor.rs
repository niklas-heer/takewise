//! Software monitoring of the narrator's voice: the seam the window calls.
//!
//! Not implemented yet. Monitoring waits for the hardware spike that proves
//! it on a real interface (see `docs/design.md`, "Monitoring spike, the gate
//! for 4a"); until then [`Monitor::start`] always fails with
//! [`Error::MonitorUnavailable`], and the window shows the direct-monitoring
//! hint instead.

use crate::error::{Error, Result};

/// A running monitor: the capture's audio played on the same device.
#[derive(Debug)]
pub struct Monitor {
    _private: (),
}

impl Monitor {
    /// Whether software monitoring can run at all in this build; the error
    /// says why not.
    pub fn available() -> Result<()> {
        Err(Error::MonitorUnavailable)
    }

    /// Starts monitoring on the device named `device` at `volume_db`.
    pub fn start(_device: &str, _volume_db: f64) -> Result<Self> {
        Err(Error::MonitorUnavailable)
    }
}
