+++
schema_version = 1
id = "01M82T7GS4FBTXSEV97TDHWTPY"
title = "Offer optional, low-latency monitoring"
date = "2026-11-22"
status = "accepted"
tags = ["audio", "ux"]
supersedes = []
superseded_by = []
depends_on = []
related_to = []
+++
## Decision

Takewise offers optional software monitoring of the narrator's voice. It is off by default and toggled with one key, uses the smallest buffer the output device allows, shows the resulting delay, has its own volume, and recommends the interface's hardware monitoring when there is one. Monitoring never changes what is recorded.

Accepted by Niklas on 2026-11-23: "Don't forget that some may want to hear their own voice and some may not", with the volume changeable during a session.

## Context

The original design ruled monitoring out entirely, because Resolve's delayed playback of his own voice was "super irritating" during the Kindred narration. Some narrators prefer hearing themselves, which helps with level and diction; the irritation came from delay and from not being able to turn it off.

## Consequences

- The anti-goal "never plays the mic input back" becomes "never monitors without being asked".
- A duplex audio path (input to output) joins the media crate, separate from capture so that recording is unaffected.
