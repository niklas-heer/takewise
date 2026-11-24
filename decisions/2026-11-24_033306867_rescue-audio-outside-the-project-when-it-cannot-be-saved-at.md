+++
schema_version = 1
id = "01M85STJ5K9GV401PWP63KF2R2"
title = "Rescue audio outside the project when it cannot be saved at exit"
date = "2026-11-24"
status = "accepted"
tags = ["recording", "durability"]
supersedes = []
superseded_by = []
depends_on = []
related_to = []
+++
## Context

The recording window keeps every captured sample in memory until its take or passage is committed, and refuses to close, or to quit with ⌘Q, while audio exists only there. A review of milestone 6 (2026-11-24) found two holes. A quit that macOS starts (the Dock's Quit, a logout, a shutdown) is not asked about: GPUI's macOS platform (gpui-pre 0.3.12, pinned by gpui-kit 0.10.0) implements only `applicationWillTerminate`, which runs once termination is underway, and exposes no `applicationShouldTerminate` or other hook that could cancel it; its `on_quit` callback returns a `bool` that the macOS platform ignores. The window then tried once more to save and exited either way. And when the project's disk was gone for good, the refusal left Force Quit, which loses the audio, as the only way out.

## Decision

When the window goes while a take or passage is still only in memory, after one more attempt to save it, it writes that audio to a rescue folder that does not depend on the project's disk: `rescue/<project folder name>-<timestamp>/` beside the settings file (`~/Library/Application Support/Takewise/rescue/`). Each take or passage is a WAV like a take's with a JSON file of the same name that names the project and lines and carries exactly what the project stores beside partial audio, plus the two paths to create in the project. A copy is brought back by hand, by putting those two files in place and running `takewise status --repair`, which adopts them as it adopts a take a crash interrupted. Nothing writes into a project from the rescue folder and nothing removes a copy.

Closing and ⌘Q are still refused once, naming the folder; asked again about the same audio, the window writes the copy and, once it is synced, closes. A copy that cannot be written either keeps the window open. `status` lists rescue folders with audio of the project (`rescued-audio`), and a window opened on the project says so.

## Alternatives

- Veto the quit. Not possible through GPUI or gpui-kit as they are. Adding `applicationShouldTerminate:` to GPUI's delegate class at run time through the Objective-C runtime would work without a fork, but relies on GPUI's internals and can only be tested by hand; it was left out for the maintainer to decide. A veto alone would also still leave Force Quit as the way out of a project whose disk is gone, so the copy is needed either way.
- Keep refusing until saving works. Leaves Force Quit as the only way out when the project's disk does not come back.
- Restore rescue copies automatically when the project opens. More convenient, but it writes takes into a project from outside it, and the manual steps reuse a recovery that is already tested.

## Consequences

Audio is lost at exit only when neither the project nor the rescue folder can be written, which a quit from outside cannot wait out: with the project on the same full disk as the home folder, the Dock's Quit still loses a take that could not be saved. A veto would close that case and is worth revisiting if GPUI gains a should-terminate hook. Rescue folders accumulate until they are deleted by hand, and `status` keeps listing them. A take saved after it got a rescue copy leaves a spare copy, which the window says.
