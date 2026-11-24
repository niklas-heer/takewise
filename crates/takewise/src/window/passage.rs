//! Passages in the recording window: the keys with Shift.
//!
//! Shift+Space records from the current line through the end of its
//! passage in one go; the prompter and the current line follow the line
//! being read. Shift+P plays the selected take's passage from its line, and
//! Shift+Enter chooses that passage for every line it has a take of. The
//! recording itself is the recorder's; the passage on disk is
//! `takewise_core::passage`'s.

use gpui_kit::{Context, Window};
use takewise_core::{self as tw, PassageChoice, PassagePiece, RepairReport, TakeRef};

use super::recorder::PassageTakes;
use super::{ChoosePassage, PlayPassage, RecordPassage, RecordingWindow};

impl RecordingWindow {
    /// Shift+Space: records the current line and the lines after it in its
    /// passage as one take, or stops whatever runs, as Space does.
    pub(super) fn record_passage(
        &mut self,
        _: &RecordPassage,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.recorder.is_recording() {
            self.finish_take();
        } else if self.recorder.is_playing() {
            self.recorder.stop_playback(&mut *self.media);
        } else {
            self.begin_take(true);
        }
        cx.notify();
    }

    /// During a passage take, when the line being read moved on: the
    /// prompter starts the new line at the top, and the current line
    /// follows it (so the line list shows it, and the film rests there
    /// after the take). When the take stops, the current line goes to the
    /// line it rests on: the one being read when the narrator stopped it,
    /// or the line it started on when it ran to its end.
    pub(super) fn follow_reading(&mut self) {
        let reading = self
            .recorder
            .passage_reading()
            .map(|(index, id)| (index, id.to_owned()));
        let index = reading.as_ref().map(|(index, _)| *index);
        if index == self.reading {
            return;
        }
        self.reading = index;
        let Some((_, id)) = reading else {
            return;
        };
        self.scroll.reset();
        if let Some(line) = self.project.script.position(&id)
            && line != self.line
        {
            self.line = line;
            self.take = None;
            self.fix_take();
            self.park();
        }
    }

    /// Takes in the takes a passage was cut into: selects the current
    /// line's, and returns the message saying what was recorded.
    pub(super) fn took_passage(&mut self, passage: &PassageTakes) -> String {
        for (take, peak) in &passage.takes {
            if let Some(peak) = peak {
                self.take_peaks
                    .insert((take.line.clone(), take.take), *peak);
            }
        }
        self.reload();
        let mine = passage
            .takes
            .iter()
            .find(|(take, _)| self.line_id() == Some(take.line.as_str()));
        if let Some((take, _)) = mine {
            self.take = Some(take.take);
        }
        let takes: Vec<&TakeRef> = passage.takes.iter().map(|(take, _)| take).collect();
        recorded_note(&takes, &passage.unread)
    }

    /// The passage piece of the selected take, or why there is none.
    fn selected_piece(&self) -> Result<PassagePiece, String> {
        let take = self
            .line_state()
            .and_then(|line| line.takes.iter().find(|t| Some(t.take) == self.take))
            .ok_or("This line has no take yet.")?;
        take.passage.clone().ok_or_else(|| {
            format!(
                "Take {} was recorded on its own, not as part of a passage.",
                take.take
            )
        })
    }

    /// Shift+P: plays the selected take's passage, locked to the film,
    /// from the selected take's line (a pre-roll before it) to the
    /// passage's end; stops a playback that runs.
    pub(super) fn play_passage(&mut self, _: &PlayPassage, _: &mut Window, cx: &mut Context<Self>) {
        cx.notify();
        if self.recorder.is_recording() {
            return;
        }
        if self.recorder.is_playing() {
            self.recorder.stop_playback(&mut *self.media);
            return;
        }
        let started = self.selected_piece().and_then(|piece| {
            self.sync_output()?;
            self.recorder
                .start_passage_playback(&mut *self.media, &self.project, &piece)
        });
        self.message = started.err();
    }

    /// Shift+Enter: chooses the selected take's passage for every line it
    /// has a take of, all at once or not at all.
    pub(super) fn choose_passage(
        &mut self,
        _: &ChoosePassage,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.notify();
        if self.recorder.is_recording() {
            return;
        }
        let chosen = self.selected_piece().and_then(|piece| {
            tw::choose_passage(&self.dir, &piece.id, "narrator")
                .map_err(|err| format!("Cannot choose the passage: {err}"))
        });
        match chosen {
            Ok(choice) => {
                self.reload();
                self.message = Some(chosen_note(&choice));
            }
            Err(err) => self.message = Some(err),
        }
    }
}

/// "a to c", or "a" for one line.
fn span(takes: &[&TakeRef]) -> String {
    match (takes.first(), takes.last()) {
        (Some(first), Some(last)) if takes.len() > 1 => format!("{} to {}", first.line, last.line),
        (Some(only), _) => only.line.clone(),
        _ => String::new(),
    }
}

fn lines(count: usize) -> String {
    if count == 1 {
        "1 line".to_owned()
    } else {
        format!("{count} lines")
    }
}

/// What a passage take recorded: its takes, and the lines it did not reach.
fn recorded_note(takes: &[&TakeRef], unread: &[String]) -> String {
    let recorded = match takes {
        [] => "The passage gave no take.".to_owned(),
        [only] => format!("Recorded a passage: {} take {}.", only.line, only.take),
        _ => format!(
            "Recorded a passage of {} ({}).",
            lines(takes.len()),
            span(takes)
        ),
    };
    if unread.is_empty() {
        return recorded;
    }
    format!(
        "{recorded} No take for {}: not read before the recording stopped.",
        unread.join(", ")
    )
}

/// What Shift+Enter chose, and which lines kept their choice.
fn chosen_note(choice: &PassageChoice) -> String {
    let takes: Vec<&TakeRef> = choice.chosen.iter().collect();
    let chosen = format!(
        "Chose the passage for {} ({}).",
        lines(takes.len()),
        span(&takes)
    );
    if choice.skipped.is_empty() {
        return chosen;
    }
    let skipped: Vec<String> = choice
        .skipped
        .iter()
        .map(|skipped| format!("{} ({})", skipped.line, skipped.reason))
        .collect();
    format!("{chosen} Left as they were: {}.", skipped.join("; "))
}

/// What opening the window recovered from an interrupted recording: the
/// takes adopted and those cut from a passage it finished.
pub(super) fn recovered_note(report: &RepairReport) -> String {
    let takes: Vec<String> = report
        .adopted
        .iter()
        .chain(&report.pieces)
        .map(|take| format!("{} take {}", take.line, take.take))
        .collect();
    if !takes.is_empty() {
        return format!(
            "Recovered {} from an interrupted recording.",
            takes.join(", ")
        );
    }
    let why = report
        .passages_failed
        .first()
        .map(|failed| format!(" ({})", failed.error))
        .unwrap_or_default();
    format!(
        "Recovered a passage from an interrupted recording; it could not be cut into takes yet{why}. Run `takewise status --repair`."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn take(line: &str, take: u32) -> TakeRef {
        TakeRef {
            line: line.to_owned(),
            take,
        }
    }

    #[test]
    fn the_recorded_note_names_the_lines_and_those_not_read() {
        let (a, b, c) = (take("a", 2), take("b", 1), take("c", 1));
        assert_eq!(
            recorded_note(&[&a, &b, &c], &[]),
            "Recorded a passage of 3 lines (a to c)."
        );
        assert_eq!(
            recorded_note(&[&a], &["b".to_owned(), "c".to_owned()]),
            "Recorded a passage: a take 2. No take for b, c: not read before the recording stopped."
        );
    }

    #[test]
    fn the_chosen_note_counts_the_lines_and_names_those_left() {
        let mut choice = PassageChoice {
            passage: "p".to_owned(),
            by: "narrator".to_owned(),
            chosen: vec![take("a", 1), take("b", 3)],
            skipped: Vec::new(),
        };
        assert_eq!(
            chosen_note(&choice),
            "Chose the passage for 2 lines (a to b)."
        );
        choice.skipped.push(tw::passage::SkippedLine {
            line: "c".to_owned(),
            reason: "the passage has no take of this line".to_owned(),
        });
        assert_eq!(
            chosen_note(&choice),
            "Chose the passage for 2 lines (a to b). Left as they were: c (the passage has no take of this line)."
        );
    }

    #[test]
    fn the_recovered_note_lists_adopted_takes_and_cut_pieces() {
        let report = RepairReport {
            adopted: vec![take("a", 1)],
            pieces: vec![take("b", 1), take("c", 2)],
            ..RepairReport::default()
        };
        assert_eq!(
            recovered_note(&report),
            "Recovered a take 1, b take 1, c take 2 from an interrupted recording."
        );
    }
}
