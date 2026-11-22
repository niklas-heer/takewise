//! The energy speech detector and level measurements.

use serde::Serialize;

use crate::takes::FULL_SCALE;

/// The level reported for digital silence, about 24-bit PCM's dynamic range.
pub const SILENCE_DBFS: f64 = -144.0;
/// Frames must exceed the noise floor by this much to count as speech.
const SPEECH_ABOVE_FLOOR_DB: f64 = 12.0;
/// Frames must also exceed this absolute level, so that noise after digital
/// silence is not mistaken for speech.
const SPEECH_MIN_DBFS: f64 = -55.0;
const FRAMES_PER_SECOND: u32 = 100;
/// Gaps shorter than this between speech runs are bridged (in 10 ms frames).
const MERGE_GAP_FRAMES: usize = 30;
/// Runs shorter than this after merging are dropped (in 10 ms frames).
const MIN_RUN_FRAMES: usize = 8;
const CLIP_DBFS: f64 = -0.1;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Analysis {
    /// Speech start and end in seconds from the take's first sample.
    pub speech: Option<(f64, f64)>,
    pub peak_dbfs: f64,
    /// Samples at or above -0.1 dBFS.
    pub clipped_samples: u64,
    /// The 20th percentile of 10 ms frame levels in the pre-roll.
    pub noise_floor_dbfs: f64,
}

fn dbfs(amplitude: f64) -> f64 {
    if amplitude <= 0.0 {
        return SILENCE_DBFS;
    }
    (20.0 * (amplitude / f64::from(FULL_SCALE)).log10()).max(SILENCE_DBFS)
}

fn rms_dbfs(frame: &[i32]) -> f64 {
    if frame.is_empty() {
        return SILENCE_DBFS;
    }
    let sum: f64 = frame.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    dbfs((sum / frame.len() as f64).sqrt())
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn to_index(x: f64) -> usize {
    x.max(0.0).round() as usize
}

/// Finds speech by frame energy relative to the noise floor in the pre-roll,
/// and measures peak, clipping and noise floor.
pub fn analyze(samples: &[i32], sample_rate: u32, preroll_s: f64) -> Analysis {
    let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    let clip_level = f64::from(FULL_SCALE) * 10f64.powf(CLIP_DBFS / 20.0);
    let clipped = samples
        .iter()
        .filter(|s| f64::from(s.unsigned_abs()) >= clip_level)
        .count();

    let frame_len = usize::try_from((sample_rate / FRAMES_PER_SECOND).max(1)).unwrap_or(1);
    let levels: Vec<f64> = samples.chunks(frame_len).map(rms_dbfs).collect();
    let preroll_frames = to_index(preroll_s * f64::from(FRAMES_PER_SECOND));
    let floor_frames = match levels.get(..preroll_frames) {
        Some(frames) if !frames.is_empty() => frames,
        _ => &levels[..],
    };
    let noise_floor = percentile(floor_frames, 0.2).unwrap_or(SILENCE_DBFS);
    let threshold = (noise_floor + SPEECH_ABOVE_FLOOR_DB).max(SPEECH_MIN_DBFS);

    let speech = speech_runs(&levels, threshold).map(|(first, last)| {
        let rate = f64::from(sample_rate);
        let start = (first * frame_len) as f64 / rate;
        let end = (((last + 1) * frame_len).min(samples.len())) as f64 / rate;
        (start, end)
    });

    Analysis {
        speech,
        peak_dbfs: dbfs(f64::from(peak)),
        clipped_samples: clipped as u64,
        noise_floor_dbfs: noise_floor,
    }
}

fn percentile(values: &[f64], q: f64) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let idx = to_index((sorted.len().checked_sub(1)?) as f64 * q);
    sorted.get(idx).copied()
}

/// The first and last frame of speech, after bridging short gaps and
/// dropping short runs.
fn speech_runs(levels: &[f64], threshold: f64) -> Option<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (idx, _) in levels.iter().enumerate().filter(|(_, db)| **db > threshold) {
        match runs.last_mut() {
            Some((_, end)) if idx - *end - 1 < MERGE_GAP_FRAMES => *end = idx,
            _ => runs.push((idx, idx)),
        }
    }
    let mut kept = runs
        .into_iter()
        .filter(|(start, end)| end - start + 1 >= MIN_RUN_FRAMES);
    let first = kept.next()?;
    let last = kept.next_back().unwrap_or(first);
    Some((first.0, last.1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::takes::FULL_SCALE;
    use crate::testutil::{RATE, burst, noise};

    fn close(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() <= 0.020
    }

    #[test]
    fn detects_burst_bounds() {
        let mut take = noise(3.0);
        burst(&mut take, 1.0, 2.2);
        let analysis = analyze(&take, RATE, 0.5);
        let (start, end) = analysis.speech.expect("speech found");
        assert!(close(start, 1.0), "start {start}");
        assert!(close(end, 2.2), "end {end}");
        assert!(
            analysis.noise_floor_dbfs < -60.0 && analysis.noise_floor_dbfs > -70.0,
            "{analysis:?}"
        );
        assert!((analysis.peak_dbfs + 6.0).abs() < 0.1, "{analysis:?}");
        assert_eq!(analysis.clipped_samples, 0);
    }

    #[test]
    fn silent_take_has_no_speech() {
        assert_eq!(analyze(&noise(3.0), RATE, 2.0).speech, None);
        let digital_silence = vec![0; 48_000 * 3];
        let analysis = analyze(&digital_silence, RATE, 2.0);
        assert_eq!(analysis.speech, None);
        assert!(analysis.peak_dbfs.is_finite() && analysis.noise_floor_dbfs.is_finite());
        assert_eq!(analyze(&[], RATE, 2.0).speech, None);
    }

    #[test]
    fn noise_after_digital_silence_is_not_speech() {
        let mut take = vec![0; 48_000 * 2];
        take.extend(noise(2.0));
        assert_eq!(analyze(&take, RATE, 2.0).speech, None);
    }

    #[test]
    fn short_clicks_are_ignored() {
        let mut take = noise(3.0);
        burst(&mut take, 0.2, 0.25);
        assert_eq!(analyze(&take, RATE, 0.5).speech, None);
        burst(&mut take, 1.0, 2.2);
        let (start, _) = analyze(&take, RATE, 0.5).speech.unwrap();
        assert!(close(start, 1.0), "the isolated click is dropped: {start}");
    }

    #[test]
    fn close_pauses_merge_into_one_range() {
        let mut take = noise(3.0);
        burst(&mut take, 1.0, 1.5);
        burst(&mut take, 1.7, 1.75); // a short syllable after a short pause
        let (start, end) = analyze(&take, RATE, 0.5).speech.unwrap();
        assert!(close(start, 1.0) && close(end, 1.75), "{start}..{end}");
    }

    #[test]
    fn preroll_longer_than_take_uses_whole_take() {
        let mut take = noise(1.0);
        burst(&mut take, 0.5, 0.7);
        let analysis = analyze(&take, RATE, 5.0);
        assert!(analysis.speech.is_some(), "{analysis:?}");
    }

    #[test]
    fn counts_clipping() {
        let mut take = noise(1.0);
        take[100] = FULL_SCALE;
        take[200] = -FULL_SCALE - 1;
        take[300] = -FULL_SCALE;
        let near = (f64::from(FULL_SCALE) * 0.99).round();
        let below = (f64::from(FULL_SCALE) * 0.98).round();
        #[allow(clippy::cast_possible_truncation)]
        let (near, below) = (near as i32, below as i32);
        take[400] = near;
        take[500] = -near;
        take[600] = below;
        take[700] = -below;
        let analysis = analyze(&take, RATE, 0.5);
        assert_eq!(analysis.clipped_samples, 5);
        assert!(analysis.peak_dbfs.abs() < 0.01, "{analysis:?}");
    }
}
