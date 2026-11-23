//! The last steps of export: the placed narration (48 kHz, in 24-bit units)
//! is leveled for `clean` (one static gain through a look-ahead true-peak
//! limiter), is resampled once when the export rate is 44.1 kHz
//! (with ffmpeg's swr),
//! dithered when written at 16 bits, and encoded as WAV or FLAC; loudness
//! and true peak are measured with `ffmpeg`'s `ebur128` (see
//! `docs/design.md`, "Export cleanup and formats").

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use crate::ffmpeg::{Ffmpeg, read_float_wav, write_float_wav};
use crate::project::ExportFormat;

/// The takes' sample rate; placement happens at it.
pub const SOURCE_RATE: u32 = 48_000;
/// `clean` aims the integrated loudness here…
pub const TARGET_LUFS: f64 = -16.0;
/// …and keeps the final file's true peak at or below this.
pub const MAX_TRUE_PEAK_DBTP: f64 = -1.0;
/// The limiter's ceiling, as a peak of the 4× oversampled signal: 0.5 dB
/// below [`MAX_TRUE_PEAK_DBTP`] leaves room for resampling and dither.
pub const LIMITER_CEILING_DBFS: f64 = -1.5;
/// The limiter never reduces the gain by more than this; narration that
/// would need more stays below [`TARGET_LUFS`].
pub const MAX_LIMITER_REDUCTION_DB: f64 = 6.0;
/// The limiter's look-ahead (its attack) and release.
const LIMITER_ATTACK_MS: f64 = 5.0;
const LIMITER_RELEASE_MS: f64 = 80.0;
/// Leveling stops once the loudness lies this close to [`TARGET_LUFS`]…
const LEVEL_TOLERANCE_LU: f64 = 0.1;
/// …or after this many limiter passes.
const LEVEL_PASSES: usize = 5;
/// The oversampling resampler around the limiter (and for 44.1 kHz).
const SWR: &str = "resampler=swr:filter_size=128:cutoff=0.97";
/// 24-bit full scale as ffmpeg converts it to float.
pub const UNIT: f64 = 8_388_608.0;

/// The export file's sample rate, bit depth and format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub format: ExportFormat,
}

/// Integrated loudness and true peak, as `ebur128` measures them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudness {
    pub integrated_lufs: f64,
    pub true_peak_dbtp: f64,
}

/// A look-ahead peak limiter run by `ffmpeg` at 4× the source rate
/// (`alimiter` between two swr resamplings), so it catches the peaks
/// between samples that a true-peak meter sees. Its delay is measured with
/// a click and compensated, so limited speech keeps its samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limiter {
    pub ffmpeg: Ffmpeg,
    /// Samples at 48 kHz the chain delays its output by.
    pub delay: usize,
}

/// What [`Limiter::apply`] made.
#[derive(Debug, Clone, PartialEq)]
pub struct Limited {
    /// 48 kHz, 24-bit units, as long as the input.
    pub samples: Vec<f64>,
    /// The most the limiter reduced any sample near the ceiling, in dB.
    pub max_reduction_db: f64,
}

impl Limiter {
    /// Measures the chain's delay with a click fixture.
    pub fn new(ffmpeg: Ffmpeg) -> Result<Limiter, String> {
        let delay = crate::clean::filter_delay(&ffmpeg, &limiter_chain(0))?;
        Ok(Limiter { ffmpeg, delay })
    }

    /// `samples` (48 kHz, 24-bit units) with `gain_db` applied, then
    /// limited to [`LIMITER_CEILING_DBFS`].
    pub fn apply(&self, samples: &[f64], gain_db: f64) -> Result<Limited, String> {
        let gain = 10f64.powf(gain_db / 20.0);
        let gained: Vec<f64> = samples.iter().map(|v| v * gain / UNIT).collect();
        let tmp = work_dir()?;
        let (src, out) = (tmp.path().join("in.wav"), tmp.path().join("out.wav"));
        write_float_wav(&src, &gained, SOURCE_RATE)?;
        self.ffmpeg
            .filter_to_float(&src, &limiter_chain(self.delay), &out)?;
        let (mut limited, rate) = read_float_wav(&out)?;
        if rate != SOURCE_RATE {
            return Err(format!(
                "the limiter returned {rate} Hz, not {SOURCE_RATE} Hz"
            ));
        }
        limited.resize(gained.len(), 0.0);
        let max_reduction_db = max_reduction_db(&gained, &limited);
        Ok(Limited {
            samples: limited.into_iter().map(|v| v * UNIT).collect(),
            max_reduction_db,
        })
    }
}

/// The limiter chain, compensated for `delay`.
fn limiter_chain(delay: usize) -> String {
    let ceiling = 10f64.powf(LIMITER_CEILING_DBFS / 20.0);
    let oversampled = SOURCE_RATE * 4;
    let chain = format!(
        "aresample={oversampled}:{SWR},alimiter=limit={ceiling:.6}:attack={LIMITER_ATTACK_MS}:release={LIMITER_RELEASE_MS}:level=0:latency=1,aresample={SOURCE_RATE}:{SWR}"
    );
    crate::clean::compensated(&chain, delay)
}

/// The largest gain reduction from `before` to `after` (both −1..1) over
/// the samples within 6 dB of the ceiling or above it, where the limiter
/// acts and the ratio is not swamped by rounding; 0 when there are none.
fn max_reduction_db(before: &[f64], after: &[f64]) -> f64 {
    let near = 10f64.powf((LIMITER_CEILING_DBFS - 6.0) / 20.0);
    before
        .iter()
        .zip(after)
        .filter(|(b, _)| b.abs() >= near)
        .map(|(b, a)| 20.0 * (b.abs() / a.abs().max(f64::MIN_POSITIVE)).log10())
        .fold(0.0, f64::max)
}

/// The narration [`level`] made for `clean`.
#[derive(Debug, Clone, PartialEq)]
pub struct Leveled {
    /// 48 kHz, 24-bit units.
    pub samples: Vec<f64>,
    /// The static gain before the limiter.
    pub gain_db: f64,
    pub max_reduction_db: f64,
    /// The limiter's 6 dB cap kept the narration below [`TARGET_LUFS`].
    pub capped: bool,
}

/// Levels the placed narration (48 kHz, 24-bit units) to [`TARGET_LUFS`]:
/// measures it, applies the static gain to the target through the
/// [`Limiter`], measures again and corrects the gain for the loudness the
/// limiter took, until within [`LEVEL_TOLERANCE_LU`]. The gain stops where
/// the limiter would reduce more than [`MAX_LIMITER_REDUCTION_DB`].
pub fn level(ffmpeg: &Ffmpeg, samples: &[f64]) -> Result<Leveled, String> {
    let before = measure_samples(ffmpeg, samples)?;
    let limiter = Limiter::new(ffmpeg.clone())?;
    let mut max_gain = LIMITER_CEILING_DBFS + MAX_LIMITER_REDUCTION_DB - before.true_peak_dbtp;
    let mut gain_db = (TARGET_LUFS - before.integrated_lufs).min(max_gain);
    let mut pass = 0;
    loop {
        pass += 1;
        let pass_out = limiter.apply(samples, gain_db)?;
        let over = pass_out.max_reduction_db - MAX_LIMITER_REDUCTION_DB;
        if over > 0.0 {
            // The meter's tenths of a dB or the limiter's envelope asked for
            // a little more than the cap: the gain may rise no further.
            max_gain = gain_db - over;
        }
        let after = measure_samples(ffmpeg, &pass_out.samples)?;
        let short = TARGET_LUFS - after.integrated_lufs;
        let next = (gain_db + short).min(max_gain);
        let settled =
            over <= 0.0 && (short.abs() <= LEVEL_TOLERANCE_LU || (next - gain_db).abs() < 0.01);
        if settled || pass >= LEVEL_PASSES {
            return Ok(Leveled {
                samples: pass_out.samples,
                gain_db,
                max_reduction_db: pass_out.max_reduction_db,
                capped: short > LEVEL_TOLERANCE_LU && gain_db >= max_gain - 0.01,
            });
        }
        gain_db = next;
    }
}

/// A sample index at 48 kHz as an index at `rate`: `round(n × rate / 48000)`.
pub fn at_rate(n: u64, rate: u32) -> u64 {
    if rate == SOURCE_RATE {
        return n;
    }
    (u128::from(n) * u128::from(rate) * 2 + u128::from(SOURCE_RATE))
        .checked_div(u128::from(SOURCE_RATE) * 2)
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(u64::MAX)
}

/// Measures a file's loudness and true peak with `ebur128`.
pub fn measure(ffmpeg: &Ffmpeg, path: &Path) -> Result<Loudness, String> {
    let stderr = ffmpeg.run(&[
        std::ffi::OsStr::new("-nostats"),
        std::ffi::OsStr::new("-i"),
        path.as_os_str(),
        std::ffi::OsStr::new("-af"),
        std::ffi::OsStr::new("ebur128=peak=true:framelog=verbose"),
        std::ffi::OsStr::new("-f"),
        std::ffi::OsStr::new("null"),
        std::ffi::OsStr::new("-"),
    ])?;
    parse_ebur128(&stderr).ok_or_else(|| {
        format!(
            "could not read the loudness of {} from ffmpeg's ebur128",
            path.display()
        )
    })
}

/// [`measure`] of samples at 48 kHz in 24-bit units, through a float WAV.
pub fn measure_samples(ffmpeg: &Ffmpeg, samples: &[f64]) -> Result<Loudness, String> {
    let tmp = work_dir()?;
    let path = tmp.path().join("narration.wav");
    let scaled: Vec<f64> = samples.iter().map(|v| v / UNIT).collect();
    write_float_wav(&path, &scaled, SOURCE_RATE)?;
    measure(ffmpeg, &path)
}

/// The last `Integrated loudness` and `True peak` of an `ebur128` summary.
pub fn parse_ebur128(stderr: &str) -> Option<Loudness> {
    let summary = &stderr[stderr.rfind("Summary:")?..];
    let value_after = |section: &str, key: &str| -> Option<f64> {
        let rest = &summary[summary.find(section)?..];
        let line = rest.lines().find(|l| l.trim_start().starts_with(key))?;
        line.trim_start()
            .strip_prefix(key)?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    Some(Loudness {
        integrated_lufs: value_after("Integrated loudness:", "I:")?,
        true_peak_dbtp: value_after("True peak:", "Peak:")?,
    })
}

/// The narration at the target's rate and bit depth: `samples` (48 kHz,
/// 24-bit units) with `gain_db` applied, resampled once when the
/// target is 44.1 kHz (ffmpeg's own resampler, swr, with a 128-tap
/// filter; exactly `round(n × rate / 48000)` samples long),
/// rounded to 24 bits or dithered (TPDF) to 16. Resampling needs `ffmpeg`.
pub fn prepare(
    samples: &[f64],
    gain_db: f64,
    target: Target,
    ffmpeg: Option<&Ffmpeg>,
) -> Result<Vec<i32>, String> {
    let gain = 10f64.powf(gain_db / 20.0);
    let mut x: Vec<f64> = samples.iter().map(|v| v * gain).collect();
    if target.sample_rate != SOURCE_RATE {
        let ffmpeg = ffmpeg.ok_or("resampling needs ffmpeg")?;
        let len = usize::try_from(at_rate(x.len() as u64, target.sample_rate))
            .map_err(|err| err.to_string())?;
        x = resample(ffmpeg, &x, target.sample_rate)?;
        x.resize(len, 0.0);
    }
    Ok(quantize(&x, target.bit_depth))
}

fn resample(ffmpeg: &Ffmpeg, x: &[f64], rate: u32) -> Result<Vec<f64>, String> {
    let tmp = work_dir()?;
    let (src, out) = (tmp.path().join("in.wav"), tmp.path().join("out.wav"));
    let scaled: Vec<f64> = x.iter().map(|v| v / UNIT).collect();
    write_float_wav(&src, &scaled, SOURCE_RATE)?;
    // swr, not soxr: Homebrew's ffmpeg is built without libsoxr. A long
    // Kaiser filter keeps the passband flat to 20 kHz.
    let filter = format!("aresample={rate}:{SWR}");
    ffmpeg.run(&[
        std::ffi::OsStr::new("-loglevel"),
        std::ffi::OsStr::new("error"),
        std::ffi::OsStr::new("-i"),
        src.as_os_str(),
        std::ffi::OsStr::new("-af"),
        std::ffi::OsStr::new(&filter),
        std::ffi::OsStr::new("-c:a"),
        std::ffi::OsStr::new("pcm_f32le"),
        out.as_os_str(),
    ])?;
    let (y, got) = read_float_wav(&out)?;
    if got != rate {
        return Err(format!("ffmpeg resampled to {got} Hz, not {rate} Hz"));
    }
    Ok(y.into_iter().map(|v| v * UNIT).collect())
}

/// Rounds 24-bit units to 24 bits, or to 16 bits with triangular (TPDF)
/// dither of ±1 LSB from a fixed seed, so an export is reproducible.
#[allow(clippy::cast_possible_truncation)]
pub fn quantize(x: &[f64], bit_depth: u16) -> Vec<i32> {
    if bit_depth == 24 {
        let (lo, hi) = (-UNIT, UNIT - 1.0);
        return x.iter().map(|v| v.round().clamp(lo, hi) as i32).collect();
    }
    let mut state: u32 = 0x6d2b_79f5;
    let mut uniform = move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(state >> 8) / f64::from(1_u32 << 24)
    };
    let step = UNIT / 32_768.0;
    x.iter()
        .map(|v| {
            let dither = uniform() - uniform();
            (v / step + dither).round().clamp(-32_768.0, 32_767.0) as i32
        })
        .collect()
}

/// Writes `pcm` (at the target's bit depth) to `out` as WAV or FLAC; FLAC
/// is encoded by `ffmpeg` from a temporary WAV.
pub fn write(
    out: &Path,
    pcm: &[i32],
    target: Target,
    ffmpeg: Option<&Ffmpeg>,
) -> Result<(), String> {
    match target.format {
        ExportFormat::Wav => write_wav(out, pcm, target),
        ExportFormat::Flac => {
            let ffmpeg = ffmpeg.ok_or("FLAC needs ffmpeg")?;
            let tmp = work_dir()?;
            let wav = tmp.path().join("narration.wav");
            write_wav(&wav, pcm, target)?;
            ffmpeg.run(&[
                std::ffi::OsStr::new("-loglevel"),
                std::ffi::OsStr::new("error"),
                std::ffi::OsStr::new("-i"),
                wav.as_os_str(),
                std::ffi::OsStr::new("-c:a"),
                std::ffi::OsStr::new("flac"),
                std::ffi::OsStr::new("-f"),
                std::ffi::OsStr::new("flac"),
                out.as_os_str(),
            ])?;
            File::open(out)
                .and_then(|f| f.sync_all())
                .map_err(|err| format!("could not sync {}: {err}", out.display()))
        }
    }
}

fn write_wav(out: &Path, pcm: &[i32], target: Target) -> Result<(), String> {
    let error = |err: hound::Error| format!("could not write {}: {err}", out.display());
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: target.sample_rate,
        bits_per_sample: target.bit_depth,
        sample_format: hound::SampleFormat::Int,
    };
    let file = File::create(out).map_err(|err| format!("{}: {err}", out.display()))?;
    let mut writer = hound::WavWriter::new(BufWriter::new(file), spec).map_err(error)?;
    for &s in pcm {
        if target.bit_depth == 16 {
            #[allow(clippy::cast_possible_truncation)]
            writer.write_sample(s as i16).map_err(error)?;
        } else {
            writer.write_sample(s).map_err(error)?;
        }
    }
    writer.finalize().map_err(error)?;
    File::open(out)
        .and_then(|f| f.sync_all())
        .map_err(|err| format!("could not sync {}: {err}", out.display()))
}

fn work_dir() -> Result<tempfile::TempDir, String> {
    tempfile::Builder::new()
        .prefix("takewise-export-")
        .tempdir()
        .map_err(|err| format!("could not create a temporary directory: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUMMARY: &str = "[Parsed_ebur128_0 @ 0x1] Summary:

  Integrated loudness:
    I:         -21.8 LUFS
    Threshold: -31.8 LUFS

  Loudness range:
    LRA:        20.0 LU
    Threshold: -41.8 LUFS

  True peak:
    Peak:      -18.1 dBFS
";

    #[test]
    fn parses_the_ebur128_summary() {
        assert_eq!(
            parse_ebur128(SUMMARY),
            Some(Loudness {
                integrated_lufs: -21.8,
                true_peak_dbtp: -18.1
            })
        );
        assert_eq!(parse_ebur128("no summary"), None);
    }

    fn ffmpeg_or_skip() -> Option<Ffmpeg> {
        let found = Ffmpeg::find();
        if found.is_none() {
            eprintln!("skipped: ffmpeg not found");
        }
        found
    }

    /// Two seconds of low noise (24-bit units) with a click peaking at
    /// sample 50 000, `over_db` above the limiter's ceiling. The click is a
    /// Gaussian pulse a few samples wide, so, like speech, it has nothing
    /// near 24 kHz for the resampler's filter to take away.
    fn click_over_ceiling(over_db: f64) -> Vec<f64> {
        let mut x: Vec<f64> = crate::testutil::noise_at(2.0, -60.0)
            .into_iter()
            .map(f64::from)
            .collect();
        let peak = UNIT * 10f64.powf((LIMITER_CEILING_DBFS + over_db) / 20.0);
        for (n, v) in x.iter_mut().enumerate().skip(49_970).take(61) {
            let d = n as f64 - 50_000.0;
            *v += peak * (-d * d / 18.0).exp();
        }
        x
    }

    fn peak_index(x: &[f64]) -> usize {
        x.iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .map(|(i, _)| i)
            .unwrap()
    }

    #[test]
    fn limiter_keeps_a_click_on_its_sample() {
        let Some(ffmpeg) = ffmpeg_or_skip() else {
            return;
        };
        let limiter = Limiter::new(ffmpeg).unwrap();
        let x = click_over_ceiling(3.0);
        let out = limiter.apply(&x, 0.0).unwrap();
        assert_eq!(out.samples.len(), x.len());
        assert_eq!(
            peak_index(&out.samples),
            50_000,
            "limiter delay {}",
            limiter.delay
        );
        // The click was brought down to the ceiling, by about 3 dB.
        let peak_db = 20.0 * (out.samples[50_000].abs() / UNIT).log10();
        assert!(peak_db <= LIMITER_CEILING_DBFS + 0.05, "{peak_db}");
        assert!(
            (out.max_reduction_db - 3.0).abs() < 0.3,
            "{}",
            out.max_reduction_db
        );
        // Gain applies before the limiter.
        let quiet = limiter.apply(&x, -6.0).unwrap();
        assert!(quiet.max_reduction_db < 0.01, "{}", quiet.max_reduction_db);
    }

    #[test]
    fn leveling_stops_at_six_db_of_limiting() {
        let Some(ffmpeg) = ffmpeg_or_skip() else {
            return;
        };
        // Quiet noise with a click near full scale: reaching −16 LUFS would
        // need far more than 6 dB of limiting.
        let x = click_over_ceiling(1.0);
        let leveled = level(&ffmpeg, &x).unwrap();
        assert!(leveled.capped);
        assert!(
            leveled.max_reduction_db <= MAX_LIMITER_REDUCTION_DB
                && leveled.max_reduction_db > MAX_LIMITER_REDUCTION_DB - 0.5,
            "{}",
            leveled.max_reduction_db
        );
        assert!(leveled.gain_db < 6.0, "{}", leveled.gain_db);
    }

    #[test]
    fn ranges_follow_the_export_rate() {
        assert_eq!(at_rate(480_000, 48_000), 480_000);
        assert_eq!(at_rate(480_000, 44_100), 441_000);
        // round(1 × 44100 / 48000) = round(0.91875) = 1; 7 → 6.43 → 6.
        assert_eq!(at_rate(1, 44_100), 1);
        assert_eq!(at_rate(7, 44_100), 6);
        assert_eq!(at_rate(2_472_000, 44_100), 2_271_150);
    }

    #[test]
    fn sixteen_bits_are_dithered_and_reproducible() {
        // A constant between two 16-bit steps comes out as both, averaging
        // to the value: dither, not truncation.
        let step = UNIT / 32_768.0;
        let x = vec![1000.25 * step; 10_000];
        let pcm = quantize(&x, 16);
        assert!(pcm.iter().all(|&s| (999..=1002).contains(&s)), "{pcm:?}");
        let mean = pcm.iter().map(|&s| f64::from(s)).sum::<f64>() / pcm.len() as f64;
        assert!((mean - 1000.25).abs() < 0.05, "{mean}");
        assert_eq!(pcm, quantize(&x, 16));
        // 24 bits round, without dither.
        assert_eq!(quantize(&[1.4, -2.6, 9e9], 24), [1, -3, (1 << 23) - 1]);
    }
}
