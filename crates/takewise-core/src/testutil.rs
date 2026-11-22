//! Synthetic audio for tests: low noise with 440 Hz bursts standing in for speech.

use crate::takes::FULL_SCALE;

pub const RATE: u32 = 48_000;

/// Uniform noise at about -65 dBFS RMS, deterministic.
pub fn noise(seconds: f64) -> Vec<i32> {
    let n = samples(seconds);
    let mut state: u32 = 0x1234_5678;
    let amplitude = f64::from(FULL_SCALE) * 0.001;
    (0..n)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let unit = f64::from(state >> 8) / f64::from(1u32 << 24) * 2.0 - 1.0;
            round(unit * amplitude)
        })
        .collect()
}

/// Replaces `from..to` seconds of `take` with a 440 Hz tone at half scale.
pub fn burst(take: &mut [i32], from: f64, to: f64) {
    let (a, b) = (samples(from), samples(to).min(take.len()));
    for (i, sample) in take[a..b].iter_mut().enumerate() {
        let t = i as f64 / f64::from(RATE);
        *sample = round((t * 440.0 * std::f64::consts::TAU).sin() * 0.5 * f64::from(FULL_SCALE));
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn samples(seconds: f64) -> usize {
    (seconds * f64::from(RATE)).round() as usize
}

#[allow(clippy::cast_possible_truncation)]
fn round(x: f64) -> i32 {
    x.round() as i32
}
