//! The last steps of export: the placed narration (48 kHz, in 24-bit units)
//! gets one static gain, is resampled once when the export rate is 44.1 kHz
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
/// …unless that would put the true peak above this.
pub const MAX_TRUE_PEAK_DBTP: f64 = -1.0;
/// The gain aims this far below [`MAX_TRUE_PEAK_DBTP`], since the meter
/// reports tenths of a dB.
const PEAK_MARGIN_DB: f64 = 0.1;
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

/// The one static gain of `clean`: to −16 LUFS, or less when the true peak
/// would rise above −1 dBTP.
pub fn static_gain_db(loudness: Loudness) -> f64 {
    let to_target = TARGET_LUFS - loudness.integrated_lufs;
    let to_peak = MAX_TRUE_PEAK_DBTP - PEAK_MARGIN_DB - loudness.true_peak_dbtp;
    to_target.min(to_peak)
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
    let filter = format!("aresample={rate}:resampler=swr:filter_size=128:cutoff=0.97");
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

    #[test]
    fn gain_is_the_smaller_of_loudness_and_peak() {
        // Quiet speech with modest peaks: loudness decides.
        let gain = static_gain_db(Loudness {
            integrated_lufs: -24.0,
            true_peak_dbtp: -12.0,
        });
        assert!((gain - 8.0).abs() < 1e-9);
        // Peaky speech: the peak decides.
        let gain = static_gain_db(Loudness {
            integrated_lufs: -24.0,
            true_peak_dbtp: -4.0,
        });
        assert!((gain - 2.9).abs() < 1e-9);
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
