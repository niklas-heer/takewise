+++
schema_version = 1
id = "01M83457YH2XZPKKWT15ENYGJT"
title = "Reach the loudness target with a look-ahead peak limiter"
date = "2026-11-23"
status = "accepted"
tags = ["audio", "export"]
supersedes = []
superseded_by = []
depends_on = []
related_to = []
+++
## Decision

The `clean` export reaches −16 LUFS integrated with a transparent look-ahead peak limiter after the static gain, instead of stopping the gain where the true peak reaches −1 dBTP. The limiter is `ffmpeg`'s `alimiter` (5 ms look-ahead and attack, 80 ms release, auto level off) run at 4× the sample rate between two `swr` resamplings, with a ceiling of −1.5 dBFS at that rate. It may reduce the gain by at most 6 dB; narration that would need more stays below −16 LUFS and the manifest says so. Its delay is measured with a click and compensated, so `clean` still places speech on the same samples as `raw`. The final encoded file is measured again: its true peak must be at or below −1 dBTP, and it lies within 0.5 LU of −16 LUFS unless the cap stopped the gain. The manifest gains `limiter_max_reduction_db` and `loudness_target_reached`.

This changes the design's rule for `clean` loudness, "one static gain, no dynamic normalization", in `docs/design.md` ("Export cleanup and formats", step 3).

## Context

Takewise aims for narration that needs little post-production. With one static gain capped by the true peak, narration with a high crest factor never reaches the target: Niklas's eight Kindred lines peak about 16 dB above their average level and exported at −19.1 LUFS, −1.1 dBTP, so he would still have to limit or compress them in Resolve.

A limiter touches only the brief peaks (plosives, stressed onsets) and leaves the level of the rest unchanged, so it reaches the target without the audible pumping or changed dynamics of loudness normalization (`loudnorm`'s dynamic mode) or a compressor. Measured on the Kindred lines: −16.0 LUFS, −1.5 dBTP (−1.4 dBTP at 44.1 kHz, 16 bits), with at most 4.2 dB of gain reduction and a static gain of 8.3 dB.

Alternatives considered:

- Keep the static gain only: transparent and simple, but leaves peaky narration 3 dB or more below the target and the work to the editor.
- `alimiter` at 48 kHz: holds the sample peak at the ceiling, but on the Kindred lines the true peak rose to −0.8 dBTP between samples; oversampling the limiter fixed that without lowering the ceiling.
- A compressor or `loudnorm`: changes the narration's dynamics far more than needed to tame a few peaks.

## Consequences

- `clean` is no longer purely a static gain: up to 6 dB of peak limiting is applied, which a careful listener may hear on hard plosives at the cap; the 6 dB cap and the measured maximum in the manifest keep that bounded and visible.
- Export runs a few more `ffmpeg` passes over the narration: a delay probe and up to five limiter-and-measure passes.
- An agent or narrator can read `loudness_target_reached` to see when a take is too peaky for the target and a retake or manual treatment would help.
- Revisit if narrators report audible limiting, if the cap is hit often, or if a delivery target other than −16 LUFS is needed.
