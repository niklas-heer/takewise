+++
schema_version = 1
id = "01M82MHA04XN6RJAATM2MFK770"
title = "Play, capture and align media through AVPlayer, GPUI surfaces and cpal"
date = "2026-11-22"
status = "proposed"
tags = ["media", "architecture"]
supersedes = []
superseded_by = []
depends_on = []
related_to = []
+++
## Decision

- **Video:** AVPlayer with `AVPlayerItemVideoOutput` delivering NV12 full-range, Metal-compatible, IOSurface-backed pixel buffers, drawn with GPUI's `surface()` element (gpui-pre 0.3.12, pinned by gpui-kit 0.10.0). No `AVPlayerLayer` fallback.
- **Capture:** cpal (0.18) input stream on a named device, one channel, written with hound; CoreAudio latencies through `objc2-core-audio`. Not AVAudioEngine.
- **Alignment:** film time of sample `i` in a buffer = `timebase(callback_host_time + i / sample_rate − L_in) + latency_offset_ms`, with `L_in` = device latency + safety offset + stream latency. Output latency is not subtracted; AVPlayer's clock already accounts for it. `latency_offset_ms` is calibrated per device pair by the loopback test.
- **Permission:** `takewise record` launches a signed `Takewise.app` with `NSMicrophoneUsageDescription`.
- **Toolchain:** Rust 1.99.0, because gpui-pre 0.3.12 does not compile on 1.97.1.

Proposed on 2026-11-22 from the milestone 0 spike's evidence.

## Context

The spike (throwaway, not committed) measured on Niklas's Mac:

- Video: 30 fps with no dropped frames in four of five runs after a 150–290 ms start-up hiccup; one run under heavy system load rendered about 26 frames per second. Text overlays draw on top of the surface.
- Loopback with MacBook speakers into the MacBook mic: beeps 31.5 ms late with a standard deviation of 0.01 ms, at film time 0.1 s and after seeks, for AAC and LPCM films and for a take played locked to the film. Reported CoreAudio latencies alone leave −19.5 ms.
- cpal's capture timestamp subtracts one buffer and omits the 50 ms stream latency of the built-in mic, so the callback host time is used instead.
- AVAudioEngine needs a workaround to select an input device and carries an output node that could monitor the mic.
- A bare binary launched from T3 Code hung on capture while the host app's microphone prompt was unanswered; an ad hoc signed bundle launched with `open` recorded at once.
- The RØDECaster Duo delivered digital silence on all four channels during the spike, so its latency is not yet measured.

Alternatives: an `AVPlayerLayer` under GPUI's view (not needed once `surface()` worked); AVAudioEngine (above); a bare binary for recording (above).

## Consequences

- Takewise needs Rust 1.99.0, newer than Focal's pin.
- The app keeps a history of timebase changes so buffers captured around a seek map correctly, and plays the film briefly muted before the first take to warm the output device.
- Recording needs an app bundle from milestone 2 on, not only at release; Developer ID signing keeps the microphone grant across rebuilds.
- Calibration per device pair is required for ±20 ms; `doctor` warns about Bluetooth output (187 ms reported latency).
