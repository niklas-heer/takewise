//! The media calls the window makes, behind a trait so tests can replace
//! AVPlayer, CoreAudio and the host clock with a fake.

use std::cell::RefCell;
use std::path::Path;
use std::sync::mpsc::Sender;

use takewise_core::{AvailableInput, Timebase};
use takewise_media::{
    Capture, CaptureBuffer, ChannelMeter, DeviceInfo, DeviceLookup, FilmPlayer, Frame, Monitor,
    TakePlayer,
};

/// Everything the window needs from the film player, the take player, the
/// capture and the host clock. Called on the main thread only.
pub trait Media {
    /// Seconds of the host clock: the clock of capture timestamps and start
    /// times.
    fn host_now_s(&self) -> f64;

    fn film_ready(&self) -> bool;
    /// Why the film's player can no longer play, once it failed: macOS
    /// fails a playing item whose output device goes away, for good, even
    /// when the device comes back; only a new player plays again.
    fn film_failure(&self) -> Option<String>;
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

    /// Opens a take (or a passage) for playback, through the output last
    /// set with [`Media::set_output`] (the very device the film plays
    /// through, not looked up again); it loads asynchronously.
    fn open_take(&mut self, path: &Path) -> Result<(), String>;
    fn take_ready(&self) -> bool;
    /// Why the open take's player can no longer play, once it failed (as
    /// for the film; the next take opened gets a new player).
    fn take_failure(&self) -> Option<String>;
    /// Plays take time `take_s` at `host_s` (a negative `take_s` delays the
    /// take's beginning).
    fn take_play_at(&mut self, take_s: f64, host_s: f64) -> Result<(), String>;
    /// The open take's level in dB (0 at most), at once, also while it
    /// plays.
    fn take_set_gain_db(&mut self, db: f64);
    fn stop_take(&mut self);

    /// The connected output devices, as the panel lists them.
    fn outputs(&self) -> Vec<DeviceInfo>;
    /// Whether an output device with this name is connected, by the same
    /// lookup [`Media::set_output`] uses (a device whose configuration
    /// cannot be read now, such as Bluetooth headphones switching profile,
    /// is still there).
    fn output_connected(&self, device: &str) -> bool;
    /// A counter that goes up whenever the system's devices change (one
    /// comes or goes, another default output); a different reading means
    /// something changed since the last.
    fn device_generation(&self) -> u64;
    /// The UID of the device the players were last set to (`None`: the
    /// system's default output).
    fn applied_output_uid(&self) -> Option<String>;
    /// The CoreAudio object of the device with this UID while it is
    /// connected: a device that went away and came back has a new one, so
    /// comparing objects tells a reconnect from no change. A failed read is
    /// [`DeviceLookup::Unknown`], not absent. One property read.
    fn output_object(&self, uid: &str) -> DeviceLookup;
    /// The CoreAudio object of the system's default output: one property
    /// read, for every frame.
    fn default_output_object(&self) -> DeviceLookup;
    /// The system's default output: its name and UID (lists devices: not
    /// for every frame).
    fn default_output_uid(&self) -> Option<(String, String)>;
    /// The name of the system's default output device.
    fn default_output(&self) -> Option<String>;
    /// Plays the film and the takes through the output device with this
    /// name (`None`: the system's default output): the film and an open
    /// take at once, and every film and take opened later. All or nothing:
    /// when it fails, both keep the output they had.
    fn set_output(&mut self, device: Option<&str>) -> Result<(), String>;
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
    /// The CoreAudio UID of the output the film and takes were last set to
    /// play through (`None`: the system's default output); every player
    /// opened later gets this same device.
    output_uid: Option<String>,
    take: Option<TakePlayer>,
    capture: Option<Capture>,
    meter: Option<ChannelMeter>,
    /// What costs listing the devices, which the window asks for while it
    /// draws: kept until the devices change (see [`Listed`]).
    outputs: Listed<Vec<DeviceInfo>>,
    default_output: Listed<Option<String>>,
    buffer: Listed<Option<(String, Option<u32>)>>,
}

/// How long a listing is kept should device changes go unreported.
const LIST_FRESH_S: f64 = 2.0;

/// A value that costs listing the devices, kept while the device
/// generation stays the same and for at most [`LIST_FRESH_S`].
#[derive(Debug, Default)]
pub struct Listed<T>(RefCell<Option<(u64, f64, T)>>);

impl<T: Clone> Listed<T> {
    /// The kept value, or a new one from `fetch` when the devices changed
    /// (`generation`), it is older than [`LIST_FRESH_S`] at `now`, or
    /// `keep` refuses it.
    pub fn get(
        &self,
        generation: u64,
        now: f64,
        keep: impl FnOnce(&T) -> bool,
        fetch: impl FnOnce() -> T,
    ) -> T {
        if let Some((seen, at, value)) = &*self.0.borrow()
            && *seen == generation
            && now - at < LIST_FRESH_S
            && keep(value)
        {
            return value.clone();
        }
        let value = fetch();
        *self.0.borrow_mut() = Some((generation, now, value.clone()));
        value
    }
}

impl RealMedia {
    /// Opens the film. Main thread only.
    pub fn open(film: &Path) -> Result<Self, String> {
        let film = FilmPlayer::open(film).map_err(|err| err.to_string())?;
        Ok(Self {
            film,
            output_uid: None,
            take: None,
            capture: None,
            meter: None,
            outputs: Listed::default(),
            default_output: Listed::default(),
            buffer: Listed::default(),
        })
    }
}

impl Media for RealMedia {
    fn host_now_s(&self) -> f64 {
        takewise_media::host_time_now_s()
    }

    fn film_ready(&self) -> bool {
        self.film.is_ready()
    }

    fn film_failure(&self) -> Option<String> {
        self.film.failure()
    }

    fn film_open(&mut self, path: &Path) -> Result<(), String> {
        let film = FilmPlayer::open(path).map_err(|err| err.to_string())?;
        film.set_output_uid(self.output_uid.as_deref());
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
        self.take = None;
        let take = TakePlayer::open(path).map_err(|err| err.to_string())?;
        take.set_output_uid(self.output_uid.as_deref());
        self.take = Some(take);
        Ok(())
    }

    fn take_ready(&self) -> bool {
        self.take.as_ref().is_some_and(TakePlayer::is_ready)
    }

    fn take_failure(&self) -> Option<String> {
        self.take.as_ref().and_then(TakePlayer::failure)
    }

    fn take_play_at(&mut self, take_s: f64, host_s: f64) -> Result<(), String> {
        let take = self.take.as_ref().ok_or("no take is open")?;
        take.play_at(take_s, host_s).map_err(|err| err.to_string())
    }

    fn take_set_gain_db(&mut self, db: f64) {
        if let Some(take) = &self.take {
            take.set_gain_db(db);
        }
    }

    fn stop_take(&mut self) {
        if let Some(take) = &self.take {
            take.stop();
        }
    }

    fn outputs(&self) -> Vec<DeviceInfo> {
        let (generation, now) = (self.device_generation(), self.host_now_s());
        self.outputs.get(
            generation,
            now,
            |_| true,
            || takewise_media::output_devices().unwrap_or_default(),
        )
    }

    fn default_output(&self) -> Option<String> {
        let (generation, now) = (self.device_generation(), self.host_now_s());
        self.default_output.get(
            generation,
            now,
            |_| true,
            || takewise_media::default_output().ok().map(|d| d.name),
        )
    }

    fn output_connected(&self, device: &str) -> bool {
        takewise_media::output_uid(device).is_ok()
    }

    fn device_generation(&self) -> u64 {
        takewise_media::device_generation()
    }

    fn applied_output_uid(&self) -> Option<String> {
        self.output_uid.clone()
    }

    fn output_object(&self, uid: &str) -> DeviceLookup {
        takewise_media::output_object(uid)
    }

    fn default_output_object(&self) -> DeviceLookup {
        takewise_media::default_output_object()
    }

    fn default_output_uid(&self) -> Option<(String, String)> {
        takewise_media::default_output_uid().ok()
    }

    fn set_output(&mut self, device: Option<&str>) -> Result<(), String> {
        // One lookup for both players: once the device is found, setting
        // its UID cannot fail, so the film and the take never part.
        let uid = device
            .map(takewise_media::output_uid)
            .transpose()
            .map_err(|err| err.to_string())?;
        self.film.set_output_uid(uid.as_deref());
        if let Some(take) = &self.take {
            take.set_output_uid(uid.as_deref());
        }
        self.output_uid = uid;
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
        let (generation, now) = (self.device_generation(), self.host_now_s());
        let kept = |value: &Option<(String, Option<u32>)>| {
            value.as_ref().is_some_and(|(name, _)| name == device)
        };
        let fetch = || {
            Some((
                device.to_owned(),
                takewise_media::buffer_frames(device).ok(),
            ))
        };
        self.buffer
            .get(generation, now, kept, fetch)
            .and_then(|(_, frames)| frames)
    }

    fn monitor_unavailable(&self) -> Option<String> {
        Monitor::available().err().map(|err| err.to_string())
    }

    fn microphone_permission(&self) -> takewise_core::MicPermission {
        takewise_media::microphone_permission()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::Listed;

    #[test]
    fn a_listing_is_kept_until_the_devices_change_or_it_is_old() {
        let listed = Listed::default();
        let fetched = Cell::new(0);
        let get = |generation, now| {
            listed.get(
                generation,
                now,
                |_| true,
                || {
                    fetched.set(fetched.get() + 1);
                    fetched.get()
                },
            )
        };
        assert_eq!(get(0, 10.0), 1);
        assert_eq!(get(0, 11.0), 1, "the same devices: kept");
        assert_eq!(get(1, 11.0), 2, "the devices changed");
        assert_eq!(get(1, 13.5), 3, "older than two seconds");
        assert_eq!(
            listed.get(1, 13.6, |value| *value != 3, || 9),
            9,
            "refused by the caller"
        );
    }
}
