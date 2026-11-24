+++
schema_version = 1
id = "01M85QSYVN53WD0TJZBTYECXR3"
title = "Remember latency and setup results across projects"
date = "2026-11-24"
status = "proposed"
tags = ["setup", "latency"]
supersedes = []
superseded_by = []
depends_on = []
related_to = []
+++
## Context

Each new Takewise project asked again for the setup check and the loopback latency test on the same devices, including the Bluetooth headphones Niklas used, whose measured offset barely changed between projects (−29, −29, −28 ms). Only the input, channel and output were remembered, in `settings.toml`.

## Decision

Remember, in `remembered.json` beside `settings.toml` (one locked read-modify-write, never rewriting `settings.toml`), each latency measurement per exact input, output and capture buffer size, and the latest setup check result with its room tone per exact input, channel and output. The window copies a remembered latency into a project without a fitting calibration (`from = "remembered"`), replaces such a copy when a newer measurement is remembered, and uses it only while the route is fully known. A same-day setup result is adopted with its room tone copied into the project, so checks and `clean` keep working from project files. Remembered offsets apply to Bluetooth outputs; L re-measures latency alone.

## Alternatives

- Keep everything per project: exact, but repeats a 15 s test and the setup check every time.
- Apply calibrations by device name alone: reuses more, but an offset from another buffer size or output would misplace every take.
- Look up shared room tones at export: avoids copies, but makes an export depend on files outside the project.

## Consequences

A project's own measurement always wins and is never replaced. Bluetooth delay can still change after reconnecting; the panel says so and offers L. `init` copies nothing, since it knows neither the resolved output nor the buffer size.
