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
/// Runs with fewer voiced frames than this after merging are dropped (in
/// 10 ms frames); bridged silence does not count.
const MIN_RUN_FRAMES: usize = 8;
/// A first or last run is dropped as a breath, a click or a chair, not the
/// first or last word, only when all hold: it has fewer voiced frames than
/// this (in 10 ms frames), at least `ISOLATION_GAP_FRAMES` separate it from
/// the next run, and its level is at least `WEAK_RUN_BELOW_LOUDEST_DB` below
/// the loudest of the runs that remain.
const WEAK_RUN_FRAMES: usize = 15;
const ISOLATION_GAP_FRAMES: usize = 40;
const WEAK_RUN_BELOW_LOUDEST_DB: f64 = 10.0;
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
    /// The frame levels the speech was found in, for later questions about
    /// the take's sound; not part of the JSON.
    #[serde(skip)]
    pub frames: Frames,
}

/// A take's frame levels and the level above which a frame is sound.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frames {
    /// The RMS level of each frame in dBFS, from the take's first sample.
    pub levels_dbfs: Vec<f64>,
    /// A frame above this level is sound, at or below it silence.
    pub threshold_dbfs: f64,
    /// The length of one frame in seconds.
    pub frame_s: f64,
}

impl Frames {
    /// The longest run of silent frames lying wholly between `from` and `to`
    /// seconds, in seconds; 0 when there are none or no frames at all.
    pub fn longest_silence_s(&self, from: f64, to: f64) -> f64 {
        if self.frame_s <= 0.0 || to <= from {
            return 0.0;
        }
        // Allow for rounding at the frame edges.
        let first = to_index((from / self.frame_s - 1e-6).ceil());
        let end = to_index((to / self.frame_s + 1e-6).floor()).min(self.levels_dbfs.len());
        let mut longest = 0_usize;
        let mut run = 0_usize;
        for &level in self.levels_dbfs.get(first..end).unwrap_or_default() {
            run = if level > self.threshold_dbfs {
                0
            } else {
                run + 1
            };
            longest = longest.max(run);
        }
        longest as f64 * self.frame_s
    }
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
        frames: Frames {
            levels_dbfs: levels,
            threshold_dbfs: threshold,
            frame_s: frame_len as f64 / f64::from(sample_rate),
        },
    }
}

fn percentile(values: &[f64], q: f64) -> Option<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let idx = to_index((sorted.len().checked_sub(1)?) as f64 * q);
    sorted.get(idx).copied()
}

/// A stretch of speech frames with short gaps bridged.
#[derive(Debug, Clone, Copy)]
struct Run {
    start: usize,
    end: usize,
    /// Frames above the threshold, not counting bridged gaps.
    voiced: usize,
    /// Sum of the voiced frames' mean-square levels, relative to full scale.
    power: f64,
}

impl Run {
    /// Frames of silence between this run and a later one.
    fn gap_to(self, later: Run) -> usize {
        later.start - self.end - 1
    }

    /// Mean square of the voiced frames, relative to full scale.
    fn mean_power(self) -> f64 {
        self.power / self.voiced as f64
    }

    /// RMS level of the voiced frames in dBFS.
    fn level_db(self) -> f64 {
        10.0 * self.mean_power().log10()
    }

    /// Whether this run, isolated by `gap` frames at one end of the speech,
    /// is noise rather than a word, judged against the loudest run that
    /// remains, so noise at the other end cannot shield it and trimming never
    /// leaves only weaker runs; `rest` is never empty.
    fn is_isolated_noise(self, gap: usize, rest: &[Run]) -> bool {
        self.voiced < WEAK_RUN_FRAMES
            && gap >= ISOLATION_GAP_FRAMES
            && loudest_level(rest)
                .is_some_and(|loudest| self.level_db() <= loudest - WEAK_RUN_BELOW_LOUDEST_DB)
    }
}

/// The RMS level in dBFS of the loudest run.
fn loudest_level(runs: &[Run]) -> Option<f64> {
    runs.iter().map(|r| r.level_db()).max_by(f64::total_cmp)
}

/// The first and last frame of speech: bridges short gaps, drops runs with
/// too few voiced frames, then drops short, quiet runs isolated at either end.
fn speech_runs(levels: &[f64], threshold: f64) -> Option<(usize, usize)> {
    let mut runs: Vec<Run> = Vec::new();
    for (idx, db) in levels.iter().enumerate().filter(|(_, db)| **db > threshold) {
        let power = 10f64.powf(db / 10.0);
        match runs.last_mut() {
            Some(run) if idx - run.end - 1 < MERGE_GAP_FRAMES => {
                run.end = idx;
                run.voiced += 1;
                run.power += power;
            }
            _ => runs.push(Run {
                start: idx,
                end: idx,
                voiced: 1,
                power,
            }),
        }
    }
    let mut kept: &[Run] = &runs
        .into_iter()
        .filter(|run| run.voiced >= MIN_RUN_FRAMES)
        .collect::<Vec<_>>();
    loop {
        if let [first, rest @ ..] = kept
            && let Some(next) = rest.first()
            && first.is_isolated_noise(first.gap_to(*next), rest)
        {
            kept = rest;
        } else if let [rest @ .., last] = kept
            && let Some(previous) = rest.last()
            && last.is_isolated_noise(previous.gap_to(*last), rest)
        {
            kept = rest;
        } else {
            break;
        }
    }
    Some((kept.first()?.start, kept.last()?.end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::takes::FULL_SCALE;
    use crate::testutil::{RATE, burst, burst_at, noise};

    #[test]
    fn noise_at_both_ends_is_trimmed() {
        // 90 ms of noise 12 dB below the phrase, isolated at either end.
        let mut levels = vec![SILENCE_DBFS; 200];
        levels[0..9].fill(-21.0);
        levels[49..149].fill(-9.0);
        levels[189..198].fill(-21.0);
        assert_eq!(speech_runs(&levels, -60.0), Some((49, 148)));
    }

    #[test]
    fn weak_run_is_judged_against_the_loudest_run() {
        // A 100 ms breath at -37 dBFS, then phrases at -40 and -20 dBFS. The
        // breath is 17 dB below the loudest phrase, so it is trimmed even
        // though it is louder than the quieter phrase.
        let mut levels = vec![SILENCE_DBFS; 160];
        levels[0..10].fill(-37.0);
        levels[50..80].fill(-40.0);
        levels[120..150].fill(-20.0);
        assert_eq!(speech_runs(&levels, -60.0), Some((50, 149)));
    }

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
    fn bridged_clicks_are_not_speech() {
        // Two 10 ms clicks 150 ms apart bridge into one 160 ms run, but only
        // 20 ms of it is voiced.
        let mut take = noise(3.0);
        burst(&mut take, 1.0, 1.01);
        burst(&mut take, 1.15, 1.16);
        assert_eq!(analyze(&take, RATE, 0.5).speech, None);
    }

    #[test]
    fn isolated_noise_before_speech_is_not_the_start() {
        // A 90 ms bump (a breath, a chair) 1 s before the real speech.
        let mut take = noise(4.0);
        burst_at(&mut take, 0.91, 1.0, -37.0);
        burst(&mut take, 2.0, 3.0);
        let (start, end) = analyze(&take, RATE, 0.5).speech.unwrap();
        assert!(close(start, 2.0), "start {start}");
        assert!(close(end, 3.0), "end {end}");
    }

    #[test]
    fn isolated_noise_after_speech_is_not_the_end() {
        let mut take = noise(4.0);
        burst(&mut take, 1.0, 2.0);
        burst_at(&mut take, 3.0, 3.09, -37.0);
        let (start, end) = analyze(&take, RATE, 0.5).speech.unwrap();
        assert!(close(start, 1.0) && close(end, 2.0), "{start}..{end}");
    }

    #[test]
    fn a_short_word_before_a_quieter_breath_is_kept() {
        // A 100 ms word, then a 90 ms breath 500 ms later.
        let mut take = noise(3.0);
        burst(&mut take, 1.0, 1.1);
        burst_at(&mut take, 1.6, 1.69, -37.0);
        let (start, end) = analyze(&take, RATE, 0.5).speech.unwrap();
        assert!(close(start, 1.0) && close(end, 1.1), "{start}..{end}");
    }

    #[test]
    fn a_loud_short_last_word_is_kept() {
        // A 130 ms last word 450 ms after the phrase, as loud as the phrase.
        let mut take = noise(4.0);
        burst(&mut take, 1.0, 2.0);
        burst(&mut take, 2.45, 2.58);
        let (start, end) = analyze(&take, RATE, 0.5).speech.unwrap();
        assert!(close(start, 1.0) && close(end, 2.58), "{start}..{end}");
    }

    #[test]
    fn a_short_first_word_before_a_pause_is_kept() {
        // "Yes." (120 ms), a 500 ms pause, then the phrase.
        let mut take = noise(4.0);
        burst(&mut take, 1.0, 1.12);
        burst(&mut take, 1.62, 2.6);
        let (start, end) = analyze(&take, RATE, 0.5).speech.unwrap();
        assert!(close(start, 1.0) && close(end, 2.6), "{start}..{end}");
    }

    #[test]
    fn a_burst_12_db_below_speech_is_still_trimmed() {
        // Half-scale bursts are about -9 dBFS RMS; this one is 12 dB quieter.
        let mut take = noise(4.0);
        burst_at(&mut take, 0.91, 1.0, -21.0);
        burst(&mut take, 2.0, 3.0);
        let (start, end) = analyze(&take, RATE, 0.5).speech.unwrap();
        assert!(close(start, 2.0) && close(end, 3.0), "{start}..{end}");
    }

    #[test]
    fn a_short_word_alone_is_still_speech() {
        // A single 100 ms word with nothing else is kept.
        let mut take = noise(3.0);
        burst(&mut take, 1.0, 1.1);
        let (start, end) = analyze(&take, RATE, 0.5).speech.unwrap();
        assert!(close(start, 1.0) && close(end, 1.1), "{start}..{end}");
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
    fn measures_the_longest_silence_between_two_times() {
        // A breath at 0.5-0.7 s, 200 ms of silence, then words from 0.9 s.
        let mut take = noise(3.0);
        burst_at(&mut take, 0.5, 0.7, -30.0);
        burst(&mut take, 0.9, 2.0);
        let analysis = analyze(&take, RATE, 0.4);
        let frames = &analysis.frames;
        let gap = frames.longest_silence_s(0.5, 1.0);
        assert!((gap - 0.2).abs() <= 0.02, "gap {gap}");
        // Only frames wholly inside the range count.
        let gap = frames.longest_silence_s(0.75, 1.0);
        assert!((gap - 0.15).abs() <= 0.02, "gap {gap}");
        assert!(frames.longest_silence_s(1.0, 1.9) < 0.02);
        assert_eq!(frames.longest_silence_s(1.0, 0.5), 0.0);
        // Without frames there is no silence to find.
        assert_eq!(Frames::default().longest_silence_s(0.0, 1.0), 0.0);
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
