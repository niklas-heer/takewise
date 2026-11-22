//! The media calls the window makes, behind a trait so tests can replace
//! AVPlayer, CoreAudio and the host clock with a fake.

use std::path::Path;
use std::sync::mpsc::Sender;

use takewise_core::{AvailableInput, Timebase};
use takewise_media::{Capture, CaptureBuffer, FilmPlayer, Frame, TakePlayer};

/// Everything the window needs from the film player, the take player, the
/// capture and the host clock. Called on the main thread only.
pub trait Media {
    /// Seconds of the host clock: the clock of capture timestamps and start
    /// times.
    fn host_now_s(&self) -> f64;

    fn film_ready(&self) -> bool;
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
}

/// The real media: AVPlayer for the film and takes, cpal for capture.
pub struct RealMedia {
    film: FilmPlayer,
    take: Option<TakePlayer>,
    capture: Option<Capture>,
}

impl RealMedia {
    /// Opens the film. Main thread only.
    pub fn open(film: &Path) -> Result<Self, String> {
        let film = FilmPlayer::open(film).map_err(|err| err.to_string())?;
        Ok(Self {
            film,
            take: None,
            capture: None,
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
}
