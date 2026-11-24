//! The script: the lines to speak, their slots, and validation.

use std::collections::HashSet;
use std::ops::Range;

use serde::{Deserialize, Serialize};

/// The lines of `script.toml`, in file order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Script {
    #[serde(rename = "line", default)]
    pub lines: Vec<Line>,
}

/// One line to speak.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub id: String,
    /// Film time in seconds where the line should begin.
    pub start: f64,
    /// End of the slot; defaults to the next line's start or the film's end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<f64>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Script {
    /// The end of line `idx`'s slot: its explicit end, else the next line's
    /// start, else the film's end.
    pub fn slot_end(&self, idx: usize, film_duration: f64) -> f64 {
        let Some(line) = self.lines.get(idx) else {
            return film_duration;
        };
        line.end
            .or_else(|| self.lines.get(idx + 1).map(|next| next.start))
            .unwrap_or(film_duration)
    }

    pub fn position(&self, id: &str) -> Option<usize> {
        self.lines.iter().position(|line| line.id == id)
    }

    /// The script's passages: runs of consecutive lines in which each
    /// line's slot ends less than `gap_s` before the next line starts
    /// (`next.start − slot_end(k) < gap_s`), as index ranges in script
    /// order. Every line is in exactly one; a line with a longer gap on
    /// both sides is a passage of its own.
    pub fn passages(&self, film_duration: f64, gap_s: f64) -> Vec<Range<usize>> {
        let mut passages: Vec<Range<usize>> = Vec::new();
        for (idx, line) in self.lines.iter().enumerate() {
            match passages.last_mut() {
                Some(passage) if line.start - self.slot_end(idx - 1, film_duration) < gap_s => {
                    passage.end = idx + 1;
                }
                _ => passages.push(idx..idx + 1),
            }
        }
        passages
    }

    /// The passage line `idx` belongs to (see [`Script::passages`]).
    pub fn passage_of(&self, idx: usize, film_duration: f64, gap_s: f64) -> Option<Range<usize>> {
        self.passages(film_duration, gap_s)
            .into_iter()
            .find(|passage| passage.contains(&idx))
    }
}

/// A validation finding, reported by `status` and refused by recording.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Problem {
    pub line: Option<String>,
    pub code: &'static str,
    pub message: String,
}

impl Problem {
    pub fn new(line: Option<&str>, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            line: line.map(str::to_owned),
            code,
            message: message.into(),
        }
    }
}

/// Whether `id` matches `^[a-z0-9]+(-[a-z0-9]+)*$`.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// Checks ids, order and slots against the film's duration.
pub fn validate(script: &Script, film_duration: f64) -> Vec<Problem> {
    let mut problems = Vec::new();
    let mut seen = HashSet::new();
    for (idx, line) in script.lines.iter().enumerate() {
        let id = Some(line.id.as_str());
        if !is_valid_id(&line.id) {
            problems.push(Problem::new(
                id,
                "bad-id",
                format!(
                    "line id {:?} must be lowercase letters, digits and single hyphens",
                    line.id
                ),
            ));
        }
        if !seen.insert(line.id.as_str()) {
            problems.push(Problem::new(
                id,
                "duplicate-id",
                format!("line id {:?} is used twice", line.id),
            ));
        }
        if let Some(prev) = idx.checked_sub(1).and_then(|p| script.lines.get(p))
            && line.start.is_finite()
            && prev.start.is_finite()
            && line.start <= prev.start
        {
            problems.push(Problem::new(
                id,
                "unordered",
                format!(
                    "starts at {} s, not after {:?} at {} s",
                    line.start, prev.id, prev.start
                ),
            ));
        }
        if let Some(end) = line.end
            && end <= line.start
        {
            problems.push(Problem::new(
                id,
                "empty-slot",
                format!("ends at {end} s, not after its start at {} s", line.start),
            ));
        }
        let start_outside =
            !line.start.is_finite() || line.start < 0.0 || line.start >= film_duration;
        let end_outside = line
            .end
            .is_some_and(|end| !end.is_finite() || end > film_duration);
        if start_outside || end_outside {
            problems.push(Problem::new(
                id,
                "out-of-film",
                format!(
                    "slot {}–{} s is outside the film (0–{film_duration} s)",
                    line.start,
                    script.slot_end(idx, film_duration)
                ),
            ));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, start: f64, end: Option<f64>) -> Line {
        Line {
            id: id.to_owned(),
            start,
            end,
            text: "Some words.".to_owned(),
            note: None,
        }
    }

    fn codes(problems: &[Problem]) -> Vec<&'static str> {
        problems.iter().map(|p| p.code).collect()
    }

    #[test]
    fn validate_accepts_design_example() {
        let script: Script = toml::from_str(
            r#"
[[line]]
id = "hook"
start = 0.0
end = 5.0
text = "I put our whole family tree together. Everyone looked at it once, and never came back."
note = "warm, a little wry"
"#,
        )
        .unwrap();
        assert_eq!(script.lines.len(), 1);
        assert_eq!(script.lines[0].note.as_deref(), Some("warm, a little wry"));
        assert_eq!(validate(&script, 51.5), []);
    }

    #[test]
    fn integer_times_parse_as_seconds() {
        let script: Script =
            toml::from_str("[[line]]\nid = \"a\"\nstart = 12\ntext = \"x\"\n").unwrap();
        assert!((script.lines[0].start - 12.0).abs() < f64::EPSILON);
    }

    #[test]
    fn validate_flags_each_problem() {
        let cases: Vec<(&str, Script)> = vec![
            (
                "bad-id",
                Script {
                    lines: vec![line("Hook", 0.0, None)],
                },
            ),
            (
                "bad-id",
                Script {
                    lines: vec![line("a--b", 0.0, None)],
                },
            ),
            (
                "bad-id",
                Script {
                    lines: vec![line("", 0.0, None)],
                },
            ),
            (
                "duplicate-id",
                Script {
                    lines: vec![line("a", 0.0, None), line("a", 5.0, None)],
                },
            ),
            (
                "unordered",
                Script {
                    lines: vec![line("a", 5.0, None), line("b", 5.0, None)],
                },
            ),
            (
                "empty-slot",
                Script {
                    lines: vec![line("a", 5.0, Some(5.0))],
                },
            ),
            (
                "out-of-film",
                Script {
                    lines: vec![line("a", 60.0, None)],
                },
            ),
            (
                "out-of-film",
                Script {
                    lines: vec![line("a", -1.0, None)],
                },
            ),
            (
                "out-of-film",
                Script {
                    lines: vec![line("a", 50.0, Some(52.0))],
                },
            ),
            (
                "out-of-film",
                Script {
                    lines: vec![line("a", f64::NAN, None)],
                },
            ),
        ];
        for (code, script) in cases {
            let problems = validate(&script, 51.5);
            assert!(
                codes(&problems).contains(&code),
                "expected {code} for {script:?}, got {problems:?}"
            );
        }
    }

    #[test]
    fn problems_name_their_line() {
        let script = Script {
            lines: vec![line("a", 0.0, None), line("a", 5.0, None)],
        };
        let problems = validate(&script, 51.5);
        assert_eq!(problems[0].line.as_deref(), Some("a"));
    }

    #[test]
    fn passages_join_lines_across_short_gaps() {
        // Sentences 0.1 to 0.7 s apart, then pauses of 1.5 s and 2 s for
        // the picture.
        let script = Script {
            lines: vec![
                line("a", 0.0, Some(3.0)),
                line("b", 3.1, Some(6.0)),
                line("c", 6.7, Some(9.0)),
                line("d", 10.5, Some(12.0)),
                line("e", 14.0, Some(15.0)),
                line("f", 15.0, None),
                line("g", 20.0, Some(22.0)),
            ],
        };
        // A slot without an end runs to the next line, so it joins it.
        assert_eq!(script.passages(51.5, 1.0), vec![0..3, 3..4, 4..7]);
        assert_eq!(script.passage_of(4, 51.5, 1.0), Some(4..7));
        assert_eq!(script.passage_of(3, 51.5, 1.0), Some(3..4));
        assert_eq!(script.passage_of(7, 51.5, 1.0), None);
        // A gap of exactly the limit splits; a wider limit joins more, and
        // none makes every line a passage of its own.
        assert_eq!(script.passages(51.5, 1.5), vec![0..3, 3..4, 4..7]);
        assert_eq!(script.passages(51.5, 1.75), vec![0..4, 4..7]);
        let alone: Vec<_> = (0..7).map(|i| i..i + 1).collect();
        assert_eq!(script.passages(51.5, 0.0), alone);
        assert_eq!(Script::default().passages(51.5, 1.0), vec![]);
    }

    #[test]
    fn slot_end_defaults() {
        let script = Script {
            lines: vec![
                line("a", 0.0, Some(3.0)),
                line("b", 4.0, None),
                line("c", 9.0, None),
            ],
        };
        assert!((script.slot_end(0, 51.5) - 3.0).abs() < f64::EPSILON);
        assert!((script.slot_end(1, 51.5) - 9.0).abs() < f64::EPSILON);
        assert!((script.slot_end(2, 51.5) - 51.5).abs() < f64::EPSILON);
    }
}
