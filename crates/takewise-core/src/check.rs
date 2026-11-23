//! The verdict rules: a `checked` event from a take's analysis and transcript.
//!
//! Every finding becomes a reason, in the order of the rules in
//! `docs/design.md` ("Checks"), so the first reason always explains the
//! verdict, and the verdict is the first matching rule's:
//!
//! 1. no speech → `retake` (`no-speech`)
//! 2. clipping → `retake` (`clipping`)
//! 3. missing or extra words → `retake` (`missing-words`, `extra-words`)
//! 4. overrun beyond 0.25 s or early start beyond 0.3 s → `retake`
//!    (`overrun`, `early-start`)
//! 5. transcription failed → `listen` (`words-not-checked`)
//! 6. only substitutions → `listen` (`possible-mishearing`)
//! 7. a sound whisper annotates inside the speech, such as `(coughs)` or
//!    `[Music]` → `listen` (`non-speech-sound`); outside it is dropped
//! 8. otherwise `keep`, with no reasons.
//!
//! Tolerances are absolute; a short line is held to the same word rules as a
//! long one.

use crate::ledger::{Checked, Reason, Snapshot, Verdict, Word, WordDiff, WordDiffKind};
use crate::speech::Analysis;
use crate::words::{align, is_annotation, normalize};

/// Speech may end this long after the slot before it is an overrun.
pub const OVERRUN_TOLERANCE_S: f64 = 0.25;
/// Speech may start this long before the line before it is an early start.
pub const EARLY_START_TOLERANCE_S: f64 = 0.3;
/// An energy speech start at most this long before the first transcribed
/// word is that word's onset (DTW places words 0.08 to 0.20 s late); an
/// earlier one is a breath before the words.
pub const ONSET_LEAD_S: f64 = 0.25;
/// A reason names at most this many words, then says how many more.
const NAMED_WORDS: usize = 6;

/// What transcription produced for a take.
#[derive(Debug, Clone, PartialEq)]
pub enum Transcript {
    /// Words with times in seconds from the take's first sample.
    Words(Vec<Word>),
    /// Transcription failed or is unavailable; the text says why.
    Failed(String),
}

/// Applies the verdict rules. `line_offset_s` is the film time of the take's
/// first sample minus the line's start (from the `recorded` event), so a time
/// `t` in the take is `line_offset_s + t` in line time. Early start is
/// measured from the energy speech start, unless it lies more than
/// [`ONSET_LEAD_S`] before the first transcribed word (a breath before the
/// words), then from that word; overrun always from the energy speech end.
/// `speech` and the transcript's times stay in seconds from the take's first
/// sample; `overrun_s` is signed (negative when speech ends inside the slot).
pub fn build_checked(
    snapshot: &Snapshot,
    line_offset_s: f64,
    analysis: &Analysis,
    transcript: &Transcript,
    checker: &str,
) -> Checked {
    let (heard, sounds) = split_sounds(transcript, analysis.speech);
    let word_diffs = heard
        .as_deref()
        .map(|spoken| align(&normalize(&snapshot.text), spoken))
        .unwrap_or_default();
    let slot = snapshot.end - snapshot.start;
    let overrun_s = analysis.speech.map(|(_, end)| line_offset_s + end - slot);
    let early_s = speech_onset(analysis, heard.as_deref()).map(|start| -(line_offset_s + start));

    let mut reasons = Vec::new();
    let mut verdict = None;
    let mut add = |reason: Reason, rule_verdict: Verdict| {
        verdict.get_or_insert(rule_verdict);
        reasons.push(reason);
    };
    if analysis.speech.is_none() {
        add(
            reason("no-speech", "no speech detected".to_owned()),
            Verdict::Retake,
        );
    }
    if analysis.clipped_samples > 0 {
        add(
            reason(
                "clipping",
                format!(
                    "{} samples clipped (peak {:.1} dBFS)",
                    analysis.clipped_samples, analysis.peak_dbfs
                ),
            ),
            Verdict::Retake,
        );
    }
    let missing = named(&word_diffs, WordDiffKind::Missing);
    if let Some(names) = missing {
        add(
            reason("missing-words", format!("missing {names}")),
            Verdict::Retake,
        );
    }
    if let Some(names) = named(&word_diffs, WordDiffKind::Extra) {
        add(
            reason("extra-words", format!("extra {names}")),
            Verdict::Retake,
        );
    }
    if let Some(over) = overrun_s.filter(|&s| s > OVERRUN_TOLERANCE_S) {
        add(
            reason("overrun", format!("speech ends {over:.2} s after the slot")),
            Verdict::Retake,
        );
    }
    if let Some(early) = early_s.filter(|&s| s > EARLY_START_TOLERANCE_S) {
        add(
            reason(
                "early-start",
                format!("speech starts {early:.2} s before the line"),
            ),
            Verdict::Retake,
        );
    }
    if let Transcript::Failed(error) = transcript {
        add(
            reason("words-not-checked", format!("words not checked: {error}")),
            Verdict::Listen,
        );
    }
    if let Some(names) = named(&word_diffs, WordDiffKind::Substituted) {
        add(
            reason("possible-mishearing", format!("heard {names}")),
            Verdict::Listen,
        );
    }
    if !sounds.is_empty() {
        add(
            reason(
                "non-speech-sound",
                format!("non-speech sound in the speech: {}", sounds.join(", ")),
            ),
            Verdict::Listen,
        );
    }

    Checked {
        snapshot: snapshot.clone(),
        transcript: heard,
        word_diffs,
        speech: analysis.speech,
        overrun_s,
        peak_dbfs: analysis.peak_dbfs,
        clipped_samples: analysis.clipped_samples,
        noise_floor_dbfs: analysis.noise_floor_dbfs,
        verdict: verdict.unwrap_or(Verdict::Keep),
        reasons,
        checker: checker.to_owned(),
    }
}

/// When the speech starts, in seconds from the take's first sample, for the
/// early-start rule (see [`build_checked`]).
fn speech_onset(analysis: &Analysis, heard: Option<&[Word]>) -> Option<f64> {
    let first_word = heard.and_then(|words| words.iter().map(|w| w.start).min_by(f64::total_cmp));
    let energy_start = analysis.speech.map(|(start, _)| start);
    match (energy_start, first_word) {
        (Some(energy), Some(word)) if word - energy > ONSET_LEAD_S => Some(word),
        (Some(energy), _) => Some(energy),
        (None, word) => word,
    }
}

/// The spoken words of a transcript (`None` when it failed) and the sounds
/// whisper annotated inside the speech, quoted, such as `“(coughs)”`; an
/// annotation counts as inside when its middle lies in the speech range.
fn split_sounds(
    transcript: &Transcript,
    speech: Option<(f64, f64)>,
) -> (Option<Vec<Word>>, Vec<String>) {
    let Transcript::Words(words) = transcript else {
        return (None, Vec::new());
    };
    let (sounds, spoken): (Vec<&Word>, Vec<&Word>) =
        words.iter().partition(|w| is_annotation(&w.text));
    let inside = |sound: &&Word| {
        let middle = f64::midpoint(sound.start, sound.end);
        speech.is_some_and(|(start, end)| (start..=end).contains(&middle))
    };
    let sounds = sounds
        .into_iter()
        .filter(inside)
        .map(|sound| format!("“{}”", sound.text))
        .collect();
    (Some(spoken.into_iter().cloned().collect()), sounds)
}

fn reason(code: &str, message: String) -> Reason {
    Reason {
        code: code.to_owned(),
        message,
    }
}

/// The diffs of one kind as quoted words, such as `“a”, “birthday”`, or
/// `“kisses” for “misses”` for substitutions; `None` when there are none.
fn named(diffs: &[WordDiff], kind: WordDiffKind) -> Option<String> {
    let quote = |word: Option<&String>| format!("“{}”", word.map_or("", String::as_str));
    let names: Vec<String> = diffs
        .iter()
        .filter(|d| d.kind == kind)
        .map(|d| match kind {
            WordDiffKind::Missing => quote(d.expected.as_ref()),
            WordDiffKind::Extra => quote(d.heard.as_ref()),
            WordDiffKind::Substituted => format!(
                "{} for {}",
                quote(d.heard.as_ref()),
                quote(d.expected.as_ref())
            ),
        })
        .collect();
    if names.is_empty() {
        return None;
    }
    let shown = names.len().min(NAMED_WORDS);
    let text = names.get(..shown).unwrap_or_default().join(", ");
    Some(match names.len() - shown {
        0 => text,
        more => format!("{text} and {more} more"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::WordDiffKind;

    const CHECKER: &str = "takewise 0.0.0 whisper ggml-large-v3-turbo.bin";
    const LINE: &str = "Nobody misses a birthday.";
    /// The take starts one second before the line (pre-roll).
    const OFFSET: f64 = -1.0;

    /// A 3 s slot at 10..13 s of film.
    fn snapshot() -> Snapshot {
        Snapshot {
            text: LINE.to_owned(),
            start: 10.0,
            end: 13.0,
        }
    }

    /// Speech from 1.0 s to 3.5 s of the take: 0.0 to 2.5 s of the line.
    fn analysis() -> Analysis {
        Analysis {
            speech: Some((1.0, 3.5)),
            peak_dbfs: -6.0,
            clipped_samples: 0,
            noise_floor_dbfs: -70.0,
        }
    }

    /// `text`'s words spread evenly from `from` to 3.5 s of the take.
    fn words_from(text: &str, from: f64) -> Transcript {
        let words: Vec<&str> = text.split_whitespace().collect();
        let step = (3.5 - from) / words.len() as f64;
        Transcript::Words(
            words
                .iter()
                .enumerate()
                .map(|(i, w)| Word {
                    text: (*w).to_owned(),
                    start: from + i as f64 * step,
                    end: from + (i + 1) as f64 * step,
                    confidence: Some(0.9),
                })
                .collect(),
        )
    }

    fn words(text: &str) -> Transcript {
        words_from(text, 1.0)
    }

    fn check(analysis: &Analysis, transcript: &Transcript) -> Checked {
        build_checked(&snapshot(), OFFSET, analysis, transcript, CHECKER)
    }

    fn codes(checked: &Checked) -> Vec<&str> {
        checked.reasons.iter().map(|r| r.code.as_str()).collect()
    }

    #[test]
    fn rule_no_speech_wins_over_clipping() {
        let analysis = Analysis {
            speech: None,
            clipped_samples: 12,
            ..analysis()
        };
        let checked = check(&analysis, &Transcript::Words(Vec::new()));
        assert_eq!(checked.verdict, Verdict::Retake);
        assert_eq!(codes(&checked)[0], "no-speech");
        assert!(codes(&checked).contains(&"clipping"));
        assert_eq!(checked.overrun_s, None);
    }

    #[test]
    fn rule_clipping() {
        let analysis = Analysis {
            clipped_samples: 3,
            peak_dbfs: 0.0,
            ..analysis()
        };
        let checked = check(&analysis, &words(LINE));
        assert_eq!(checked.verdict, Verdict::Retake);
        assert_eq!(codes(&checked), ["clipping"]);
        assert!(checked.reasons[0].message.contains('3'));
    }

    #[test]
    fn rule_missing_word() {
        let checked = check(&analysis(), &words("Nobody misses birthday"));
        assert_eq!(checked.verdict, Verdict::Retake);
        assert_eq!(codes(&checked), ["missing-words"]);
        assert!(checked.reasons[0].message.contains("“a”"));
        assert_eq!(checked.word_diffs.len(), 1);
        assert_eq!(checked.word_diffs[0].kind, WordDiffKind::Missing);
    }

    #[test]
    fn rule_extra_word() {
        let checked = check(&analysis(), &words("Nobody ever misses a birthday"));
        assert_eq!(checked.verdict, Verdict::Retake);
        assert_eq!(codes(&checked), ["extra-words"]);
        assert!(checked.reasons[0].message.contains("“ever”"));
    }

    #[test]
    fn rule_overrun_beyond_quarter_second() {
        // Speech ends at 4.3 s of the take: 3.3 s into a 3 s slot.
        let analysis = Analysis {
            speech: Some((1.0, 4.3)),
            ..analysis()
        };
        let checked = check(&analysis, &words(LINE));
        assert_eq!(checked.verdict, Verdict::Retake);
        assert_eq!(codes(&checked), ["overrun"]);
        assert!((checked.overrun_s.unwrap() - 0.3).abs() < 1e-9);
        assert!(checked.reasons[0].message.contains("0.30 s"));
    }

    #[test]
    fn rule_overrun_within_tolerance_keeps() {
        let analysis = Analysis {
            speech: Some((1.0, 4.2)),
            ..analysis()
        };
        let checked = check(&analysis, &words(LINE));
        assert_eq!(checked.verdict, Verdict::Keep);
        assert!((checked.overrun_s.unwrap() - 0.2).abs() < 1e-9);
    }

    #[test]
    fn rule_early_start_from_first_word_not_breath() {
        // A breath starts the energy speech 0.5 s before the line; the first
        // word is on time.
        let analysis = Analysis {
            speech: Some((0.5, 3.5)),
            ..analysis()
        };
        let checked = check(&analysis, &words(LINE));
        assert_eq!(checked.verdict, Verdict::Keep, "{:?}", checked.reasons);

        // Without words, the energy start is all there is.
        let checked = check(&analysis, &Transcript::Failed("no model".to_owned()));
        assert_eq!(checked.verdict, Verdict::Retake);
        assert_eq!(codes(&checked), ["early-start", "words-not-checked"]);
        assert!(checked.reasons[0].message.contains("0.50 s"));

        // A first word 0.4 s early is early.
        let checked = check(&analysis, &words_from(LINE, 0.6));
        assert_eq!(checked.verdict, Verdict::Retake);
        assert_eq!(codes(&checked), ["early-start"]);
    }

    #[test]
    fn rule_early_start_from_energy_just_before_first_word() {
        // DTW puts a word 0.08 to 0.20 s after its real onset: the energy
        // start 0.5 s early with the first word 0.22 s after it (0.28 s
        // early, within the tolerance on its own) is the real start.
        let analysis = Analysis {
            speech: Some((0.5, 3.5)),
            ..analysis()
        };
        let checked = check(&analysis, &words_from(LINE, 0.72));
        assert_eq!(checked.verdict, Verdict::Retake, "{:?}", checked.reasons);
        assert_eq!(codes(&checked), ["early-start"]);
        assert!(checked.reasons[0].message.contains("0.50 s"));
    }

    #[test]
    fn rule_failed_transcription_listens() {
        let checked = check(
            &analysis(),
            &Transcript::Failed("whisper-cli not found".to_owned()),
        );
        assert_eq!(checked.verdict, Verdict::Listen);
        assert_eq!(codes(&checked), ["words-not-checked"]);
        assert!(checked.reasons[0].message.contains("whisper-cli not found"));
        assert_eq!(checked.transcript, None);
        assert_eq!(checked.word_diffs, []);
    }

    #[test]
    fn rule_only_substitutions_listen() {
        let checked = check(&analysis(), &words("Nobody kisses a birthday"));
        assert_eq!(checked.verdict, Verdict::Listen);
        assert_eq!(codes(&checked), ["possible-mishearing"]);
        assert!(checked.reasons[0].message.contains("“kisses”"));
        assert!(checked.reasons[0].message.contains("“misses”"));
    }

    #[test]
    fn rule_keep() {
        let transcript = words("Nobody misses a birthday.");
        let checked = check(&analysis(), &transcript);
        assert_eq!(checked.verdict, Verdict::Keep);
        assert_eq!(checked.reasons, []);
        assert_eq!(checked.word_diffs, []);
        assert_eq!(checked.snapshot, snapshot());
        assert_eq!(checked.speech, Some((1.0, 3.5)));
        assert!((checked.overrun_s.unwrap() + 0.5).abs() < 1e-9);
        assert_eq!(checked.peak_dbfs, -6.0);
        assert_eq!(checked.noise_floor_dbfs, -70.0);
        assert_eq!(checked.checker, CHECKER);
        let Transcript::Words(heard) = transcript else {
            unreachable!()
        };
        assert_eq!(checked.transcript, Some(heard));
    }

    /// Words with a time each: (text, start, end) in seconds of the take.
    fn timed(words: &[(&str, f64, f64)]) -> Transcript {
        Transcript::Words(
            words
                .iter()
                .map(|&(text, start, end)| Word {
                    text: text.to_owned(),
                    start,
                    end,
                    confidence: Some(0.9),
                })
                .collect(),
        )
    }

    #[test]
    fn non_speech_sound_inside_the_speech_listens() {
        let transcript = timed(&[
            ("Nobody", 1.0, 1.5),
            ("(coughs)", 1.5, 2.0),
            ("misses", 2.0, 2.5),
            ("a", 2.5, 2.8),
            ("birthday.", 2.8, 3.5),
        ]);
        let checked = check(&analysis(), &transcript);
        assert_eq!(checked.verdict, Verdict::Listen);
        assert_eq!(codes(&checked), ["non-speech-sound"]);
        assert!(
            checked.reasons[0].message.contains("“(coughs)”"),
            "{:?}",
            checked.reasons
        );
        assert_eq!(checked.word_diffs, []);
        let heard: Vec<&str> = checked
            .transcript
            .iter()
            .flatten()
            .map(|w| w.text.as_str())
            .collect();
        assert_eq!(heard, ["Nobody", "misses", "a", "birthday."]);

        // Outside the speech an annotation is dropped.
        let transcript = timed(&[
            ("[BLANK_AUDIO]", 0.0, 0.9),
            ("Nobody", 1.0, 1.5),
            ("misses", 2.0, 2.5),
            ("a", 2.5, 2.8),
            ("birthday.", 2.8, 3.5),
            ("[Music]", 3.6, 4.5),
        ]);
        let checked = check(&analysis(), &transcript);
        assert_eq!(checked.verdict, Verdict::Keep, "{:?}", checked.reasons);
        assert_eq!(checked.transcript.map(|w| w.len()), Some(4));
    }

    #[test]
    fn compound_join_listens() {
        let checked = build_checked(
            &Snapshot {
                text: "Everyone looked at it.".to_owned(),
                ..snapshot()
            },
            OFFSET,
            &analysis(),
            &words("Every one looked at it."),
            CHECKER,
        );
        assert_eq!(checked.verdict, Verdict::Listen);
        assert_eq!(codes(&checked), ["possible-mishearing"]);
        assert!(
            checked.reasons[0]
                .message
                .contains("“every one” for “everyone”"),
            "{:?}",
            checked.reasons
        );
    }

    #[test]
    fn short_line_single_substitution_listens() {
        let checked = check(&analysis(), &words("Nobody misses a birthdays"));
        assert_eq!(checked.verdict, Verdict::Listen);
        let checked = check(&analysis(), &words("Nobody misses birthday"));
        assert_eq!(checked.verdict, Verdict::Retake);
    }
}
