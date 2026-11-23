//! The `clean` export's processing of each used take: a high-pass at 80 Hz
//! and, with a matching room tone, gentle noise reduction (`afftdn`, 10 dB,
//! the room tone's floor as the noise floor). Takes are never changed: the
//! result is a new 48 kHz float file. `afftdn` delays its output by its
//! window; the delay is measured with a click when a [`Cleaner`] is made and
//! compensated, so cleaned speech sits on the same samples as the take's.

use std::path::Path;

use crate::ffmpeg::{Ffmpeg, read_float_wav, write_float_wav};
use crate::ledger::Recorded;
use crate::room::{RoomTone, room_tones};
use crate::setup::is_gated;

/// The high-pass every cleaned take goes through.
pub const HIGHPASS: &str = "highpass=f=80";
/// How much `afftdn` reduces the noise, in dB.
pub const NOISE_REDUCTION_DB: f64 = 10.0;
/// A room tone stands for a take only when the take's pre-roll floor lies
/// within this many dB of the room tone's: a larger difference means the
/// gain or the room changed (the interface's gain cannot be read).
pub const ROOM_TONE_MATCH_DB: f64 = 6.0;
/// `afftdn` accepts noise floors in this range.
const AFFTDN_FLOOR_DB: (f64, f64) = (-80.0, -20.0);
/// The click fixture: one second at 48 kHz with a click in the middle.
const PROBE_LEN: usize = 48_000;
const PROBE_CLICK: usize = 24_000;

/// What cleaning did to a take.
#[derive(Debug, Clone, PartialEq)]
pub struct CleanInfo {
    /// The noise floor `afftdn` was given, when noise reduction ran.
    pub noise_floor_db: Option<f64>,
}

/// The room tone for a take recorded as `recorded` at `at`, whose pre-roll
/// floor is `take_floor_dbfs`: the newest one recorded on the take's input
/// device and channel on the same calendar day in `tz` (the narrator's local
/// day, not UTC), not digital silence, whose floor lies within
/// [`ROOM_TONE_MATCH_DB`] of the take's. `Err` says why there is none, for
/// the manifest.
pub fn choose_room_tone(
    tones: &[RoomTone],
    recorded: &Recorded,
    at: jiff::Timestamp,
    take_floor_dbfs: f64,
    tz: &jiff::tz::TimeZone,
) -> Result<RoomTone, String> {
    if is_gated(take_floor_dbfs) {
        return Err(
            "the take's pre-roll is digital silence (a gated input); noise reduction skipped"
                .to_owned(),
        );
    }
    let day = |t: jiff::Timestamp| tz.to_datetime(t).date();
    let same_input: Vec<&RoomTone> = tones
        .iter()
        .filter(|t| t.input == recorded.device && t.channel == recorded.channel)
        .filter(|t| day(t.recorded_at) == day(at))
        .collect();
    if same_input.is_empty() {
        return Err(format!(
            "no room tone recorded on {} channel {} on {}; run the setup check's room step",
            recorded.device,
            recorded.channel,
            day(at)
        ));
    }
    let usable: Vec<&&RoomTone> = same_input.iter().filter(|t| !t.gated).collect();
    if usable.is_empty() {
        return Err(
            "the room tone is digital silence (a gated input); noise reduction skipped".to_owned(),
        );
    }
    usable
        .iter()
        .filter(|t| (t.floor_dbfs - take_floor_dbfs).abs() <= ROOM_TONE_MATCH_DB)
        .max_by_key(|t| t.recorded_at)
        .map(|t| (**t).clone())
        .ok_or_else(|| {
            let newest = usable.iter().max_by_key(|t| t.recorded_at).map_or(0.0, |t| t.floor_dbfs);
            format!(
                "the take's pre-roll floor ({take_floor_dbfs:.1} dBFS) is more than {ROOM_TONE_MATCH_DB:.0} dB from the room tone's ({newest:.1} dBFS): the gain or the room changed; run the setup check's room step again"
            )
        })
}

/// [`choose_room_tone`] among the project's room tones, by the system's
/// local calendar day.
pub fn find_room_tone(
    dir: &Path,
    recorded: &Recorded,
    at: jiff::Timestamp,
    take_floor_dbfs: f64,
) -> Result<RoomTone, String> {
    let tones = room_tones(dir).map_err(|err| format!("could not read the room tones: {err}"))?;
    choose_room_tone(
        &tones,
        recorded,
        at,
        take_floor_dbfs,
        &jiff::tz::TimeZone::system(),
    )
}

/// Cleans takes through `ffmpeg`, knowing how much `afftdn` delays them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleaner {
    pub ffmpeg: Ffmpeg,
    /// Samples at 48 kHz `afftdn` delays its output by.
    pub nr_delay: usize,
}

impl Cleaner {
    /// Measures `afftdn`'s delay with a click fixture.
    pub fn new(ffmpeg: Ffmpeg) -> Result<Cleaner, String> {
        let nr_delay = filter_delay(&ffmpeg, &nr_chain(-60.0, 0))?;
        Ok(Cleaner { ffmpeg, nr_delay })
    }

    /// Writes `src` cleaned to `out` (48 kHz mono float, the same length):
    /// high-passed, and noise-reduced with `room`'s floor when given.
    pub fn clean_take(
        &self,
        src: &Path,
        room: Option<&RoomTone>,
        out: &Path,
    ) -> Result<CleanInfo, String> {
        let floor = room.map(|r| r.floor_dbfs.clamp(AFFTDN_FLOOR_DB.0, AFFTDN_FLOOR_DB.1));
        let filters = match floor {
            Some(floor) => nr_chain(floor, self.nr_delay),
            None => HIGHPASS.to_owned(),
        };
        self.ffmpeg.filter_to_float(src, &filters, out)?;
        Ok(CleanInfo {
            noise_floor_db: floor,
        })
    }
}

/// How many samples at 48 kHz the filter chain `filters` delays its output
/// by, measured with a click fixture.
pub fn filter_delay(ffmpeg: &Ffmpeg, filters: &str) -> Result<usize, String> {
    let tmp = tempfile::Builder::new()
        .prefix("takewise-clean-")
        .tempdir()
        .map_err(|err| format!("could not create a temporary directory: {err}"))?;
    let (probe, out) = (tmp.path().join("click.wav"), tmp.path().join("out.wav"));
    write_float_wav(&probe, &click_fixture(), 48_000)?;
    ffmpeg.filter_to_float(&probe, filters, &out)?;
    let (heard, _) = read_float_wav(&out)?;
    let at = peak_index(&heard).ok_or("the click fixture came back silent")?;
    at.checked_sub(PROBE_CLICK)
        .filter(|d| *d < PROBE_LEN / 2)
        .ok_or_else(|| {
            format!("`{filters}` moved a click from sample {PROBE_CLICK} to {at}; cannot place speech exactly")
        })
}

/// `filters` padded at the end and trimmed at the start by `delay` samples,
/// so the output keeps the input's timing and length.
pub fn compensated(filters: &str, delay: usize) -> String {
    if delay == 0 {
        return filters.to_owned();
    }
    format!("apad=pad_len={delay},{filters},atrim=start_sample={delay},asetpts=N/SR/TB")
}

/// The high-pass and noise reduction, compensated for `delay`.
fn nr_chain(floor_db: f64, delay: usize) -> String {
    let nr = format!("afftdn=nr={NOISE_REDUCTION_DB}:nf={floor_db:.1}");
    compensated(&format!("{HIGHPASS},{nr}"), delay)
}

/// Low noise with a click at [`PROBE_CLICK`].
fn click_fixture() -> Vec<f64> {
    let mut state: u32 = 0x2545_f491;
    let mut x: Vec<f64> = (0..PROBE_LEN)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (f64::from(state >> 8) / f64::from(1_u32 << 24) * 2.0 - 1.0) * 1e-4
        })
        .collect();
    x[PROBE_CLICK] = 0.5;
    x
}

fn peak_index(x: &[f64]) -> Option<usize> {
    x.iter()
        .enumerate()
        .filter(|(_, v)| v.abs() > 0.0)
        .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Snapshot;
    use crate::takes::write_take_wav;
    use crate::testutil::noise_at;

    fn recorded() -> Recorded {
        Recorded {
            file: "takes/hook/001.wav".to_owned(),
            sample_rate: 48_000,
            samples: 48_000,
            device: "Duo".to_owned(),
            channel: 1,
            line_offset_s: -2.0,
            snapshot: Snapshot {
                text: "Hi.".to_owned(),
                start: 1.0,
                end: 2.0,
            },
            recovered: false,
            dropouts: None,
            film_shift_ms: None,
        }
    }

    fn tone(at: &str, floor: f64) -> RoomTone {
        RoomTone {
            file: format!("takes/.room/{at}.wav"),
            recorded_at: at.parse().unwrap(),
            input: "Duo".to_owned(),
            channel: 1,
            output: "Duo".to_owned(),
            sample_rate: 48_000,
            samples: 240_000,
            floor_dbfs: floor,
            gated: false,
        }
    }

    const UTC: jiff::tz::TimeZone = jiff::tz::TimeZone::UTC;

    fn at(t: &str) -> jiff::Timestamp {
        t.parse().unwrap()
    }

    #[test]
    fn mismatched_room_tone_skips_nr() {
        let tones = [tone("2026-11-23T09:00:00Z", -70.0)];
        let take_at = at("2026-11-23T10:00:00Z");
        assert!(choose_room_tone(&tones, &recorded(), take_at, -66.0, &UTC).is_ok());
        // 8 dB louder: the gain changed.
        let why = choose_room_tone(&tones, &recorded(), take_at, -62.0, &UTC).unwrap_err();
        assert!(why.contains("more than 6 dB"), "{why}");
        // Another device, channel or day has no room tone.
        let other = Recorded {
            channel: 2,
            ..recorded()
        };
        let why = choose_room_tone(&tones, &other, take_at, -70.0, &UTC).unwrap_err();
        assert!(why.contains("no room tone"), "{why}");
        let why = choose_room_tone(&tones, &recorded(), at("2026-11-24T10:00:00Z"), -70.0, &UTC)
            .unwrap_err();
        assert!(why.contains("no room tone"), "{why}");
        assert!(choose_room_tone(&[], &recorded(), take_at, -70.0, &UTC).is_err());
    }

    #[test]
    fn gated_room_tone_skips_nr() {
        let gated = RoomTone {
            gated: true,
            floor_dbfs: crate::speech::SILENCE_DBFS,
            ..tone("2026-11-23T09:00:00Z", -70.0)
        };
        let take_at = at("2026-11-23T10:00:00Z");
        let why = choose_room_tone(&[gated], &recorded(), take_at, -70.0, &UTC).unwrap_err();
        assert!(why.contains("gated"), "{why}");
        // A take whose own pre-roll is digital silence is not reduced.
        let tones = [tone("2026-11-23T09:00:00Z", -70.0)];
        let why = choose_room_tone(
            &tones,
            &recorded(),
            take_at,
            crate::speech::SILENCE_DBFS,
            &UTC,
        )
        .unwrap_err();
        assert!(why.contains("gated"), "{why}");
    }

    #[test]
    fn room_tone_day_is_the_local_calendar_day() {
        let tones = [tone("2026-11-23T21:30:00Z", -70.0)];
        let utc = jiff::tz::TimeZone::UTC;
        let berlin = jiff::tz::TimeZone::fixed(jiff::tz::offset(1));
        // 22:15 and 23:15 UTC: the same UTC day, but 23:15 is already the
        // 24th in UTC+1, while the room tone was recorded on the 23rd.
        let take_at = at("2026-11-23T23:15:00Z");
        assert!(choose_room_tone(&tones, &recorded(), take_at, -70.0, &utc).is_ok());
        let why = choose_room_tone(&tones, &recorded(), take_at, -70.0, &berlin).unwrap_err();
        assert!(why.contains("on 2026-11-24"), "{why}");
        // Late evening in UTC-8 is the next UTC day but the same local day.
        let tones = [tone("2026-11-23T20:00:00Z", -70.0)];
        let la = jiff::tz::TimeZone::fixed(jiff::tz::offset(-8));
        let take_at = at("2026-11-24T06:00:00Z");
        assert!(choose_room_tone(&tones, &recorded(), take_at, -70.0, &utc).is_err());
        assert!(choose_room_tone(&tones, &recorded(), take_at, -70.0, &la).is_ok());
    }

    #[test]
    fn newest_matching_room_tone_wins() {
        let tones = [
            tone("2026-11-23T08:00:00Z", -70.0),
            tone("2026-11-23T11:00:00Z", -69.0),
            tone("2026-11-23T09:00:00Z", -68.0),
            // Newest, but recorded at another gain.
            tone("2026-11-23T12:00:00Z", -50.0),
        ];
        let chosen =
            choose_room_tone(&tones, &recorded(), at("2026-11-23T13:00:00Z"), -70.0, &UTC).unwrap();
        assert_eq!(chosen.recorded_at, at("2026-11-23T11:00:00Z"));
    }

    #[test]
    fn click_lands_on_same_sample_after_clean() {
        let Some(ffmpeg) = Ffmpeg::find() else {
            eprintln!("skipped: ffmpeg not found");
            return;
        };
        let processor = Cleaner::new(ffmpeg).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("take.wav");
        let mut take = noise_at(3.0, -60.0);
        let click = 100_000;
        take[click] = 1 << 22;
        write_take_wav(&src, &take, 48_000).unwrap();
        let room = tone("2026-11-23T09:00:00Z", -60.0);
        for room in [None, Some(&room)] {
            let out = tmp.path().join("clean.wav");
            let info = processor.clean_take(&src, room, &out).unwrap();
            assert_eq!(info.noise_floor_db.is_some(), room.is_some());
            let (cleaned, rate) = read_float_wav(&out).unwrap();
            assert_eq!(rate, 48_000);
            assert_eq!(
                cleaned.len(),
                take.len(),
                "same length, room tone: {room:?}"
            );
            let peak = peak_index(&cleaned).unwrap();
            assert!(
                peak.abs_diff(click) <= 1,
                "click at {click} came out at {peak} (room tone: {room:?}, delay {})",
                processor.nr_delay
            );
        }
        // Noise reduction lowered the noise.
        let (cleaned, _) = read_float_wav(&tmp.path().join("clean.wav")).unwrap();
        let rms = |x: &[f64]| (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt();
        let before = rms(&noise_at(1.0, -60.0)
            .iter()
            .map(|&s| f64::from(s) / f64::from(1 << 23))
            .collect::<Vec<_>>());
        let after = rms(&cleaned[..48_000]);
        assert!(after < before * 0.7, "{before} → {after}");
    }
}
