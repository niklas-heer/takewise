+++
schema_version = 1
id = "01M85QSYVC08CY8JSBW9HRV7K7"
title = "Record passages whole and cut them into line takes"
date = "2026-11-24"
status = "proposed"
tags = ["recording", "export"]
supersedes = []
superseded_by = []
depends_on = []
related_to = []
+++
## Context

Niklas narrates a 13-minute vacation film whose narration runs in long passages. Recording one sentence per line, each with its own count-in, lost the intonation joining one sentence to the next, and the export put silence between lines. Long single lines kept the flow but cost a full re-read for one wrong word and tied only the paragraph's first sentence to its shot.

## Decision

Consecutive lines whose slots lie less than `[record] passage_gap_s` (1.0 s) apart form a passage. Shift+Space records a passage in one capture; the recording is published whole in `takes/.passages/<id>.wav` (the commit point) and then cut, in the pause nearest each cue, into one ordinary take per line with a `passage` object. Pieces are checked, ranked and chosen like any take, with timing rules moved to the passage's edges; a single line is still retaken with Space. The export joins consecutive pieces of one passage that are still placed as recorded into one stretch, trimmed and faded only at its outer edges and cleaned as one file, so a passage chosen whole exports as it was read.

## Alternatives

- Paragraph-sized lines only: no code, but no pickups and no per-sentence anchoring.
- One take spanning several lines as a new kind of take: every check, choice and export rule would need a second path.
- Cutting with whisper word alignment: more precise attribution, but makes the commit depend on transcription.

## Consequences

Takes stay per line, so status, checks, choices and agents work unchanged. Cuts follow the cues within about a second; a read further off the cues splits words between neighbouring pieces, which the word check flags, while the passage file and a passage chosen whole stay intact. A 13-minute `clean` export peaks near 1.8 GB of memory. Council review: design rounds and code rounds recorded in `docs/plans/2026-11-24-m6-passages-and-remembered-setup.md`.
