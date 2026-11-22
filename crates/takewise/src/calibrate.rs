//! The loopback calibration: a film of short beeps at known film times plays
//! through the default output while the configured input records, with the
//! same sample mapping as a take (and `latency_offset_ms` = 0). Where the
//! recorded beeps land on the film gives the offset to store.
//!
//! The beep film is made with `ffmpeg` by the command line; the measurement
//! runs inside Takewise.app (`calibrate-run`), which owns the microphone
//! permission, and hands its result back as JSON.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;

use serde::{Deserialize, Serialize};
use takewise_core::takes::FULL_SCALE;
use takewise_core::{
    AvailableInput, Project, RECORD_RATE, TimebaseHistory, check_input, sample_film_time,
};
use takewise_media::{
    Capture, CaptureBuffer, FilmPlayer, default_output, host_time_now_s, input_devices,
    input_latency_s, pump_run_loop,
};

/// Film times of the beeps; the first is 0.1 s, to test the film's start.
pub const BEEPS: [f64; 10] = [0.1, 1.3, 2.6, 3.9, 5.2, 6.5, 7.8, 9.1, 10.4, 11.7];
const FILM_LEN_S: f64 = 12.5;
const BEEP_LEN_S: f64 = 0.02;
const BEEP_HZ: f64 = 1000.0;
/// The stretches of the film played, as (from, seconds): seeks backwards
/// and forwards, ending on the film's start again.
const SEGMENTS: [(f64, f64); 5] = [(0.0, 3.0), (5.0, 3.0), (1.0, 2.0), (9.0, 3.0), (0.0, 1.5)];
/// Onsets farther than this from every expected beep are ignored.
pub const MAX_ERROR_S: f64 = 0.1;
/// Fewer placed beeps than this is not a measurement.
pub const MIN_BEEPS: usize = 6;
/// Onsets this close to an earlier one are the same beep.
const REFRACTORY_S: f64 = 0.3;
/// The lowest noise floor assumed, so digital silence still has a threshold.
const MIN_FLOOR_DBFS: f64 = -80.0;
/// An onset rises this far above the noise floor.
const ONSET_ABOVE_FLOOR_DB: f64 = 20.0;

/// What a calibration run measured.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Measurement {
    pub input: String,
    pub output: String,
    pub input_latency_ms: f64,
    /// Beep plays expected across all segments.
    pub expected: usize,
    pub placed: Vec<Placement>,
    pub mean_ms: Option<f64>,
    pub sd_ms: Option<f64>,
}

/// One recorded beep: the film time it should be heard at and the error of
/// where it landed (positive: late).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub film_s: f64,
    pub error_ms: f64,
}

/// One play of a beep: the film time and the host time it is heard at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Expected {
    pub host_s: f64,
    pub film_s: f64,
}

/// A detected onset: the host time of its sample (latency removed) and its
/// film time through the take mapping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Onset {
    pub host_s: f64,
    pub film_s: f64,
}

/// Writes the beep film (and nothing else) into `dir` with `ffmpeg`.
pub fn make_beep_film(dir: &Path) -> Result<PathBuf, String> {
    let expr: Vec<String> = BEEPS
        .iter()
        .map(|b| {
            format!(
                "between(t,{b},{e})*sin(2*PI*{BEEP_HZ}*(t-{b}))*0.7",
                e = b + BEEP_LEN_S
            )
        })
        .collect();
    let audio = format!("aevalsrc='{}':s=48000:d={FILM_LEN_S}", expr.join("+"));
    let video = format!("testsrc2=s=640x360:r=30:d={FILM_LEN_S}");
    // LPCM audio, so no codec priming shifts the beeps.
    let film = dir.join("beeps.mov");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg(&video)
        .args(["-f", "lavfi", "-i"])
        .arg(&audio)
        .args([
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "pcm_s16le",
            "-shortest",
        ])
        .arg(&film)
        .status()
        .map_err(|err| format!("cannot run ffmpeg (install ffmpeg): {err}"))?;
    if !status.success() {
        return Err(format!("ffmpeg could not make the beep film ({status})"));
    }
    Ok(film)
}

/// Sample indices where a beep starts: the first sample rising above the
/// noise floor + 20 dB, then nothing for 0.3 s. The floor is the 20th
/// percentile of 10 ms peaks, at least −80 dBFS.
pub fn detect_onsets(samples: &[i32], sample_rate: u32) -> Vec<usize> {
    let window = (sample_rate / 100).max(1) as usize;
    let mut peaks: Vec<u32> = samples
        .chunks(window)
        .map(|chunk| chunk.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0))
        .collect();
    if peaks.is_empty() {
        return Vec::new();
    }
    peaks.sort_unstable();
    let min_floor = f64::from(FULL_SCALE) * 10f64.powf(MIN_FLOOR_DBFS / 20.0);
    let floor = f64::from(peaks[peaks.len() / 5]).max(min_floor);
    let threshold = floor * 10f64.powf(ONSET_ABOVE_FLOOR_DB / 20.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let refractory = (REFRACTORY_S * f64::from(sample_rate)) as usize;
    let mut onsets = Vec::new();
    let mut i = 0;
    while i < samples.len() {
        if f64::from(samples[i].unsigned_abs()) >= threshold {
            onsets.push(i);
            i += refractory.max(1);
        } else {
            i += 1;
        }
    }
    onsets
}

/// Matches each onset to the beep play nearest in host time and keeps it
/// when it lands within 100 ms of that beep's film time; each play is
/// matched once.
pub fn place_onsets(onsets: &[Onset], expected: &[Expected]) -> Vec<Placement> {
    let mut used = vec![false; expected.len()];
    let mut placed = Vec::new();
    for onset in onsets {
        let nearest = expected
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                (a.host_s - onset.host_s)
                    .abs()
                    .total_cmp(&(b.host_s - onset.host_s).abs())
            })
            .map(|(idx, _)| idx);
        let Some(idx) = nearest else {
            continue;
        };
        let error = onset.film_s - expected[idx].film_s;
        if used[idx] || error.abs() > MAX_ERROR_S {
            continue;
        }
        used[idx] = true;
        placed.push(Placement {
            film_s: expected[idx].film_s,
            error_ms: error * 1000.0,
        });
    }
    placed
}

/// Mean and (population) standard deviation.
pub fn mean_sd(values: &[f64]) -> Option<(f64, f64)> {
    if values.is_empty() {
        return None;
    }
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;
    Some((mean, var.sqrt()))
}

/// How far one beep may land from the mean error for the mean to be stored
/// as the offset: the design's ±20 ms.
pub const MAX_DEVIATION_MS: f64 = 20.0;

/// The beeps that landed more than [`MAX_DEVIATION_MS`] from `mean_ms`: a
/// placement this inconsistent is no basis for one fixed offset.
pub fn outliers(placed: &[Placement], mean_ms: f64) -> Vec<Placement> {
    placed
        .iter()
        .filter(|p| (p.error_ms - mean_ms).abs() > MAX_DEVIATION_MS)
        .copied()
        .collect()
}

/// The `latency_offset_ms` that cancels a mean error.
#[allow(clippy::cast_possible_truncation)]
pub fn offset_for(mean_ms: f64) -> i32 {
    (-mean_ms).round() as i32
}

/// The onsets in the captured buffers, placed with the take mapping.
fn onsets_of(buffers: &[CaptureBuffer], history: &TimebaseHistory, latency_s: f64) -> Vec<Onset> {
    let samples: Vec<i32> = buffers
        .iter()
        .flat_map(|b| b.samples.iter().copied())
        .collect();
    let mut onsets = Vec::new();
    let mut index = detect_onsets(&samples, RECORD_RATE).into_iter().peekable();
    let mut first = 0;
    for buffer in buffers {
        let end = first + buffer.samples.len();
        while let Some(&at) = index.peek().filter(|&&at| at < end) {
            index.next();
            let offset = at - first;
            let host_s = buffer.host_time_s + offset as f64 / f64::from(RECORD_RATE) - latency_s;
            if let Some(film_s) = sample_film_time(
                history,
                buffer.host_time_s,
                offset,
                RECORD_RATE,
                latency_s,
                0.0,
            ) {
                onsets.push(Onset { host_s, film_s });
            }
        }
        first = end;
    }
    onsets
}

/// Plays the beep film and records the project's input. Runs on the main
/// thread inside Takewise.app.
pub fn measure(
    project: &Project,
    film: &Path,
    output: Option<&str>,
) -> Result<Measurement, String> {
    let config = &project.config.input;
    let device = config
        .device
        .clone()
        .ok_or("No input device chosen; set [input] device in project.toml.")?;
    let available: Vec<AvailableInput> = input_devices()
        .map_err(|err| err.to_string())?
        .into_iter()
        .map(|d| AvailableInput {
            name: d.name,
            sample_rate: d.sample_rate,
        })
        .collect();
    check_input(Some(&device), &available)?;
    let latency_s = input_latency_s(&device).map_err(|err| err.to_string())?;
    let player = FilmPlayer::open(film).map_err(|err| err.to_string())?;
    let output = match output {
        Some(name) => {
            player
                .set_output_device(name)
                .map_err(|err| err.to_string())?;
            name.to_owned()
        }
        None => default_output().map_or_else(|_| "unknown".to_owned(), |d| d.name),
    };
    let loading_until = host_time_now_s() + 10.0;
    while !player.is_ready() {
        if host_time_now_s() > loading_until {
            return Err("the beep film did not load".to_owned());
        }
        pump_run_loop(0.05);
    }
    player.set_gain_db(0.0);
    // Wake the output device, as before the first take.
    player.set_muted(true);
    player
        .play_at(0.0, host_time_now_s())
        .map_err(|err| err.to_string())?;
    pump_run_loop(0.3);
    player.pause();
    player.set_muted(false);
    player.seek(0.0);
    pump_run_loop(0.2);

    let (tx, rx) = mpsc::channel();
    let capture = Capture::start(&device, config.channel, tx).map_err(|err| err.to_string())?;
    let mut buffers: Vec<CaptureBuffer> = Vec::new();
    let mut history = TimebaseHistory::new();
    history.record(host_time_now_s() - 1.0, player.timebase());
    let mut expected = Vec::new();
    for (from, secs) in SEGMENTS {
        let start = host_time_now_s() + 0.3;
        let tb = player.play_at(from, start).map_err(|err| err.to_string())?;
        history.record(host_time_now_s(), tb);
        expected.extend(
            BEEPS
                .iter()
                .filter(|&&beep| beep >= from && beep < from + secs)
                .map(|&beep| Expected {
                    host_s: start + (beep - from),
                    film_s: beep,
                }),
        );
        let until = start + secs;
        while host_time_now_s() < until {
            pump_run_loop(0.02);
            buffers.extend(rx.try_iter());
        }
        player.pause();
        history.record(host_time_now_s(), player.timebase());
        let settle = host_time_now_s() + 0.4;
        while host_time_now_s() < settle {
            pump_run_loop(0.02);
            buffers.extend(rx.try_iter());
        }
        if let Some(err) = capture.last_error() {
            return Err(format!("capture failed: {err}"));
        }
    }
    capture.stop();
    buffers.extend(rx.try_iter());

    let onsets = onsets_of(&buffers, &history, latency_s);
    let placed = place_onsets(&onsets, &expected);
    let errors: Vec<f64> = placed.iter().map(|p| p.error_ms).collect();
    let stats = mean_sd(&errors);
    Ok(Measurement {
        input: device,
        output,
        input_latency_ms: latency_s * 1000.0,
        expected: expected.len(),
        placed,
        mean_ms: stats.map(|(mean, _)| mean),
        sd_ms: stats.map(|(_, sd)| sd),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// Low noise (about −60 dBFS) with 1 kHz beeps starting at `beeps`.
    fn recording(seconds: f64, beeps: &[f64]) -> Vec<i32> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = (seconds * f64::from(RATE)) as usize;
        let mut state: u32 = 0x9e37_79b9;
        let noise = f64::from(FULL_SCALE) * 0.001;
        let mut out: Vec<i32> = (0..n)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let unit = f64::from(state >> 8) / f64::from(1u32 << 24) * 2.0 - 1.0;
                #[allow(clippy::cast_possible_truncation)]
                let sample = (unit * noise).round() as i32;
                sample
            })
            .collect();
        for &beep in beeps {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let start = (beep * f64::from(RATE)).round() as usize;
            for i in 0..960 {
                let t = i as f64 / f64::from(RATE);
                #[allow(clippy::cast_possible_truncation)]
                let tone =
                    ((t * BEEP_HZ * std::f64::consts::TAU).sin() * 0.3 * f64::from(FULL_SCALE))
                        .round() as i32;
                out[start + i] += tone;
            }
        }
        out
    }

    #[test]
    fn detects_beep_onsets() {
        let beeps = [0.5, 1.7, 2.4];
        let onsets = detect_onsets(&recording(3.0, &beeps), RATE);
        assert_eq!(onsets.len(), 3, "{onsets:?}");
        for (onset, beep) in onsets.iter().zip(beeps) {
            let at = *onset as f64 / f64::from(RATE);
            // Within a quarter period of the 1 kHz tone.
            assert!((at - beep).abs() < 0.000_25, "{at} vs {beep}");
        }
    }

    #[test]
    fn detects_nothing_in_silence() {
        assert_eq!(detect_onsets(&vec![0; 48_000], RATE), Vec::<usize>::new());
        assert_eq!(detect_onsets(&[], RATE), Vec::<usize>::new());
    }

    #[test]
    fn ignores_far_onsets() {
        // Two plays of the beep at film 1.3: first heard at host 11.3, then
        // after a seek at host 21.3.
        let expected = [
            Expected {
                host_s: 10.1,
                film_s: 0.1,
            },
            Expected {
                host_s: 11.3,
                film_s: 1.3,
            },
            Expected {
                host_s: 21.3,
                film_s: 1.3,
            },
            Expected {
                host_s: 22.6,
                film_s: 2.6,
            },
        ];
        let onsets = [
            Onset {
                host_s: 10.1315,
                film_s: 0.1315,
            }, // 31.5 ms late
            Onset {
                host_s: 11.25,
                film_s: 1.25,
            }, // 50 ms early
            Onset {
                host_s: 11.29,
                film_s: 1.29,
            }, // same play again: ignored
            Onset {
                host_s: 16.0,
                film_s: 4.0,
            }, // a knock between beeps
            Onset {
                host_s: 21.33,
                film_s: 1.33,
            }, // the second play
            Onset {
                host_s: 22.9,
                film_s: 2.9,
            }, // 300 ms off: ignored
        ];
        let placed = place_onsets(&onsets, &expected);
        let got: Vec<(f64, i64)> = placed
            .iter()
            .map(|p| {
                #[allow(clippy::cast_possible_truncation)]
                let tenths = (p.error_ms * 10.0).round() as i64;
                (p.film_s, tenths)
            })
            .collect();
        assert_eq!(got, vec![(0.1, 315), (1.3, -500), (1.3, 300)]);
    }

    #[test]
    fn finds_beeps_far_from_the_mean() {
        let placed = |errors: &[f64]| -> Vec<Placement> {
            errors
                .iter()
                .map(|&error_ms| Placement {
                    film_s: 1.0,
                    error_ms,
                })
                .collect()
        };
        let steady = placed(&[30.0, 32.0, 31.0, 29.0, 33.0, 31.0]);
        assert_eq!(outliers(&steady, 31.0), Vec::<Placement>::new());
        let scattered = placed(&[30.0, 32.0, 31.0, 29.0, 55.0, 31.0]);
        let (mean, _) = mean_sd(&[30.0, 32.0, 31.0, 29.0, 55.0, 31.0]).unwrap();
        let far = outliers(&scattered, mean);
        assert_eq!(far.len(), 1);
        assert!((far[0].error_ms - 55.0).abs() < 1e-9);
    }

    #[test]
    fn offset_cancels_the_mean() {
        let (mean, sd) = mean_sd(&[31.0, 32.0, 31.5]).unwrap();
        assert!((mean - 31.5).abs() < 1e-9);
        assert!(sd < 0.5);
        assert_eq!(offset_for(mean), -32);
        assert_eq!(offset_for(-19.4), 19);
        assert_eq!(mean_sd(&[]), None);
    }
}
