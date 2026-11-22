+++
schema_version = 1
id = "01M82K7NZQJHK49C08VX3CRB34"
title = "Build Takewise in Rust with GPUI"
date = "2026-11-22"
status = "accepted"
tags = ["architecture", "stack"]
supersedes = []
superseded_by = []
depends_on = []
related_to = []
+++
## Decision

Build Takewise as a Rust application with GPUI through `gpui-kit`, reaching AVFoundation and CoreAudio through `objc2` for film playback and audio capture. The CLI, the checks and the app share one Cargo workspace. `unsafe` code is denied everywhere except the `takewise-media` crate, which holds the macOS bridges.

Accepted by Niklas on 2026-11-22, choosing GPUI and gpui-kit over a Swift app after the council (Claude Fable 5.2 and GPT Sol 6.2) had recommended Swift for its easier media path.

## Context

Takewise needs video playback in sync with audio capture, a focused recording screen and a CLI for agents.

Alternatives considered:

- **A Swift app (AppKit or SwiftUI) with a Rust CLI.** AVPlayerView and AVAudioEngine make the media side easiest. It adds a second language beside the Rust CLI, and Niklas has had stability trouble with SwiftUI.
- **Tauri or a web page.** Quick to build, but browser capture adds processing and opaque latency, and playback sync is harder to trust.

Niklas already ships Focal with GPUI, gpui-kit and `objc2` bridges, so the stack, lints and packaging are known.

## Consequences

- Neither GPUI nor gpui-kit has a video player. Showing the film inside a GPUI window is the main technical risk; a throwaway spike (milestone 0 in `docs/design.md`) settles the route before the recording window is built.
- One language for the app, the CLI and the checks; agents and tests use the same core crate.
