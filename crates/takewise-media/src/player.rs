//! The film and take players (AVPlayer) and the host clock.
//!
//! Both players run on the main thread only. Start times are host times
//! (seconds of the host clock, see [`host_time_now_s`]) so the film, a take
//! locked to it, and capture timestamps share one clock.

use std::path::Path;

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_av_foundation::{
    AVPlayer, AVPlayerItem, AVPlayerItemStatus, AVPlayerItemVideoOutput, AVPlayerStatus,
};
use objc2_core_foundation::{CFRetained, CFType};
use objc2_core_media::{CMClock, CMSyncConvertTime, CMSyncGetRelativeRate, CMTime, kCMTimeZero};
use objc2_foundation::{NSDate, NSError, NSRunLoop, NSString, NSURL};
use takewise_core::Timebase;

use crate::Frame;
use crate::error::{Error, Result};
use crate::video;

/// Seconds of the host clock now: the clock of capture timestamps and of
/// the players' start times.
pub fn host_time_now_s() -> f64 {
    // SAFETY: the host time clock is a process-wide singleton; reading its
    // time has no preconditions.
    seconds(unsafe { CMClock::host_time_clock().time() })
}

/// Runs the current thread's run loop for `secs`, so AVFoundation can load
/// media and deliver its updates when no UI event loop runs (CLI and tests).
pub fn pump_run_loop(secs: f64) {
    let until = NSDate::dateWithTimeIntervalSinceNow(secs);
    NSRunLoop::currentRunLoop().runUntilDate(&until);
}

fn cmtime(secs: f64) -> CMTime {
    #[allow(clippy::cast_possible_truncation)]
    let micros = (secs * 1_000_000.0).round() as i64;
    // SAFETY: a plain value constructor.
    unsafe { CMTime::new(micros, 1_000_000) }
}

fn host_cmtime(secs: f64) -> CMTime {
    #[allow(clippy::cast_possible_truncation)]
    let nanos = (secs * 1e9).round() as i64;
    // SAFETY: a plain value constructor; host clock times are nanoseconds.
    unsafe { CMTime::new(nanos, 1_000_000_000) }
}

fn seconds(time: CMTime) -> f64 {
    // SAFETY: a plain value accessor.
    unsafe { time.seconds() }
}

/// One AVPlayer with its item, as both players use it.
struct Av {
    player: Retained<AVPlayer>,
    item: Retained<AVPlayerItem>,
    host: CFRetained<CMClock>,
}

impl Av {
    fn open(path: &Path) -> Result<Self> {
        let mtm = MainThreadMarker::new().ok_or(Error::NotMainThread)?;
        let abs = std::path::absolute(path).map_err(|_| Error::MissingFile(path.to_owned()))?;
        if !abs.is_file() {
            return Err(Error::MissingFile(abs));
        }
        let url = NSURL::fileURLWithPath(&NSString::from_str(&abs.to_string_lossy()));
        // SAFETY: AVFoundation constructors, on the main thread (`mtm`).
        // Synchronized starts (setRate:time:atHostTime:) require
        // automaticallyWaitsToMinimizeStalling to be off.
        let (item, player) = unsafe {
            let item = AVPlayerItem::playerItemWithURL(&url, mtm);
            let player = AVPlayer::playerWithPlayerItem(Some(&item), mtm);
            player.setAutomaticallyWaitsToMinimizeStalling(false);
            (item, player)
        };
        // SAFETY: the host time clock singleton.
        let host = unsafe { CMClock::host_time_clock() };
        Ok(Self { player, item, host })
    }

    fn is_ready(&self) -> bool {
        // SAFETY: a KVO property read on the main thread.
        unsafe { self.item.status() == AVPlayerItemStatus::ReadyToPlay }
    }

    /// Why the item or the player can no longer play, once either failed
    /// (AVFoundation then needs a new item, or a new player).
    fn failure(&self) -> Option<String> {
        // SAFETY: KVO property reads on the main thread.
        let (item, player) = unsafe {
            (
                (self.item.status() == AVPlayerItemStatus::Failed).then(|| self.item.error()),
                (self.player.status() == AVPlayerStatus::Failed).then(|| self.player.error()),
            )
        };
        let describe = |error: Option<Retained<NSError>>| {
            error.map_or_else(
                || "no reason given".to_owned(),
                |error| error.localizedDescription().to_string(),
            )
        };
        item.map(describe).or_else(|| player.map(describe))
    }

    /// The item's and the player's status and error, for diagnostics.
    fn state(&self) -> String {
        // SAFETY: KVO property reads on the main thread.
        let (item, item_error, player, player_error) = unsafe {
            (
                self.item.status().0,
                self.item.error(),
                self.player.status().0,
                self.player.error(),
            )
        };
        let error = |error: Option<Retained<NSError>>| {
            error.map_or_else(
                || "none".to_owned(),
                |error| format!("{} ({})", error.localizedDescription(), error.code()),
            )
        };
        format!(
            "item status {item} error {}, player status {player} error {}, rate {}",
            error(item_error),
            error(player_error),
            // SAFETY: a main-thread AVPlayer read.
            unsafe { self.player.rate() }
        )
    }

    /// Plays from `media_s` starting exactly at host time `host_time_s`.
    fn play_at(&self, media_s: f64, host_time_s: f64) -> Result<()> {
        if !self.is_ready() {
            return Err(Error::NotReady);
        }
        // SAFETY: main-thread AVPlayer calls on a ready item, with stalling
        // avoidance off (set in `open`), as setRate:time:atHostTime: needs.
        unsafe {
            self.player.setRate(0.0);
            self.player
                .setRate_time_atHostTime(1.0, cmtime(media_s), host_cmtime(host_time_s));
        }
        Ok(())
    }

    fn pause(&self) {
        // SAFETY: a main-thread AVPlayer call.
        unsafe { self.player.setRate(0.0) };
    }

    fn seek(&self, media_s: f64) {
        // SAFETY: main-thread AVPlayer calls; a frame-exact seek.
        unsafe {
            self.player.setRate(0.0);
            self.player.seekToTime_toleranceBefore_toleranceAfter(
                cmtime(media_s),
                kCMTimeZero,
                kCMTimeZero,
            );
        }
    }

    fn current_s(&self) -> f64 {
        // SAFETY: a main-thread AVPlayer read.
        seconds(unsafe { self.player.currentTime() })
    }

    /// Level in dB; AVPlayer cannot amplify, so gains above 0 dB play at
    /// 0 dB.
    fn set_gain_db(&self, db: f64) {
        #[allow(clippy::cast_possible_truncation)]
        let volume = 10f64.powf(db / 20.0).clamp(0.0, 1.0) as f32;
        // SAFETY: a main-thread AVPlayer setter.
        unsafe { self.player.setVolume(volume) };
    }

    /// Plays through the output device with this name instead of the
    /// system's default output.
    fn set_output_device(&self, name: &str) -> Result<()> {
        self.set_output_uid(Some(&crate::devices::output_uid(name)?));
        Ok(())
    }

    /// Plays through the output device with this CoreAudio UID, or with
    /// `None` through the system's default output.
    fn set_output_uid(&self, uid: Option<&str>) {
        let uid = uid.map(NSString::from_str);
        // SAFETY: a main-thread AVPlayer setter (macOS only) with a CoreAudio
        // device UID, or nil for the default output.
        unsafe { self.player.setAudioOutputDeviceUniqueID(uid.as_deref()) };
    }

    /// The UID of the output device set with `set_output_device`; `None`
    /// while the player follows the system's default output.
    fn output_device_uid(&self) -> Option<String> {
        // SAFETY: a main-thread AVPlayer read (macOS only).
        unsafe { self.player.audioOutputDeviceUniqueID() }.map(|uid| uid.to_string())
    }

    fn timebase(&self) -> Timebase {
        let now = host_time_now_s();
        // SAFETY: AVPlayerItem.timebase is readable on the main thread.
        let Some(tb) = (unsafe { self.item.timebase() }) else {
            return Timebase {
                rate: 0.0,
                film_anchor_s: self.current_s(),
                host_anchor_s: now,
            };
        };
        let host: &CFType = &self.host;
        let timebase: &CFType = &tb;
        // SAFETY: both arguments are a CMClock or a CMTimebase, as
        // CMSyncConvertTime and CMSyncGetRelativeRate require.
        let (film, rate) = unsafe {
            (
                seconds(CMSyncConvertTime(host_cmtime(now), host, timebase)),
                CMSyncGetRelativeRate(timebase, host),
            )
        };
        Timebase {
            rate: if rate.is_finite() { rate } else { 0.0 },
            film_anchor_s: film,
            host_anchor_s: now,
        }
    }
}

/// The film: AVPlayer with a video output for GPUI. Main thread only.
pub struct FilmPlayer {
    av: Av,
    output: Retained<AVPlayerItemVideoOutput>,
    frame: Option<Frame>,
}

impl FilmPlayer {
    /// Opens the film. It loads asynchronously; [`FilmPlayer::is_ready`]
    /// turns true once the main run loop has run a little.
    pub fn open(path: &Path) -> Result<Self> {
        let av = Av::open(path)?;
        let output = video::nv12_output();
        // SAFETY: attaching the output to the item before playback, on the
        // main thread.
        unsafe { av.item.addOutput(&output) };
        Ok(Self {
            av,
            output,
            frame: None,
        })
    }

    pub fn is_ready(&self) -> bool {
        self.av.is_ready()
    }

    /// Why it can no longer play, once its item or player failed (a new
    /// one must be opened then).
    pub fn failure(&self) -> Option<String> {
        self.av.failure()
    }

    /// Its item's and player's status and error, for diagnostics.
    pub fn state(&self) -> String {
        self.av.state()
    }

    /// Plays from film time `film_s` starting exactly at host time
    /// `host_time_s` (now or later). Until then the film holds its frame.
    /// Fails with [`Error::NotReady`] before the film has loaded.
    ///
    /// Returns the timebase this schedules: film `film_s` at `host_time_s`,
    /// rate 1, so samples captured while waiting for the start map to film
    /// times before `film_s` (negative ones for a held first frame). Record
    /// it in the take's timebase history as valid from the call on: the
    /// player's own timebase stays stopped until the start and only then
    /// matches it.
    pub fn play_at(&self, film_s: f64, host_time_s: f64) -> Result<Timebase> {
        self.av.play_at(film_s, host_time_s)?;
        Ok(Timebase {
            rate: 1.0,
            film_anchor_s: film_s,
            host_anchor_s: host_time_s,
        })
    }

    pub fn pause(&self) {
        self.av.pause();
    }

    /// Pauses and seeks frame-exactly to `film_s`. The seek completes
    /// asynchronously.
    pub fn seek(&self, film_s: f64) {
        self.av.seek(film_s);
    }

    /// Film audio level in dB; AVPlayer cannot amplify, so gains above
    /// 0 dB play at 0 dB.
    pub fn set_gain_db(&self, db: f64) {
        self.av.set_gain_db(db);
    }

    pub fn set_muted(&self, muted: bool) {
        // SAFETY: a main-thread AVPlayer setter.
        unsafe { self.av.player.setMuted(muted) };
    }

    /// Plays through the output device with this name instead of the
    /// system's default output.
    pub fn set_output_device(&self, name: &str) -> Result<()> {
        self.av.set_output_device(name)
    }

    /// Plays through the output device with this UID (see
    /// [`crate::output_uid`]), or with `None` the system's default output.
    pub fn set_output_uid(&self, uid: Option<&str>) {
        self.av.set_output_uid(uid);
    }

    /// The UID of the output device the film plays through; `None` while it
    /// follows the system's default output.
    pub fn output_device_uid(&self) -> Option<String> {
        self.av.output_device_uid()
    }

    pub fn current_film_s(&self) -> f64 {
        self.av.current_s()
    }

    /// The host-to-film mapping now in effect, read from the item's
    /// timebase. Before a scheduled start it is stopped; use the timebase
    /// [`FilmPlayer::play_at`] returns for that stretch.
    pub fn timebase(&self) -> Timebase {
        self.av.timebase()
    }

    /// The frame to show now: a new one when the video output has one for
    /// the current host time, else the last one shown. Poll it once per
    /// rendered frame.
    pub fn latest_frame(&mut self) -> Option<Frame> {
        if let Some(frame) = video::new_frame(&self.output) {
            self.frame = Some(frame);
        }
        self.frame.clone()
    }
}

/// A take played locked to the film (a second AVPlayer, audio only). Main
/// thread only.
///
/// It plays through the system's default output until
/// [`TakePlayer::set_output_device`] says otherwise, so a take heard with
/// the film needs the film's output set on it too.
pub struct TakePlayer {
    av: Av,
}

impl TakePlayer {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            av: Av::open(path)?,
        })
    }

    pub fn is_ready(&self) -> bool {
        self.av.is_ready()
    }

    /// Why it can no longer play, once its item or player failed (a new
    /// one must be opened then).
    pub fn failure(&self) -> Option<String> {
        self.av.failure()
    }

    /// Its item's and player's status and error, for diagnostics.
    pub fn state(&self) -> String {
        self.av.state()
    }

    /// Plays take time `take_s` at host time `host_time_s`. A negative
    /// `take_s` starts the take's beginning `-take_s` seconds later, so a
    /// take is placed by the same arithmetic as the film.
    pub fn play_at(&self, take_s: f64, host_time_s: f64) -> Result<()> {
        if take_s >= 0.0 {
            self.av.play_at(take_s, host_time_s)
        } else {
            self.av.play_at(0.0, host_time_s - take_s)
        }
    }

    pub fn stop(&self) {
        self.av.pause();
    }

    /// Take time now, in seconds.
    pub fn current_s(&self) -> f64 {
        self.av.current_s()
    }

    /// Take level in dB; AVPlayer cannot amplify, so gains above 0 dB play
    /// at 0 dB. Takes effect at once, also while the take plays.
    pub fn set_gain_db(&self, db: f64) {
        self.av.set_gain_db(db);
    }

    /// Plays through the output device with this name instead of the
    /// system's default output, also while the take plays.
    pub fn set_output_device(&self, name: &str) -> Result<()> {
        self.av.set_output_device(name)
    }

    /// Plays through the output device with this UID (see
    /// [`crate::output_uid`]), or with `None` the system's default output,
    /// also while the take plays.
    pub fn set_output_uid(&self, uid: Option<&str>) {
        self.av.set_output_uid(uid);
    }

    /// The UID of the output device the take plays through; `None` while it
    /// follows the system's default output.
    pub fn output_device_uid(&self) -> Option<String> {
        self.av.output_device_uid()
    }
}
