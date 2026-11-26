//! Pointing a project at a new render of its film (`takewise film`): what
//! changed, and what the change means for the script's timings.

use std::path::PathBuf;

use serde::Serialize;

use crate::export::FILM_DURATION_TOLERANCE_S;
use crate::project::{FilmInfo, PreviousFilm};
use crate::script::Script;

/// What [`crate::set_film`] did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FilmChange {
    /// The film's path as `project.toml` holds it now.
    pub film: PathBuf,
    pub film_info: FilmInfo,
    /// False when the project already had this film, measured the same:
    /// then nothing was written.
    pub changed: bool,
    /// The film before, as now recorded in `[previous_film]`; absent when
    /// nothing changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<PreviousFilm>,
    /// What to check before recording or exporting against the new film.
    pub warnings: Vec<FilmWarning>,
}

/// Something about the new film the timings may not fit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FilmWarning {
    /// `film-duration-changed` or `slot-past-film-end`.
    pub code: &'static str,
    /// The line it is about; absent for the film as a whole.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    pub message: String,
}

/// Whether two measurements are of the same film: the same size and
/// modification time, and durations within a millisecond (as `export`
/// compares them).
pub fn same_film(a: &FilmInfo, b: &FilmInfo) -> bool {
    a.size == b.size
        && a.modified == b.modified
        && (a.duration_s - b.duration_s).abs() <= FILM_DURATION_TOLERANCE_S
}

/// What the script's timings meet in a film of `now_s` seconds that was
/// `before_s` long: a duration that changed, and each line whose slot no
/// longer ends within the film (it starts at or after the end, or its
/// slot, explicit or up to the next line's start, runs past it).
pub fn warnings(script: &Script, before_s: f64, now_s: f64) -> Vec<FilmWarning> {
    let mut warnings = Vec::new();
    let delta = now_s - before_s;
    if delta.abs() > FILM_DURATION_TOLERANCE_S {
        warnings.push(FilmWarning {
            code: "film-duration-changed",
            line: None,
            message: format!(
                "the film is {} s long now, was {} s ({:+} s); check the line timings",
                ms(now_s),
                ms(before_s),
                ms(delta)
            ),
        });
    }
    for (idx, line) in script.lines.iter().enumerate() {
        let end = script.slot_end(idx, now_s);
        if line.start >= now_s || end > now_s + FILM_DURATION_TOLERANCE_S {
            warnings.push(FilmWarning {
                code: "slot-past-film-end",
                line: Some(line.id.clone()),
                message: format!(
                    "its slot {}–{} s runs past the film's end at {} s",
                    ms(line.start),
                    ms(end),
                    ms(now_s)
                ),
            });
        }
    }
    warnings
}

/// Seconds to the millisecond, as people read them (`51.5`, not
/// `51.500000001`).
fn ms(seconds: f64) -> f64 {
    (seconds * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::Line;

    fn line(id: &str, start: f64, end: Option<f64>) -> Line {
        Line {
            id: id.to_owned(),
            start,
            end,
            text: "Words.".to_owned(),
            note: None,
        }
    }

    fn codes(warnings: &[FilmWarning]) -> Vec<(&'static str, Option<&str>)> {
        warnings
            .iter()
            .map(|w| (w.code, w.line.as_deref()))
            .collect()
    }

    #[test]
    fn same_duration_and_fitting_slots_warn_of_nothing() {
        let script = Script {
            lines: vec![line("a", 0.0, Some(5.0)), line("b", 10.0, None)],
        };
        assert_eq!(warnings(&script, 51.5, 51.5), []);
        // Within a millisecond is the same duration.
        assert_eq!(warnings(&script, 51.5, 51.5005), []);
        assert_eq!(warnings(&Script::default(), 51.5, 51.5), []);
    }

    #[test]
    fn a_longer_film_warns_of_the_duration_only() {
        let script = Script {
            lines: vec![line("a", 0.0, Some(5.0)), line("b", 10.0, None)],
        };
        let found = warnings(&script, 51.5, 52.5);
        assert_eq!(codes(&found), [("film-duration-changed", None)]);
        assert_eq!(
            found[0].message,
            "the film is 52.5 s long now, was 51.5 s (+1 s); check the line timings"
        );
    }

    #[test]
    fn a_shorter_film_names_the_lines_past_its_end() {
        let script = Script {
            lines: vec![
                line("fits", 0.0, Some(5.0)),
                // Its slot runs to the next line's start, past the end.
                line("open", 30.0, None),
                line("starts-after", 45.0, Some(48.0)),
                line("ends-after", 46.0, Some(50.0)),
                // The last slot without an end runs to the film's end.
                line("last", 47.0, None),
            ],
        };
        let found = warnings(&script, 51.5, 40.0);
        assert_eq!(
            codes(&found),
            [
                ("film-duration-changed", None),
                ("slot-past-film-end", Some("open")),
                ("slot-past-film-end", Some("starts-after")),
                ("slot-past-film-end", Some("ends-after")),
                ("slot-past-film-end", Some("last")),
            ]
        );
        assert_eq!(
            found[1].message,
            "its slot 30–45 s runs past the film's end at 40 s"
        );
        assert!(
            found[0].message.contains("(-11.5 s)"),
            "{}",
            found[0].message
        );
    }

    #[test]
    fn same_film_compares_like_export() {
        let info = FilmInfo {
            duration_s: 51.5,
            size: 10,
            modified: "2026-11-22T20:05:11Z".to_owned(),
        };
        assert!(same_film(&info, &info.clone()));
        let longer = FilmInfo {
            duration_s: 51.6,
            ..info.clone()
        };
        assert!(!same_film(&info, &longer));
        let bigger = FilmInfo {
            size: 11,
            ..info.clone()
        };
        assert!(!same_film(&info, &bigger));
        let later = FilmInfo {
            modified: "2026-11-23T08:00:00Z".to_owned(),
            ..info.clone()
        };
        assert!(!same_film(&info, &later));
    }
}
