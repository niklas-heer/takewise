//! Media smoke tests against real devices and a real film, run by hand:
//!
//!     cargo test -p takewise-media --test smoke -- --ignored [name]
//!
//! Without `--ignored` (as in `mise run check`) every test is skipped. The
//! harness is our own because AVPlayer needs the main thread, which libtest
//! does not give its tests. `plays_film_frames` plays the film named by
//! `TAKEWISE_SMOKE_FILM`; `routes_take_to_output` plays a silent take
//! through `TAKEWISE_SMOKE_OUTPUT`, else an output that is not the system's
//! default (it skips, saying so, when there is none), as the window plays
//! takes through the project's output.
//! `shows_about_panel` brings up the About panel for a moment; with
//! `TAKEWISE_SMOKE_ABOUT_PNG` set it captures the panel there (this needs
//! screen recording access for the app that runs the test). Capture needs
//! microphone access for the app that runs the test (the terminal or the
//! agent host).

use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use takewise_media::{
    About, Capture, ChannelMeter, DeviceInfo, FilmPlayer, TakePlayer, Transport, default_input,
    default_output, host_time_now_s, input_devices, input_latency_s, output_devices, pump_run_loop,
    set_nominal_sample_rate, show_about_panel, watching_devices,
};

type Outcome = Result<(), String>;
type Test = (&'static str, fn() -> Outcome);

const TESTS: [Test; 8] = [
    ("lists_input_devices", lists_input_devices),
    ("plays_film_frames", plays_film_frames),
    ("routes_take_to_output", routes_take_to_output),
    ("shows_about_panel", shows_about_panel),
    ("watches_device_changes", watches_device_changes),
    ("captures_one_channel", captures_one_channel),
    ("meters_every_channel", meters_every_channel),
    ("switches_nominal_rate", switches_nominal_rate),
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let run_ignored = args
        .iter()
        .any(|a| a == "--ignored" || a == "--include-ignored");
    let filters: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
    let mut failed = 0;
    for (name, test) in TESTS {
        if !filters.is_empty() && !filters.iter().any(|f| name.contains(f.as_str())) {
            continue;
        }
        if !run_ignored {
            println!("test {name} ... ignored");
            continue;
        }
        let started = Instant::now();
        match test() {
            Ok(()) => println!(
                "test {name} ... ok ({:.1} s)",
                started.elapsed().as_secs_f64()
            ),
            Err(err) => {
                failed += 1;
                println!("test {name} ... FAILED: {err}");
            }
        }
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn ensure(ok: bool, message: impl FnOnce() -> String) -> Outcome {
    if ok { Ok(()) } else { Err(message()) }
}

fn lists_input_devices() -> Outcome {
    let inputs = input_devices().map_err(|e| e.to_string())?;
    for input in &inputs {
        println!(
            "    input  {:<32} {} Hz {} ch {:?}",
            input.name, input.sample_rate, input.channels, input.transport
        );
    }
    ensure(!inputs.is_empty(), || "no input devices".to_owned())?;
    ensure(
        inputs.iter().all(|i| i.sample_rate > 0 && i.channels > 0),
        || "a device without rate or channels".to_owned(),
    )?;
    let output = default_output().map_err(|e| e.to_string())?;
    println!(
        "    output {:<32} {} Hz {} ch {:?} {:?}",
        output.name, output.sample_rate, output.channels, output.transport, output.output_source
    );
    let input = default_input().map_err(|e| e.to_string())?;
    let latency = input_latency_s(&input.name).map_err(|e| e.to_string())?;
    println!(
        "    input latency of {:?}: {:.2} ms",
        input.name,
        latency * 1000.0
    );
    ensure((0.0..0.5).contains(&latency), || {
        format!("implausible input latency {latency} s")
    })
}

fn film_path() -> Result<std::path::PathBuf, String> {
    std::env::var_os("TAKEWISE_SMOKE_FILM")
        .map(Into::into)
        .ok_or_else(|| "set TAKEWISE_SMOKE_FILM to a film to play".to_owned())
}

fn plays_film_frames() -> Outcome {
    let mut film = FilmPlayer::open(&film_path()?).map_err(|e| e.to_string())?;
    let loading = Instant::now();
    while !film.is_ready() {
        ensure(loading.elapsed() < Duration::from_secs(10), || {
            "film not ready after 10 s".to_owned()
        })?;
        pump_run_loop(0.01);
    }
    film.set_muted(true);

    let start = host_time_now_s() + 0.2;
    let scheduled = film.play_at(0.0, start).map_err(|e| e.to_string())?;
    let mut frames = 0;
    let mut last: Option<*const objc2_core_video::CVPixelBuffer> = None;
    let mut first_size = None;
    while host_time_now_s() < start + 2.0 {
        pump_run_loop(1.0 / 120.0);
        if let Some(frame) = film.latest_frame() {
            let ptr: *const _ = &raw const *frame;
            if last != Some(ptr) {
                frames += 1;
                last = Some(ptr);
                first_size.get_or_insert_with(|| {
                    (
                        objc2_core_video::CVPixelBufferGetWidth(&frame),
                        objc2_core_video::CVPixelBufferGetHeight(&frame),
                        objc2_core_video::CVPixelBufferGetPixelFormatType(&frame),
                    )
                });
            }
        }
    }
    let now = film.current_film_s();
    let tb = film.timebase();
    // Once playing, the player's timebase is the one play_at scheduled.
    let drift_ms = (tb.film_at(start + 1.0) - scheduled.film_at(start + 1.0)) * 1000.0;
    println!("    live timebase vs scheduled: {drift_ms:+.3} ms");
    ensure(drift_ms.abs() < 1.0, || {
        format!("live timebase is {drift_ms} ms off the scheduled one")
    })?;
    let predicted = tb.film_at(host_time_now_s());
    film.pause();
    println!(
        "    {frames} frames in 2 s, first {first_size:?}; film at {now:.3} s, timebase rate {:.6}, predicted {predicted:.3} s",
        tb.rate
    );
    ensure(frames >= 40, || format!("only {frames} frames in 2 s"))?;
    ensure(
        first_size.is_some_and(|(_, _, format)| format == 0x3432_3066),
        || format!("frames are not NV12 full range: {first_size:?}"),
    )?;
    ensure((1.9..2.2).contains(&now), || {
        format!("film at {now} s after 2 s")
    })?;
    ensure((tb.rate - 1.0).abs() < 0.01, || format!("rate {}", tb.rate))
}

/// A take plays through the output it is set to, not the system's default
/// output: half a second of silence at −60 dB, so nothing is heard.
fn routes_take_to_output() -> Outcome {
    let outputs = output_devices().map_err(|e| e.to_string())?;
    let default = default_output().map_err(|e| e.to_string())?;
    let name = if let Ok(name) = std::env::var("TAKEWISE_SMOKE_OUTPUT") {
        name
    } else {
        // The default output would prove nothing: a take plays there
        // without being set to it.
        let Some(other) = outputs.iter().find(|o| o.name != default.name) else {
            println!(
                "    only the default output {:?} is connected; skipped (connect another, or set TAKEWISE_SMOKE_OUTPUT)",
                default.name
            );
            return Ok(());
        };
        other.name.clone()
    };
    let wav = std::env::temp_dir().join(format!("takewise-smoke-{}.wav", std::process::id()));
    takewise_core::write_take_wav(&wav, &vec![0; 24_000], 48_000).map_err(|e| e.to_string())?;
    let outcome = play_silence_through(&wav, &name);
    let _ = std::fs::remove_file(&wav);
    outcome
}

fn play_silence_through(wav: &std::path::Path, name: &str) -> Outcome {
    let take = TakePlayer::open(wav).map_err(|e| e.to_string())?;
    ensure(take.output_device_uid().is_none(), || {
        "a new take does not follow the system output".to_owned()
    })?;
    take.set_output_device(name).map_err(|e| e.to_string())?;
    let uid = take.output_device_uid();
    println!("    take output {name:?}: {uid:?}");
    ensure(uid.is_some(), || format!("the take is not set to {name:?}"))?;
    let loading = Instant::now();
    while !take.is_ready() {
        ensure(loading.elapsed() < Duration::from_secs(10), || {
            "take not ready after 10 s".to_owned()
        })?;
        pump_run_loop(0.01);
    }
    take.set_gain_db(-60.0);
    take.play_at(0.0, host_time_now_s() + 0.1)
        .map_err(|e| e.to_string())?;
    pump_run_loop(0.6);
    take.stop();
    ensure(take.output_device_uid() == uid, || {
        "the take's output changed while it played".to_owned()
    })
}

/// CoreAudio accepts the device-change listeners. With
/// `TAKEWISE_SMOKE_WATCH=1` it also waits up to 30 s for a change: plug or
/// unplug headphones meanwhile, and the counter must move.
fn watches_device_changes() -> Outcome {
    ensure(watching_devices(), || {
        "CoreAudio refused the device listeners".to_owned()
    })?;
    if std::env::var_os("TAKEWISE_SMOKE_WATCH").is_none() {
        return Ok(());
    }
    let before = takewise_media::device_generation();
    println!("    plug or unplug an output within 30 s…");
    let waiting = Instant::now();
    while takewise_media::device_generation() == before {
        ensure(waiting.elapsed() < Duration::from_secs(30), || {
            "no device change seen in 30 s".to_owned()
        })?;
        pump_run_loop(0.1);
    }
    println!(
        "    devices changed: {before} -> {}",
        takewise_media::device_generation()
    );
    Ok(())
}

/// The standard About panel opens with Takewise's credits.
fn shows_about_panel() -> Outcome {
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    show_about_panel(&About {
        name: "Takewise",
        version: "0.0.0-smoke",
        description: "Record voiceover against a film, line by line or in passages.",
        link_text: "github.com/niklas-heer/takewise",
        link_url: "https://github.com/niklas-heer/takewise",
    })
    .map_err(|e| e.to_string())?;
    pump_run_loop(0.5);
    let windows = app.windows();
    let shown: Vec<_> = (0..windows.count())
        .map(|i| windows.objectAtIndex(i))
        .filter(|w| w.isVisible())
        .collect();
    for window in &shown {
        let frame = window.frame();
        println!(
            "    window {} at {:?}, {:?}",
            window.windowNumber(),
            frame.origin,
            frame.size
        );
        if let Some(png) = std::env::var_os("TAKEWISE_SMOKE_ABOUT_PNG") {
            let captured = std::process::Command::new("screencapture")
                .args(["-x", "-o", &format!("-l{}", window.windowNumber())])
                .arg(&png)
                .status()
                .map_err(|e| e.to_string())?;
            println!("    captured to {}: {captured}", png.to_string_lossy());
        }
        window.close();
    }
    ensure(!shown.is_empty(), || "no About panel came up".to_owned())
}

/// `TAKEWISE_SMOKE_INPUT`, else the built-in microphone, else the default
/// input (which may be a Bluetooth headset that cannot do 48 kHz).
fn smoke_input() -> Result<DeviceInfo, String> {
    let inputs = input_devices().map_err(|e| e.to_string())?;
    let wanted = std::env::var("TAKEWISE_SMOKE_INPUT").ok();
    let found = match &wanted {
        Some(name) => inputs.into_iter().find(|i| &i.name == name),
        None => inputs
            .into_iter()
            .find(|i| i.transport == Transport::BuiltIn),
    };
    match (found, wanted) {
        (Some(input), _) => Ok(input),
        (None, Some(name)) => Err(format!("no input named {name:?}")),
        (None, None) => default_input().map_err(|e| e.to_string()),
    }
}

fn captures_one_channel() -> Outcome {
    // A capture that waits on an unanswered microphone prompt never returns;
    // fail instead of hanging.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(20));
        println!(
            "test captures_one_channel ... FAILED: hung for 20 s (microphone permission prompt?)"
        );
        std::process::exit(1);
    });
    let input = smoke_input()?;
    let (tx, rx) = mpsc::channel();
    let capture = Capture::start(&input.name, 1, tx).map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_secs(2));
    let error = capture.last_error();
    capture.stop();
    let buffers: Vec<_> = rx.try_iter().collect();
    let samples: usize = buffers.iter().map(|b| b.samples.len()).sum();
    let peak = buffers
        .iter()
        .flat_map(|b| b.samples.iter())
        .map(|s| s.unsigned_abs())
        .max()
        .unwrap_or(0);
    println!(
        "    {:?}: {} buffers, {samples} samples, peak {peak}{}",
        input.name,
        buffers.len(),
        if peak == 0 {
            " (all zero: microphone access denied?)"
        } else {
            ""
        }
    );
    ensure(error.is_none(), || format!("stream error {error:?}"))?;
    ensure(samples >= 48_000 * 18 / 10, || {
        format!("only {samples} samples in 2 s")
    })?;
    ensure(
        buffers
            .windows(2)
            .all(|w| w[1].host_time_s > w[0].host_time_s),
        || "host times do not increase".to_owned(),
    )?;
    // Consecutive buffers are contiguous on the host clock.
    let worst = buffers
        .windows(2)
        .map(|w| (w[1].host_time_s - w[0].host_time_s - w[0].samples.len() as f64 / 48_000.0).abs())
        .fold(0.0, f64::max);
    println!(
        "    worst host-time gap between buffers: {:.3} ms",
        worst * 1000.0
    );
    ensure(worst < 0.002, || {
        format!("buffers not contiguous: {worst} s")
    })
}

/// Meters every channel of the smoke input for a second: one level per
/// channel, and at least one channel hears something.
fn meters_every_channel() -> Outcome {
    let input = smoke_input()?;
    let meter = ChannelMeter::start(&input.name).map_err(|e| e.to_string())?;
    std::thread::sleep(Duration::from_secs(1));
    let levels = meter.levels();
    println!(
        "    {:?}: {} channel(s), levels {levels:?} dBFS",
        input.name,
        meter.channels()
    );
    for output in output_devices().map_err(|e| e.to_string())? {
        println!(
            "    output {:<32} {:?} {:?}",
            output.name, output.transport, output.output_source
        );
    }
    ensure(levels.len() == usize::from(meter.channels()), || {
        format!("{} levels for {} channels", levels.len(), meter.channels())
    })?;
    ensure(levels.iter().any(|l| l.is_finite()), || {
        "every channel is silent (microphone access denied?)".to_owned()
    })
}

/// Switches the smoke input to 48 kHz when it runs at another rate, then
/// back. Skips when it already runs at 48 kHz.
fn switches_nominal_rate() -> Outcome {
    let input = smoke_input()?;
    if input.sample_rate == 48_000 {
        println!("    {:?} already runs at 48 kHz; skipped", input.name);
        return Ok(());
    }
    let before = input.sample_rate;
    set_nominal_sample_rate(&input.name, 48_000).map_err(|e| e.to_string())?;
    let now = smoke_input()?.sample_rate;
    set_nominal_sample_rate(&input.name, before).map_err(|e| e.to_string())?;
    ensure(now == 48_000, || {
        format!("still at {now} Hz after the switch")
    })
}
