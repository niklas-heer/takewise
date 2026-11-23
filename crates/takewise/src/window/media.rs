//! The media calls the window makes, behind a trait so tests can replace
//! AVPlayer, CoreAudio and the host clock with a fake.

use std::path::Path;
use std::sync::mpsc::Sender;

use takewise_core::{AvailableInput, Timebase};
use takewise_media::{
    Capture, CaptureBuffer, ChannelMeter, DeviceInfo, FilmPlayer, Frame, Monitor, TakePlayer,
};

/// Everything the window needs from the film player, the take player, the
/// capture and the host clock. Called on the main thread only.
pub trait Media {
    /// Seconds of the host clock: the clock of capture timestamps and start
    /// times.
    fn host_now_s(&self) -> f64;

    fn film_ready(&self) -> bool;
    /// Replaces the film with the one at `path` (the calibration's beep
    /// film, then the project's again), playing through the same output.
    fn film_open(&mut self, path: &Path) -> Result<(), String>;
    /// Plays from `film_s` at `host_s`, holding the frame until then, and
    /// returns the scheduled timebase.
    fn film_play_at(&mut self, film_s: f64, host_s: f64) -> Result<Timebase, String>;
    /// The film's timebase as the player reports it now (stopped before a
    /// scheduled start).
    fn film_timebase(&self) -> Timebase;
    fn film_pause(&mut self);
    fn film_seek(&mut self, film_s: f64);
    fn film_set_gain_db(&mut self, db: f64);
    fn film_set_muted(&mut self, muted: bool);
    /// The frame to draw now (see [`FilmPlayer::latest_frame`]).
    fn latest_frame(&mut self) -> Option<Frame>;

    /// The connected input devices and their current sample rates.
    fn inputs(&self) -> Vec<AvailableInput>;
    /// Input latency of `device` (device + safety offset + stream).
    fn input_latency_s(&self, device: &str) -> Result<f64, String>;
    fn start_capture(
        &mut self,
        device: &str,
        channel: u16,
        sink: Sender<CaptureBuffer>,
    ) -> Result<(), String>;
    /// Stops the capture; buffers already sent stay in the receiver.
    fn stop_capture(&mut self);
    /// The capture's last error, such as an unplugged device.
    fn capture_error(&self) -> Option<String>;

    /// Opens a take for playback; it loads asynchronously.
    fn open_take(&mut self, path: &Path) -> Result<(), String>;
    fn take_ready(&self) -> bool;
    /// Plays take time `take_s` at `host_s` (a negative `take_s` delays the
    /// take's beginning).
    fn take_play_at(&mut self, take_s: f64, host_s: f64) -> Result<(), String>;
    fn stop_take(&mut self);

    /// The connected output devices.
    fn outputs(&self) -> Vec<DeviceInfo>;
    /// The name of the system's default output device.
    fn default_output(&self) -> Option<String>;
    /// Plays the film through the output device with this name.
    fn film_set_output(&mut self, device: &str) -> Result<(), String>;
    /// Starts metering every channel of `device` (replacing any meter).
    fn start_channel_meter(&mut self, device: &str) -> Result<(), String>;
    fn stop_channel_meter(&mut self);
    /// Peak of each channel of the metered device in dBFS over the last
    /// 100 ms; empty without a meter.
    fn channel_levels(&self) -> Vec<f32>;
    /// Sets `device`'s nominal sample rate; the error says why not in words.
    fn set_nominal_rate(&mut self, device: &str, rate: u32) -> Result<(), String>;
    /// The input buffer size of `device` in frames, when CoreAudio says.
    fn buffer_frames(&self, device: &str) -> Option<u32>;
    /// Why software monitoring cannot run at all, if it cannot; the seam
    /// monitoring plugs into once the hardware spike proved it.
    fn monitor_unavailable(&self) -> Option<String>;
    /// Whether macOS lets this app use the microphone.
    fn microphone_permission(&self) -> takewise_core::MicPermission;
}

/// The real media: AVPlayer for the film and takes, cpal for capture.
pub struct RealMedia {
    film: FilmPlayer,
    /// The output the film was last set to play through.
    output: Option<String>,
    take: Option<TakePlayer>,
    capture: Option<Capture>,
    meter: Option<ChannelMeter>,
}

impl RealMedia {
    /// Opens the film. Main thread only.
    pub fn open(film: &Path) -> Result<Self, String> {
        let film = FilmPlayer::open(film).map_err(|err| err.to_string())?;
        Ok(Self {
            film,
            output: None,
            take: None,
            capture: None,
            meter: None,
        })
    }

    /// Plays the film through the output device with this name.
    pub fn set_output_device(&self, name: &str) -> Result<(), String> {
        self.film
            .set_output_device(name)
            .map_err(|err| err.to_string())
    }
}

impl Media for RealMedia {
    fn host_now_s(&self) -> f64 {
        takewise_media::host_time_now_s()
    }

    fn film_ready(&self) -> bool {
        self.film.is_ready()
    }

    fn film_open(&mut self, path: &Path) -> Result<(), String> {
        let film = FilmPlayer::open(path).map_err(|err| err.to_string())?;
        if let Some(output) = &self.output {
            film.set_output_device(output)
                .map_err(|err| err.to_string())?;
        }
        self.film.pause();
        self.film = film;
        Ok(())
    }

    fn film_play_at(&mut self, film_s: f64, host_s: f64) -> Result<Timebase, String> {
        self.film
            .play_at(film_s, host_s)
            .map_err(|err| err.to_string())
    }

    fn film_timebase(&self) -> Timebase {
        self.film.timebase()
    }

    fn film_pause(&mut self) {
        self.film.pause();
    }

    fn film_seek(&mut self, film_s: f64) {
        self.film.seek(film_s);
    }

    fn film_set_gain_db(&mut self, db: f64) {
        self.film.set_gain_db(db);
    }

    fn film_set_muted(&mut self, muted: bool) {
        self.film.set_muted(muted);
    }

    fn latest_frame(&mut self) -> Option<Frame> {
        self.film.latest_frame()
    }

    fn inputs(&self) -> Vec<AvailableInput> {
        takewise_media::input_devices()
            .unwrap_or_default()
            .into_iter()
            .map(|device| AvailableInput {
                name: device.name,
                sample_rate: device.sample_rate,
            })
            .collect()
    }

    fn input_latency_s(&self, device: &str) -> Result<f64, String> {
        takewise_media::input_latency_s(device).map_err(|err| err.to_string())
    }

    fn start_capture(
        &mut self,
        device: &str,
        channel: u16,
        sink: Sender<CaptureBuffer>,
    ) -> Result<(), String> {
        self.stop_capture();
        let capture = Capture::start(device, channel, sink).map_err(|err| err.to_string())?;
        self.capture = Some(capture);
        Ok(())
    }

    fn stop_capture(&mut self) {
        if let Some(capture) = self.capture.take() {
            capture.stop();
        }
    }

    fn capture_error(&self) -> Option<String> {
        self.capture.as_ref().and_then(Capture::last_error)
    }

    fn open_take(&mut self, path: &Path) -> Result<(), String> {
        self.stop_take();
        self.take = Some(TakePlayer::open(path).map_err(|err| err.to_string())?);
        Ok(())
    }

    fn take_ready(&self) -> bool {
        self.take.as_ref().is_some_and(TakePlayer::is_ready)
    }

    fn take_play_at(&mut self, take_s: f64, host_s: f64) -> Result<(), String> {
        let take = self.take.as_ref().ok_or("no take is open")?;
        take.play_at(take_s, host_s).map_err(|err| err.to_string())
    }

    fn stop_take(&mut self) {
        if let Some(take) = &self.take {
            take.stop();
        }
    }

    fn outputs(&self) -> Vec<DeviceInfo> {
        takewise_media::output_devices().unwrap_or_default()
    }

    fn default_output(&self) -> Option<String> {
        takewise_media::default_output().ok().map(|d| d.name)
    }

    fn film_set_output(&mut self, device: &str) -> Result<(), String> {
        self.set_output_device(device)?;
        self.output = Some(device.to_owned());
        Ok(())
    }

    fn start_channel_meter(&mut self, device: &str) -> Result<(), String> {
        self.meter = None;
        self.meter = Some(ChannelMeter::start(device).map_err(|err| err.to_string())?);
        Ok(())
    }

    fn stop_channel_meter(&mut self) {
        self.meter = None;
    }

    fn channel_levels(&self) -> Vec<f32> {
        self.meter
            .as_ref()
            .map(ChannelMeter::levels)
            .unwrap_or_default()
    }

    fn set_nominal_rate(&mut self, device: &str, rate: u32) -> Result<(), String> {
        takewise_media::set_nominal_sample_rate(device, rate).map_err(|err| err.to_string())
    }

    fn buffer_frames(&self, device: &str) -> Option<u32> {
        takewise_media::buffer_frames(device).ok()
    }

    fn monitor_unavailable(&self) -> Option<String> {
        Monitor::available().err().map(|err| err.to_string())
    }

    fn microphone_permission(&self) -> takewise_core::MicPermission {
        takewise_media::microphone_permission()
    }
}
