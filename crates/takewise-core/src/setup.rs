//! The sound analysis behind the setup check and the per-take noise rule:
//! how loud the narrator is, whether the voice pops or booms, how quiet the
//! room is and whether it hums (see `docs/design.md`, "Setup check").
//!
//! Everything here is a hint for the narrator; only the noise rule in
//! [`crate::check`] changes a verdict, and only to `listen`.

use serde::Serialize;

use crate::speech::SILENCE_DBFS;
use crate::takes::FULL_SCALE;

/// Where the setup check aims the narrator's peaks.
pub const TARGET_PEAK_DBFS: f64 = -10.0;
/// The voice step accepts peaks in this range and moves on.
pub const PEAK_RANGE_DBFS: (f64, f64) = (-14.0, -6.0);
/// A floor at or below this is digital silence: a gated interface, whose
/// noise between words cannot be measured.
pub const GATED_FLOOR_DBFS: f64 = -120.0;
/// Speech this far or more above the room's floor is good.
pub const GOOD_SNR_DB: f64 = 40.0;
/// Speech less than this above the floor is noisy; a take's check then
/// says `listen` (`background-noise`).
pub const NOISY_SNR_DB: f64 = 30.0;
/// A plosive frame (within the speech) has this much more energy below
/// 80 Hz than in 200 Hz–4 kHz…
const PLOSIVE_EXCESS_DB: f64 = 6.0;
/// …and its energy below 80 Hz is at most this far below the loudest frame,
/// so rumble in quiet frames between words is not a pop. Tuned on Niklas's
/// Kindred narration: of its eight lines only the pop in "I put" counts.
const PLOSIVE_BELOW_LOUDEST_DB: f64 = 15.0;
/// The boom hint appears when the speech has this much more energy below
/// 200 Hz than in 200 Hz–4 kHz. A deep voice at a normal distance stays
/// well below it (Niklas's Kindred lines measure −3.7 to −0.3 dB); the
/// proximity effect of a mic a few centimetres away lifts the lows above it.
pub const BOOM_RATIO_DB: f64 = 3.0;
/// A frame within this much of the loudest frame (and above
/// [`VOICED_MIN_DBFS`]) is part of the speech of a voice recording.
const VOICED_BELOW_LOUDEST_DB: f64 = 30.0;
const VOICED_MIN_DBFS: f64 = -60.0;
/// A hum peak stands this far above the median of its neighbours.
const HUM_PEAK_DB: f64 = 10.0;
/// Neighbours of a hum peak are this far from it at most…
const HUM_NEIGHBOURS_HZ: f64 = 10.0;
/// …and at least this far, so the peak's own spread is not counted.
const HUM_PEAK_WIDTH_HZ: f64 = 2.0;
/// Frequencies are sampled in steps of this size around each harmonic.
const HUM_STEP_HZ: f64 = 0.25;
const CLIP_DBFS: f64 = -0.1;
const FRAME_S: f64 = 0.01;

/// What the narrator's reading of the test sentence sounded like.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VoiceReport {
    pub peak_dbfs: f64,
    /// RMS over the speech; [`SILENCE_DBFS`] when nothing was heard.
    pub speech_rms_dbfs: f64,
    /// Samples at or above −0.1 dBFS.
    pub clipped: u64,
    /// The gain change on the interface that puts the peaks at −10 dBFS.
    pub gain_change_db: f64,
    /// Bursts of energy below 80 Hz within the speech.
    pub plosives: u32,
    /// Energy below 200 Hz against 200 Hz–4 kHz over the speech, in dB.
    pub boom_ratio_db: f64,
    pub hints: Vec<Hint>,
}

impl VoiceReport {
    /// Whether the peaks lie within −14..−6 dBFS without clipping, so the
    /// voice step can move on.
    pub fn level_fits(&self) -> bool {
        self.clipped == 0 && (PEAK_RANGE_DBFS.0..=PEAK_RANGE_DBFS.1).contains(&self.peak_dbfs)
    }
}

/// A suggestion for the narrator; never a verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hint {
    pub code: &'static str,
    pub message: String,
}

/// How quiet the room was during five seconds of silence.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RoomReport {
    /// The 20th percentile of 10 ms frame levels, as a take's pre-roll
    /// floor is measured.
    pub floor_dbfs: f64,
    /// The input is digital silence between words (a gated interface).
    pub gated: bool,
    /// Speech level above the floor; `None` when gated.
    pub snr_db: Option<f64>,
    pub rating: RoomRating,
    pub hum: Option<HumReport>,
    pub tips: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RoomRating {
    /// 40 dB or more between voice and room, or a gated input.
    Good,
    /// 30–40 dB.
    Acceptable,
    /// Below 30 dB.
    Noisy,
}

/// Mains hum: narrow peaks at 50 or 60 Hz and its harmonics.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HumReport {
    /// 50 or 60.
    pub fundamental_hz: u32,
    /// The harmonics that stand out, such as `[50.0, 100.0, 150.0]`.
    pub harmonics: Vec<f64>,
}

/// Analyzes a recording of the narrator reading the test sentence.
pub fn voice_report(samples: &[i32], rate: u32) -> VoiceReport {
    let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    let peak_dbfs = amplitude_dbfs(f64::from(peak));
    let clip_level = f64::from(FULL_SCALE) * 10f64.powf(CLIP_DBFS / 20.0);
    let clipped = samples
        .iter()
        .filter(|s| f64::from(s.unsigned_abs()) >= clip_level)
        .count() as u64;
    let frame = frame_len(rate);
    let x: Vec<f64> = samples
        .iter()
        .map(|&s| f64::from(s) / f64::from(FULL_SCALE))
        .collect();
    let powers = frame_powers(&x, frame);
    let voiced = voiced_frames(&powers);
    let speech_rms_dbfs = power_dbfs(mean_of(&powers, &voiced));

    let fs = f64::from(rate);
    let below_80 = filtered(&x, &[Biquad::lowpass(80.0, fs), Biquad::lowpass(80.0, fs)]);
    let below_200 = filtered(
        &x,
        &[Biquad::lowpass(200.0, fs), Biquad::lowpass(200.0, fs)],
    );
    let band = filtered(
        &x,
        &[
            Biquad::highpass(200.0, fs),
            Biquad::highpass(200.0, fs),
            Biquad::lowpass(4000.0, fs),
            Biquad::lowpass(4000.0, fs),
        ],
    );
    let (low80, low200, mid) = (
        frame_powers(&below_80, frame),
        frame_powers(&below_200, frame),
        frame_powers(&band, frame),
    );

    let loudest = power_dbfs(powers.iter().copied().fold(0.0, f64::max));
    let mut plosives = 0_u32;
    let mut in_burst = false;
    for &i in &voiced {
        let pops = ratio_db(low80[i], mid[i]) > PLOSIVE_EXCESS_DB
            && power_dbfs(low80[i]) >= loudest - PLOSIVE_BELOW_LOUDEST_DB;
        if pops && !in_burst {
            plosives += 1;
        }
        // A burst spans consecutive frames; a gap in the speech ends it.
        in_burst = pops && voiced.contains(&(i + 1));
    }
    let boom_ratio_db = if voiced.is_empty() {
        0.0
    } else {
        ratio_db(mean_of(&low200, &voiced), mean_of(&mid, &voiced))
    };

    let mut hints = Vec::new();
    if voiced.is_empty() {
        hints.push(Hint {
            code: "no-voice",
            message:
                "No voice was heard. Check the input and channel, then read the sentence again."
                    .to_owned(),
        });
    }
    if clipped > 0 {
        hints.push(Hint {
            code: "clipping",
            message: format!(
                "{clipped} samples clipped. Turn the gain on your interface down and read again."
            ),
        });
    }
    if plosives > 0 {
        hints.push(Hint {
            code: "plosives",
            message: format!(
                "{plosives} popped consonant(s): turn the mic slightly off-axis or add a pop filter."
            ),
        });
    }
    if boom_ratio_db > BOOM_RATIO_DB {
        hints.push(Hint {
            code: "boom",
            message: "If you sound boomy, move a little further from the mic.".to_owned(),
        });
    }
    VoiceReport {
        peak_dbfs,
        speech_rms_dbfs,
        clipped,
        gain_change_db: if peak == 0 {
            0.0
        } else {
            TARGET_PEAK_DBFS - peak_dbfs
        },
        plosives,
        boom_ratio_db,
        hints,
    }
}

/// Analyzes five seconds of room silence against the speech level of the
/// voice step.
pub fn room_report(samples: &[i32], rate: u32, speech_rms_dbfs: f64) -> RoomReport {
    let x: Vec<f64> = samples
        .iter()
        .map(|&s| f64::from(s) / f64::from(FULL_SCALE))
        .collect();
    let powers = frame_powers(&x, frame_len(rate));
    let floor_dbfs = floor_of(&powers);
    let gated = is_gated(floor_dbfs);
    let snr_db = (!gated).then_some(speech_rms_dbfs - floor_dbfs);
    let rating = match snr_db {
        Some(snr) if snr < NOISY_SNR_DB => RoomRating::Noisy,
        Some(snr) if snr < GOOD_SNR_DB => RoomRating::Acceptable,
        _ => RoomRating::Good,
    };
    let hum = if gated { None } else { find_hum(&x, rate) };
    let mut tips = Vec::new();
    if gated {
        tips.push(
            "Your input is silent between words (gated), so the room's noise cannot be measured; noise reduction is skipped."
                .to_owned(),
        );
    }
    if rating != RoomRating::Good {
        tips.push(
            "Turn off fans and air conditioning, close windows and doors, and move the computer away from the mic."
                .to_owned(),
        );
        tips.push(
            "Soft furnishings help: record in a smaller room with curtains, a sofa or a rug."
                .to_owned(),
        );
    }
    if let Some(hum) = &hum {
        tips.push(format!(
            "Mains hum at {} Hz: check cables, plug the interface and computer into the same power strip, and keep the mic cable away from power cables.",
            hum.fundamental_hz
        ));
    }
    RoomReport {
        floor_dbfs,
        gated,
        snr_db,
        rating,
        hum,
        tips,
    }
}

/// Whether a noise floor is digital silence (a gated interface).
pub fn is_gated(floor_dbfs: f64) -> bool {
    floor_dbfs <= GATED_FLOOR_DBFS
}

/// The level of `power` (a mean square relative to full scale) in dBFS.
fn power_dbfs(power: f64) -> f64 {
    if power <= 0.0 {
        return SILENCE_DBFS;
    }
    (10.0 * power.log10()).max(SILENCE_DBFS)
}

fn amplitude_dbfs(amplitude: f64) -> f64 {
    if amplitude <= 0.0 {
        return SILENCE_DBFS;
    }
    (20.0 * (amplitude / f64::from(FULL_SCALE)).log10()).max(SILENCE_DBFS)
}

fn ratio_db(a: f64, b: f64) -> f64 {
    power_dbfs(a) - power_dbfs(b)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn frame_len(rate: u32) -> usize {
    ((f64::from(rate) * FRAME_S).round() as usize).max(1)
}

/// The mean square of each frame.
fn frame_powers(x: &[f64], frame: usize) -> Vec<f64> {
    x.chunks(frame)
        .map(|c| c.iter().map(|v| v * v).sum::<f64>() / c.len() as f64)
        .collect()
}

/// The frames of a voice recording that hold speech: within 30 dB of the
/// loudest and above −60 dBFS.
fn voiced_frames(powers: &[f64]) -> Vec<usize> {
    let loudest = powers.iter().copied().fold(0.0, f64::max);
    if loudest <= 0.0 {
        return Vec::new();
    }
    let min = (power_dbfs(loudest) - VOICED_BELOW_LOUDEST_DB).max(VOICED_MIN_DBFS);
    (0..powers.len())
        .filter(|&i| power_dbfs(powers[i]) > min)
        .collect()
}

fn mean_of(powers: &[f64], frames: &[usize]) -> f64 {
    if frames.is_empty() {
        return 0.0;
    }
    frames.iter().map(|&i| powers[i]).sum::<f64>() / frames.len() as f64
}

/// The 20th percentile of the frame levels, as [`crate::speech::analyze`]
/// measures a pre-roll's floor.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn floor_of(powers: &[f64]) -> f64 {
    let mut levels: Vec<f64> = powers.iter().map(|&p| power_dbfs(p)).collect();
    if levels.is_empty() {
        return SILENCE_DBFS;
    }
    levels.sort_by(f64::total_cmp);
    let idx = ((levels.len() - 1) as f64 * 0.2).round() as usize;
    levels[idx]
}

/// A second-order filter (RBJ's cookbook), Butterworth Q.
#[derive(Debug, Clone, Copy)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
}

impl Biquad {
    fn lowpass(f: f64, fs: f64) -> Self {
        let (cos, alpha) = Self::params(f, fs);
        Self::normalized(
            [
                f64::midpoint(1.0, -cos),
                1.0 - cos,
                f64::midpoint(1.0, -cos),
            ],
            [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        )
    }

    fn highpass(f: f64, fs: f64) -> Self {
        let (cos, alpha) = Self::params(f, fs);
        Self::normalized(
            [
                f64::midpoint(1.0, cos),
                -(1.0 + cos),
                f64::midpoint(1.0, cos),
            ],
            [1.0 + alpha, -2.0 * cos, 1.0 - alpha],
        )
    }

    fn params(f: f64, fs: f64) -> (f64, f64) {
        let w = std::f64::consts::TAU * f / fs;
        (w.cos(), w.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2))
    }

    fn normalized(b: [f64; 3], a: [f64; 3]) -> Self {
        Self {
            b: [b[0] / a[0], b[1] / a[0], b[2] / a[0]],
            a: [a[1] / a[0], a[2] / a[0]],
        }
    }
}

/// `x` through the filters in turn.
fn filtered(x: &[f64], filters: &[Biquad]) -> Vec<f64> {
    let mut out = x.to_vec();
    for f in filters {
        let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
        for v in &mut out {
            let x0 = *v;
            let y0 = f.b[0] * x0 + f.b[1] * x1 + f.b[2] * x2 - f.a[0] * y1 - f.a[1] * y2;
            (x2, x1, y2, y1) = (x1, x0, y1, y0);
            *v = y0;
        }
    }
    out
}

/// Mains hum: the mains frequency (50 or 60 Hz) whose fundamental, second
/// and third harmonic most often stand [`HUM_PEAK_DB`] above the median of
/// their neighbours; hum needs at least two of them.
fn find_hum(x: &[f64], rate: u32) -> Option<HumReport> {
    // Only frequencies up to 190 Hz matter: average blocks of 16 samples
    // (a crude low-pass) and look at the result.
    let decimate = 16_usize;
    let y: Vec<f64> = x
        .chunks(decimate)
        .map(|c| c.iter().sum::<f64>() / c.len() as f64)
        .collect();
    if y.len() < 64 {
        return None;
    }
    let fs = f64::from(rate) / decimate as f64;
    let window: Vec<f64> = (0..y.len())
        .map(|i| {
            let phase = std::f64::consts::TAU * i as f64 / (y.len() - 1) as f64;
            0.5 - 0.5 * phase.cos()
        })
        .zip(&y)
        .map(|(w, v)| w * v)
        .collect();
    [50_u32, 60]
        .into_iter()
        .map(|mains| {
            let harmonics: Vec<f64> = (1..=3)
                .map(|n| f64::from(mains * n))
                .filter(|&f| stands_out(&window, fs, f))
                .collect();
            HumReport {
                fundamental_hz: mains,
                harmonics,
            }
        })
        .filter(|hum| hum.harmonics.len() >= 2)
        .max_by_key(|hum| hum.harmonics.len())
}

/// Whether the spectrum's peak within ±0.5 Hz of `f` lies [`HUM_PEAK_DB`]
/// above the median of the frequencies 2–10 Hz away.
#[allow(clippy::cast_possible_truncation)]
fn stands_out(x: &[f64], fs: f64, f: f64) -> bool {
    let steps = (HUM_NEIGHBOURS_HZ / HUM_STEP_HZ).round() as i32;
    let mut peak = 0.0_f64;
    let mut neighbours = Vec::new();
    for k in -steps..=steps {
        let offset = f64::from(k) * HUM_STEP_HZ;
        let power = goertzel(x, fs, f + offset);
        if offset.abs() <= 0.5 {
            peak = peak.max(power);
        } else if offset.abs() >= HUM_PEAK_WIDTH_HZ {
            neighbours.push(power);
        }
    }
    neighbours.sort_by(f64::total_cmp);
    let median = neighbours.get(neighbours.len() / 2).copied().unwrap_or(0.0);
    if median <= 0.0 {
        return peak > 0.0;
    }
    10.0 * (peak / median).log10() >= HUM_PEAK_DB
}

/// The power of `x` at frequency `f`.
fn goertzel(x: &[f64], fs: f64, f: f64) -> f64 {
    let w = std::f64::consts::TAU * f / fs;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for &v in x {
        let s0 = v + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - coeff * s1 * s2
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::testutil::{RATE, noise_at, samples};

    /// A voice-like harmonic series at `f0`, flat up to 600 Hz and falling
    /// 6 dB per octave above (the shape of a long-term speech spectrum),
    /// with `low_boost_db` added below 200 Hz (the proximity effect), at
    /// `rms_dbfs`, for `seconds`.
    pub(crate) fn voice(f0: f64, low_boost_db: f64, rms_dbfs: f64, seconds: f64) -> Vec<f64> {
        let n = samples(seconds);
        let fs = f64::from(RATE);
        let partials: Vec<(f64, f64)> = (1..=200)
            .map(|k| f0 * f64::from(k))
            .take_while(|&f| f < 4000.0)
            .map(|f| {
                let mut amp = if f <= 600.0 { 1.0 } else { 600.0 / f };
                if f < 200.0 {
                    amp *= 10f64.powf(low_boost_db / 20.0);
                }
                (f, amp)
            })
            .collect();
        let mut x: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                partials
                    .iter()
                    .enumerate()
                    .map(|(k, &(f, a))| a * (std::f64::consts::TAU * f * t + k as f64).sin())
                    .sum()
            })
            .collect();
        let rms = (x.iter().map(|v| v * v).sum::<f64>() / n as f64).sqrt();
        let gain = 10f64.powf(rms_dbfs / 20.0) / rms;
        for v in &mut x {
            *v *= gain;
        }
        x
    }

    pub(crate) fn to_samples(x: &[f64]) -> Vec<i32> {
        #[allow(clippy::cast_possible_truncation)]
        x.iter()
            .map(|v| (v * f64::from(FULL_SCALE)).round() as i32)
            .collect()
    }

    /// Adds a sine at `f` Hz and `rms_dbfs` to `x`.
    fn add_sine(x: &mut [f64], f: f64, rms_dbfs: f64) {
        let amp = 10f64.powf(rms_dbfs / 20.0) * std::f64::consts::SQRT_2;
        for (i, v) in x.iter_mut().enumerate() {
            *v += amp * (std::f64::consts::TAU * f * i as f64 / f64::from(RATE)).sin();
        }
    }

    fn noise_f64(seconds: f64, rms_dbfs: f64) -> Vec<f64> {
        noise_at(seconds, rms_dbfs)
            .into_iter()
            .map(|s| f64::from(s) / f64::from(FULL_SCALE))
            .collect()
    }

    #[test]
    fn gain_suggestion_targets_minus_ten() {
        // A voice peaking around −20 dBFS needs about +10 dB.
        let x = voice(120.0, 0.0, -36.0, 3.0);
        let report = voice_report(&to_samples(&x), RATE);
        assert!(
            (report.gain_change_db - (-10.0 - report.peak_dbfs)).abs() < 1e-9,
            "{report:?}"
        );
        assert!(report.gain_change_db > 5.0, "{report:?}");
        assert!(!report.level_fits());
        assert!((report.speech_rms_dbfs + 36.0).abs() < 0.5, "{report:?}");
        assert_eq!(report.clipped, 0);

        // Scaled to the suggestion, the peaks land at −10 dBFS and fit.
        let gain = 10f64.powf(report.gain_change_db / 20.0);
        let louder: Vec<f64> = x.iter().map(|v| v * gain).collect();
        let report = voice_report(&to_samples(&louder), RATE);
        assert!((report.peak_dbfs + 10.0).abs() < 0.1, "{report:?}");
        assert!(report.gain_change_db.abs() < 0.1);
        assert!(report.level_fits());

        // Clipping never fits.
        let clipped: Vec<f64> = x.iter().map(|v| (v * 1000.0).clamp(-1.0, 1.0)).collect();
        let report = voice_report(&to_samples(&clipped), RATE);
        assert!(report.clipped > 0);
        assert!(!report.level_fits());
        assert!(report.hints.iter().any(|h| h.code == "clipping"));
        assert!(report.gain_change_db < 0.0);
    }

    #[test]
    fn detects_plosive_bursts() {
        let mut x = voice(110.0, 0.0, -24.0, 4.0);
        let clean = voice_report(&to_samples(&x), RATE);
        assert_eq!(clean.plosives, 0, "{clean:?}");
        // Three 40 ms thumps at 45 Hz, Hann-shaped, far louder than the voice.
        for at in [0.8, 1.9, 3.1] {
            let (a, len) = (samples(at), samples(0.04));
            for i in 0..len {
                let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / len as f64).cos();
                let t = i as f64 / f64::from(RATE);
                x[a + i] += 0.4 * w * (std::f64::consts::TAU * 45.0 * t).sin();
            }
        }
        let report = voice_report(&to_samples(&x), RATE);
        assert_eq!(report.plosives, 3, "{report:?}");
        assert!(report.hints.iter().any(|h| h.code == "plosives"));
    }

    #[test]
    fn deep_voice_without_proximity_is_quiet() {
        // An 85 Hz voice, the low end of a male voice, at a normal distance.
        let report = voice_report(&to_samples(&voice(85.0, 0.0, -24.0, 3.0)), RATE);
        assert!(report.boom_ratio_db < BOOM_RATIO_DB, "{report:?}");
        assert!(!report.hints.iter().any(|h| h.code == "boom"), "{report:?}");
        assert_eq!(report.plosives, 0, "{report:?}");

        // Close to a directional mic, the proximity effect lifts the lows
        // by 15 dB: that booms.
        let report = voice_report(&to_samples(&voice(85.0, 15.0, -24.0, 3.0)), RATE);
        assert!(report.boom_ratio_db > BOOM_RATIO_DB, "{report:?}");
        assert!(report.hints.iter().any(|h| h.code == "boom"), "{report:?}");
    }

    #[test]
    fn detects_mains_hum_with_harmonics() {
        let mut x = noise_f64(5.0, -75.0);
        add_sine(&mut x, 50.0, -55.0);
        add_sine(&mut x, 100.0, -60.0);
        add_sine(&mut x, 150.0, -62.0);
        let report = room_report(&to_samples(&x), RATE, -20.0);
        let hum = report.hum.clone().expect("hum");
        assert_eq!(hum.fundamental_hz, 50);
        assert_eq!(hum.harmonics, [50.0, 100.0, 150.0]);
        assert!(
            report.tips.iter().any(|t| t.contains("50 Hz")),
            "{report:?}"
        );

        // 60 Hz mains with only its second and third harmonic.
        let mut x = noise_f64(5.0, -75.0);
        add_sine(&mut x, 120.0, -60.0);
        add_sine(&mut x, 180.0, -62.0);
        let hum = room_report(&to_samples(&x), RATE, -20.0).hum.expect("hum");
        assert_eq!(hum.fundamental_hz, 60);
        assert_eq!(hum.harmonics, [120.0, 180.0]);

        // A single tone is not hum.
        let mut x = noise_f64(5.0, -75.0);
        add_sine(&mut x, 50.0, -55.0);
        assert_eq!(room_report(&to_samples(&x), RATE, -20.0).hum, None);
    }

    #[test]
    fn broadband_noise_is_not_hum() {
        for level in [-80.0, -60.0, -40.0] {
            let report = room_report(&noise_at(5.0, level), RATE, -20.0);
            assert_eq!(report.hum, None, "{level}: {report:?}");
        }
    }

    #[test]
    fn room_rating_follows_speech_to_noise() {
        let rate = |floor: f64| {
            let report = room_report(&noise_at(5.0, floor), RATE, -20.0);
            (report.rating, report.snr_db.unwrap())
        };
        let (rating, snr) = rate(-70.0);
        assert_eq!(rating, RoomRating::Good);
        assert!((snr - 50.0).abs() < 1.5, "{snr}");
        assert_eq!(rate(-55.0).0, RoomRating::Acceptable);
        let (rating, _) = rate(-45.0);
        assert_eq!(rating, RoomRating::Noisy);
        let report = room_report(&noise_at(5.0, -45.0), RATE, -20.0);
        assert!(report.tips.iter().any(|t| t.contains("fans")), "{report:?}");
    }

    #[test]
    fn gated_input_is_not_noisy() {
        let report = room_report(&vec![0; samples(5.0)], RATE, -20.0);
        assert!(report.gated, "{report:?}");
        assert_eq!(report.snr_db, None);
        assert_eq!(report.rating, RoomRating::Good);
        assert_eq!(report.hum, None);
        assert!(report.tips[0].contains("gated"), "{report:?}");
    }
}
