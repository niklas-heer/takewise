//! Text normalization and word alignment of a transcript against the script.
//!
//! Both the script line and the transcript go through [`normalize`], so that
//! spoken and written forms compare equal: case, punctuation and hyphens are
//! dropped, contractions expand to one canonical form ("there's" and "there
//! is" both become `there is`; a possessive "link's" becomes `link is` on both
//! sides, which still aligns), and numbers become words. Four-digit numbers
//! from 1100 to 1999 and from 2010 to 2099 written without a thousands
//! separator are read as years, in pairs ("1867" is `eighteen sixty seven`,
//! "1905" `nineteen oh five`, "1900" `nineteen hundred`); 2000 to 2009 are
//! read as "two thousand five", as people say them.
//!
//! [`align`] reports differences between normalized tokens, so a diff names a
//! token such as `kindred`, not the original spelling.

use crate::ledger::{Word, WordDiff, WordDiffKind};

const ONES: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const TENS: [&str; 10] = [
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];

/// Lowercase words with punctuation removed, contractions expanded and
/// numbers spelled out.
pub fn normalize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in raw_tokens(text) {
        let token = raw.trim_matches('\'');
        if token.is_empty() {
            continue;
        }
        if token.chars().any(|c| c.is_ascii_digit()) {
            numeric(token, &mut out);
        } else {
            expand_contraction(token, &mut out);
        }
    }
    out
}

/// Splits lowercased text into runs of letters, digits and apostrophes;
/// `.` and `,` stay inside a run between two digits; `%` and `&` become
/// words of their own.
fn raw_tokens(text: &str) -> Vec<String> {
    let chars: Vec<char> = text
        .chars()
        .map(|c| match c {
            '\u{2019}' | '\u{2018}' | '\u{02bc}' => '\'',
            other => other,
        })
        .flat_map(char::to_lowercase)
        .collect();
    let mut tokens = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let between_digits = || {
            i > 0
                && chars.get(i - 1).is_some_and(char::is_ascii_digit)
                && chars.get(i + 1).is_some_and(char::is_ascii_digit)
        };
        if c.is_alphanumeric() || c == '\'' || ((c == '.' || c == ',') && between_digits()) {
            current.push(c);
            continue;
        }
        if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        match c {
            '%' => tokens.push("percent".to_owned()),
            '&' => tokens.push("and".to_owned()),
            _ => {}
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Contraction endings and the word each stands for.
const SUFFIXES: [(&str, &str); 7] = [
    ("n't", "not"),
    ("'re", "are"),
    ("'ve", "have"),
    ("'ll", "will"),
    ("'m", "am"),
    ("'d", "would"),
    ("'s", "is"),
];

fn expand_contraction(token: &str, out: &mut Vec<String>) {
    let special: Option<&[&str]> = match token {
        "can't" | "cannot" => Some(&["can", "not"]),
        "won't" => Some(&["will", "not"]),
        "shan't" => Some(&["shall", "not"]),
        "let's" => Some(&["let", "us"]),
        _ => None,
    };
    if let Some(words) = special {
        out.extend(words.iter().map(|w| (*w).to_owned()));
        return;
    }
    for (suffix, word) in SUFFIXES {
        if let Some(stem) = token.strip_suffix(suffix)
            && !stem.is_empty()
        {
            out.push(stem.to_owned());
            out.push(word.to_owned());
            return;
        }
    }
    out.push(token.to_owned());
}

/// A token containing a digit: an integer, a decimal, an ordinal such as
/// `21st`, or something else (`mp3`), which stays as it is.
fn numeric(token: &str, out: &mut Vec<String>) {
    let plain: String = token.chars().filter(|&c| c != ',').collect();
    let had_separator = plain.len() != token.len();
    if let Some((whole, fraction)) = plain.split_once('.')
        && let Ok(n) = whole.parse::<u64>()
        && !fraction.is_empty()
        && fraction.chars().all(|c| c.is_ascii_digit())
    {
        cardinal(n, out);
        out.push("point".to_owned());
        for digit in fraction.chars() {
            let idx = digit.to_digit(10).map_or(0, |d| d as usize);
            out.push(ONES.get(idx).copied().unwrap_or("zero").to_owned());
        }
        return;
    }
    if let Ok(n) = plain.parse::<u64>() {
        if !had_separator && plain.len() == 4 && is_year(n) {
            year(n, out);
        } else {
            cardinal(n, out);
        }
        return;
    }
    for suffix in ["st", "nd", "rd", "th"] {
        if let Some(digits) = plain.strip_suffix(suffix)
            && let Ok(n) = digits.parse::<u64>()
        {
            cardinal(n, out);
            if let Some(last) = out.last_mut() {
                *last = ordinal(last);
            }
            return;
        }
    }
    out.push(token.to_owned());
}

fn is_year(n: u64) -> bool {
    (1100..=1999).contains(&n) || (2010..=2099).contains(&n)
}

fn year(n: u64, out: &mut Vec<String>) {
    cardinal(n / 100, out);
    match n % 100 {
        0 => out.push("hundred".to_owned()),
        low @ 1..=9 => {
            out.push("oh".to_owned());
            cardinal(low, out);
        }
        low => cardinal(low, out),
    }
}

fn cardinal(n: u64, out: &mut Vec<String>) {
    if n == 0 {
        out.push("zero".to_owned());
        return;
    }
    let mut rest = n;
    for (scale, name) in [
        (1_000_000_000_000, "trillion"),
        (1_000_000_000, "billion"),
        (1_000_000, "million"),
        (1_000, "thousand"),
    ] {
        if rest >= scale {
            below_thousand_or_more(rest / scale, out);
            out.push(name.to_owned());
            rest %= scale;
        }
    }
    if rest > 0 {
        below_thousand(rest, out);
    }
}

/// The multiplier of a scale word, which may itself exceed 999 for very
/// large numbers.
fn below_thousand_or_more(n: u64, out: &mut Vec<String>) {
    if n >= 1000 {
        cardinal(n, out);
    } else {
        below_thousand(n, out);
    }
}

/// `n` in 1..=999.
fn below_thousand(n: u64, out: &mut Vec<String>) {
    let word = |i: u64, table: &[&'static str]| {
        usize::try_from(i)
            .ok()
            .and_then(|i| table.get(i).copied())
            .unwrap_or_default()
            .to_owned()
    };
    let hundreds = n / 100;
    let rest = n % 100;
    if hundreds > 0 {
        out.push(word(hundreds, &ONES));
        out.push("hundred".to_owned());
    }
    if rest >= 20 {
        out.push(word(rest / 10, &TENS));
        if !rest.is_multiple_of(10) {
            out.push(word(rest % 10, &ONES));
        }
    } else if rest > 0 {
        out.push(word(rest, &ONES));
    }
}

fn ordinal(word: &str) -> String {
    match word {
        "one" => "first".to_owned(),
        "two" => "second".to_owned(),
        "three" => "third".to_owned(),
        "five" => "fifth".to_owned(),
        "eight" => "eighth".to_owned(),
        "nine" => "ninth".to_owned(),
        "twelve" => "twelfth".to_owned(),
        _ => match word.strip_suffix('y') {
            Some(stem) => format!("{stem}ieth"),
            None => format!("{word}th"),
        },
    }
}

/// A normalized heard token with the times of the word it came from.
struct Heard {
    text: String,
    start: f64,
    end: f64,
}

/// Aligns normalized expected tokens with the heard words by minimum edit
/// distance (a substitution costs 1, a missing or extra token 1 each, so a
/// substitution always beats a missing+extra pair). `at` is the heard word's
/// start for extra and substituted tokens; for a missing token it is the end
/// of the heard word before it, or the start of the first heard word when
/// nothing was heard before it, or `None` when nothing was heard at all.
pub fn align(expected: &[String], heard: &[Word]) -> Vec<WordDiff> {
    let heard: Vec<Heard> = heard
        .iter()
        .flat_map(|word| {
            normalize(&word.text).into_iter().map(|text| Heard {
                text,
                start: word.start,
                end: word.end,
            })
        })
        .collect();
    let (n, m) = (expected.len(), heard.len());
    // cost[i][j]: distance between expected[..i] and heard[..j].
    let mut cost = vec![vec![0_usize; m + 1]; n + 1];
    for (i, row) in cost.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in cost[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let same = usize::from(expected[i - 1] != heard[j - 1].text);
            cost[i][j] = (cost[i - 1][j - 1] + same)
                .min(cost[i - 1][j] + 1)
                .min(cost[i][j - 1] + 1);
        }
    }

    let mut diffs = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 {
            let same = expected[i - 1] == heard[j - 1].text;
            if cost[i][j] == cost[i - 1][j - 1] + usize::from(!same) {
                if !same {
                    diffs.push(WordDiff {
                        kind: WordDiffKind::Substituted,
                        expected: Some(expected[i - 1].clone()),
                        heard: Some(heard[j - 1].text.clone()),
                        at: Some(heard[j - 1].start),
                    });
                }
                i -= 1;
                j -= 1;
                continue;
            }
        }
        if i > 0 && cost[i][j] == cost[i - 1][j] + 1 {
            let at = match j.checked_sub(1) {
                Some(before) => Some(heard[before].end),
                None => heard.first().map(|h| h.start),
            };
            diffs.push(WordDiff {
                kind: WordDiffKind::Missing,
                expected: Some(expected[i - 1].clone()),
                heard: None,
                at,
            });
            i -= 1;
        } else {
            diffs.push(WordDiff {
                kind: WordDiffKind::Extra,
                expected: None,
                heard: Some(heard[j - 1].text.clone()),
                at: Some(heard[j - 1].start),
            });
            j -= 1;
        }
    }
    diffs.reverse();
    diffs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::WordDiffKind;

    fn tokens(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    /// Heard words one per second: word i spans i..i+0.5.
    fn heard(text: &str) -> Vec<Word> {
        text.split_whitespace()
            .enumerate()
            .map(|(i, w)| Word {
                text: w.to_owned(),
                start: i as f64,
                end: i as f64 + 0.5,
                confidence: None,
            })
            .collect()
    }

    #[test]
    fn normalizes_contractions() {
        assert_eq!(normalize("There's a link's"), tokens("there is a link is"));
        assert_eq!(
            normalize("there is a link is"),
            normalize("There’s a link’s")
        );
        assert_eq!(
            normalize("I can't, won't, don't"),
            tokens("i can not will not do not")
        );
        assert_eq!(
            normalize("We're, they've, you'll, I'm, she'd"),
            tokens("we are they have you will i am she would")
        );
        assert_eq!(normalize("Let's go"), tokens("let us go"));
        assert_eq!(normalize("cannot"), tokens("can not"));
    }

    #[test]
    fn normalizes_numbers_and_years() {
        assert_eq!(normalize("1867"), tokens("eighteen sixty seven"));
        assert_eq!(normalize("in 1900"), tokens("in nineteen hundred"));
        assert_eq!(normalize("1905"), tokens("nineteen oh five"));
        assert_eq!(normalize("2024"), tokens("twenty twenty four"));
        assert_eq!(normalize("2005"), tokens("two thousand five"));
        assert_eq!(normalize("0"), tokens("zero"));
        assert_eq!(normalize("7 days"), tokens("seven days"));
        assert_eq!(normalize("42"), tokens("forty two"));
        assert_eq!(normalize("100"), tokens("one hundred"));
        assert_eq!(normalize("305"), tokens("three hundred five"));
        assert_eq!(normalize("1,000"), tokens("one thousand"));
        assert_eq!(
            normalize("9999"),
            tokens("nine thousand nine hundred ninety nine")
        );
        assert_eq!(normalize("3.5"), tokens("three point five"));
        assert_eq!(
            normalize("1st 2nd 3rd 21st"),
            tokens("first second third twenty first")
        );
        assert_eq!(normalize("50%"), tokens("fifty percent"));
        // Spoken and written forms agree.
        assert_eq!(normalize("eighteen sixty-seven"), normalize("1867"));
    }

    #[test]
    fn normalizes_punctuation_and_hyphens() {
        assert_eq!(normalize("Open-source, free!"), tokens("open source free"));
        assert_eq!(
            normalize("“Hello” — he said... (twice)"),
            tokens("hello he said twice")
        );
        assert_eq!(normalize("'quoted' dogs'"), tokens("quoted dogs"));
        assert_eq!(
            normalize("A daily quiz; made from records."),
            tokens("a daily quiz made from records")
        );
        assert_eq!(normalize("Rock & roll"), tokens("rock and roll"));
        assert_eq!(normalize("  "), Vec::<String>::new());
    }

    #[test]
    fn align_exact_is_empty() {
        let expected = normalize("Nobody misses a birthday.");
        assert_eq!(align(&expected, &heard("Nobody misses a birthday.")), []);
    }

    #[test]
    fn align_reports_missing_with_time() {
        let expected = normalize("Nobody misses a birthday.");
        let diffs = align(&expected, &heard("Nobody misses birthday."));
        assert_eq!(
            diffs,
            [WordDiff {
                kind: WordDiffKind::Missing,
                expected: Some("a".to_owned()),
                heard: None,
                at: Some(1.5),
            }]
        );
        // A missing first word takes the next heard word's start.
        let diffs = align(&expected, &heard("misses a birthday"));
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].expected.as_deref(), Some("nobody"));
        assert_eq!(diffs[0].at, Some(0.0));
        // Nothing heard at all: no time.
        let diffs = align(&expected, &[]);
        assert_eq!(diffs.len(), 4);
        assert!(
            diffs
                .iter()
                .all(|d| d.kind == WordDiffKind::Missing && d.at.is_none())
        );
    }

    #[test]
    fn align_reports_extra() {
        let expected = normalize("Nobody misses a birthday.");
        let diffs = align(&expected, &heard("Nobody ever misses a birthday."));
        assert_eq!(
            diffs,
            [WordDiff {
                kind: WordDiffKind::Extra,
                expected: None,
                heard: Some("ever".to_owned()),
                at: Some(1.0),
            }]
        );
    }

    #[test]
    fn align_prefers_substitution() {
        let expected = normalize("So I built Kindred around a page.");
        let diffs = align(&expected, &heard("So I built Kindread around a page."));
        assert_eq!(
            diffs,
            [WordDiff {
                kind: WordDiffKind::Substituted,
                expected: Some("kindred".to_owned()),
                heard: Some("kindread".to_owned()),
                at: Some(3.0),
            }]
        );
    }

    #[test]
    fn align_kindred_end_line() {
        let expected =
            normalize("Kindred is free and open source. Try the demo; the link's below.");
        let diffs = align(
            &expected,
            &heard("Kindred is free and open-source, try the demo, the link is below"),
        );
        assert_eq!(diffs, []);
    }

    #[test]
    fn align_splits_heard_words_into_tokens_with_their_times() {
        // One heard word "1867" covers three tokens; a missing token after it
        // is placed at that word's end.
        let expected = normalize("born in 1867 here");
        let diffs = align(&expected, &heard("born in 1867"));
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].expected.as_deref(), Some("here"));
        assert_eq!(diffs[0].at, Some(2.5));
    }
}
