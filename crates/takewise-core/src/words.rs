//! Text normalization and word alignment of a transcript against the script.
//!
//! Both the script line and the transcript go through [`normalize`], so that
//! spoken and written forms compare equal: case, punctuation and hyphens are
//! dropped, contractions expand ("don't" is `do not`), and numbers become
//! words. Four-digit numbers from 1100 to 1999 and from 2010 to 2099 written
//! without a thousands separator are read as years, in pairs ("1867" is
//! `eighteen sixty seven`, "1905" `nineteen oh five`, "1900" `nineteen
//! hundred`); 2000 to 2009 are read as "two thousand five", as people say
//! them.
//!
//! An `'s` or a trailing apostrophe is kept as a mark on its [`Token`], since
//! it changes what the token may be heard as. [`align`] accepts a heard
//! `links`, `link is` or `link's` for a script `link's`, `families` for
//! `family's`, and `Niklas` for `Niklas's` or `Niklas'`; a plain script word
//! only accepts its own spelling (with or without a heard apostrophe), so
//! "the light is below" heard as "the lights below" is a difference. Two
//! words on one side match their concatenation on the other ("every day" and
//! "everyday"). Diffs name normalized tokens, such as `kindred` or `link's`,
//! not the original spelling.

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

/// How a token ended before normalization: a script's `link's` keeps that
/// it had an apostrophe, which allows heard forms a plain `links` does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apostrophe {
    /// No apostrophe: `links`.
    None,
    /// An `'s` ending, the text without it: `link's` is `link`.
    S,
    /// A trailing apostrophe, the text without it: `Niklas'` is `niklas`.
    Trailing,
}

/// One normalized word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    pub apostrophe: Apostrophe,
}

impl Token {
    fn plain(text: impl Into<String>) -> Token {
        Token {
            text: text.into(),
            apostrophe: Apostrophe::None,
        }
    }

    /// The token as diffs name it: `link's`, `niklas'`, `kindred`.
    pub fn display(&self) -> String {
        match self.apostrophe {
            Apostrophe::None => self.text.clone(),
            Apostrophe::S => format!("{}'s", self.text),
            Apostrophe::Trailing => format!("{}'", self.text),
        }
    }

    fn is_plain(&self, text: &str) -> bool {
        self.apostrophe == Apostrophe::None && self.text == text
    }

    fn is_plain_word(&self) -> bool {
        self.apostrophe == Apostrophe::None && self.text.chars().all(char::is_alphabetic)
    }
}

/// The plural of a stem ending in a consonant and `y`: `family` is
/// `families`.
fn ies_plural(stem: &str) -> Option<String> {
    let base = stem.strip_suffix('y')?;
    let before = base.chars().last()?;
    (!"aeiou".contains(before)).then(|| format!("{base}ies"))
}

/// Whether a heard token stands for an expected one. Beyond equal tokens, a
/// script token with an apostrophe accepts what it sounds like: `link's` the
/// heard `links`, `family's` `families`, `Niklas's` and `Niklas'` the heard
/// `Niklas`. A plain script token accepts only a heard token spelled the
/// same without its apostrophe (`links` the heard `link's`), never a plural
/// for a separate "is".
fn same(expected: &Token, heard: &Token) -> bool {
    use Apostrophe::{None, S, Trailing};
    let stem = expected.text.as_str();
    match (expected.apostrophe, heard.apostrophe) {
        (a, b) if a == b => expected.text == heard.text,
        (None, S) => expected.text == format!("{}s", heard.text),
        (None | S, Trailing) | (Trailing, None | S) => expected.text == heard.text,
        (S, None) => {
            heard.text == format!("{stem}s")
                || (stem.ends_with('s') && heard.text == stem)
                || ies_plural(stem).is_some_and(|plural| heard.text == plural)
        }
        _ => false,
    }
}

/// Two adjacent tokens heard or written as one: a compound (`every day` and
/// `everyday`), or a contracted "is" (`link is` and `link's`).
fn pair_is_one(first: &Token, second: &Token, one: &Token) -> bool {
    let contracted =
        one.apostrophe == Apostrophe::S && first.is_plain(&one.text) && second.is_plain("is");
    let compound = first.is_plain_word()
        && second.is_plain_word()
        && one.is_plain_word()
        && one.text.len() == first.text.len() + second.text.len()
        && one.text.starts_with(&first.text)
        && one.text.ends_with(&second.text);
    contracted || compound
}

/// Lowercase words with punctuation removed, contractions expanded and
/// numbers spelled out.
pub fn normalize(text: &str) -> Vec<Token> {
    normalize_parts(&[text])
        .into_iter()
        .map(|(token, _, _)| token)
        .collect()
}

/// [`normalize`] over the consecutive parts of one text (the heard words),
/// each token with the first and last part it came from.
fn normalize_parts(parts: &[&str]) -> Vec<(Token, usize, usize)> {
    let mut out = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        for raw in raw_tokens(part) {
            let mut tokens = Vec::new();
            word_tokens(&raw, &mut tokens);
            out.extend(tokens.into_iter().map(|token| (token, index, index)));
        }
    }
    out
}

/// One raw token as normalized tokens.
fn word_tokens(raw: &str, out: &mut Vec<Token>) {
    let token = raw.trim_start_matches('\'');
    let bare = token.trim_end_matches('\'');
    if bare.is_empty() {
        return;
    }
    if bare.chars().any(|c| c.is_ascii_digit()) {
        let mut words = Vec::new();
        numeric(bare, &mut words);
        out.extend(words.into_iter().map(Token::plain));
    } else if bare.len() < token.len() {
        out.push(Token {
            text: bare.to_owned(),
            apostrophe: Apostrophe::Trailing,
        });
    } else {
        expand_contraction(bare, out);
    }
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

/// Contraction endings and the word each stands for. An `'s` is kept as a
/// mark instead, since it may be a possessive.
const SUFFIXES: [(&str, &str); 6] = [
    ("n't", "not"),
    ("'re", "are"),
    ("'ve", "have"),
    ("'ll", "will"),
    ("'m", "am"),
    ("'d", "would"),
];

fn expand_contraction(token: &str, out: &mut Vec<Token>) {
    let special: Option<&[&str]> = match token {
        "can't" | "cannot" => Some(&["can", "not"]),
        "won't" => Some(&["will", "not"]),
        "shan't" => Some(&["shall", "not"]),
        "let's" => Some(&["let", "us"]),
        _ => None,
    };
    if let Some(words) = special {
        out.extend(words.iter().map(|w| Token::plain(*w)));
        return;
    }
    for (suffix, word) in SUFFIXES {
        if let Some(stem) = token.strip_suffix(suffix)
            && !stem.is_empty()
        {
            out.push(Token::plain(stem));
            out.push(Token::plain(word));
            return;
        }
    }
    if let Some(stem) = token.strip_suffix("'s")
        && !stem.is_empty()
    {
        out.push(Token {
            text: stem.to_owned(),
            apostrophe: Apostrophe::S,
        });
        return;
    }
    out.push(Token::plain(token));
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

/// A normalized heard token with the times of the words it came from.
struct Heard {
    token: Token,
    start: f64,
    end: f64,
}

/// Aligns normalized expected tokens with the heard words by minimum edit
/// distance (a substitution costs 1, a missing or extra token 1 each, so a
/// substitution always beats a missing+extra pair; two tokens matching one
/// on the other side cost nothing). `at` is the heard word's start for extra
/// and substituted tokens; for a missing token it is the end of the heard
/// word before it, or the start of the first heard word when nothing was
/// heard before it, or `None` when nothing was heard at all.
pub fn align(expected: &[Token], heard: &[Word]) -> Vec<WordDiff> {
    let texts: Vec<&str> = heard.iter().map(|word| word.text.as_str()).collect();
    let heard: Vec<Heard> = normalize_parts(&texts)
        .into_iter()
        .filter_map(|(token, first, last)| {
            Some(Heard {
                token,
                start: heard.get(first)?.start,
                end: heard.get(last)?.end,
            })
        })
        .collect();
    let (n, m) = (expected.len(), heard.len());
    // Two expected tokens heard as one, ending at expected[i - 1] and
    // heard[j - 1].
    let two_heard_as_one = |i: usize, j: usize| {
        i >= 2 && j >= 1 && pair_is_one(&expected[i - 2], &expected[i - 1], &heard[j - 1].token)
    };
    let one_heard_as_two = |i: usize, j: usize| {
        i >= 1 && j >= 2 && pair_is_one(&heard[j - 2].token, &heard[j - 1].token, &expected[i - 1])
    };
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
            let differs = usize::from(!same(&expected[i - 1], &heard[j - 1].token));
            let mut best = (cost[i - 1][j - 1] + differs)
                .min(cost[i - 1][j] + 1)
                .min(cost[i][j - 1] + 1);
            if two_heard_as_one(i, j) {
                best = best.min(cost[i - 2][j - 1]);
            }
            if one_heard_as_two(i, j) {
                best = best.min(cost[i - 1][j - 2]);
            }
            cost[i][j] = best;
        }
    }

    let mut diffs = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        if two_heard_as_one(i, j) && cost[i][j] == cost[i - 2][j - 1] {
            i -= 2;
            j -= 1;
            continue;
        }
        if one_heard_as_two(i, j) && cost[i][j] == cost[i - 1][j - 2] {
            i -= 1;
            j -= 2;
            continue;
        }
        if i > 0 && j > 0 {
            let matches = same(&expected[i - 1], &heard[j - 1].token);
            if cost[i][j] == cost[i - 1][j - 1] + usize::from(!matches) {
                if !matches {
                    diffs.push(WordDiff {
                        kind: WordDiffKind::Substituted,
                        expected: Some(expected[i - 1].display()),
                        heard: Some(heard[j - 1].token.display()),
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
                expected: Some(expected[i - 1].display()),
                heard: None,
                at,
            });
            i -= 1;
        } else {
            diffs.push(WordDiff {
                kind: WordDiffKind::Extra,
                expected: None,
                heard: Some(heard[j - 1].token.display()),
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

    /// The normalized tokens as diffs name them.
    fn shown(text: &str) -> Vec<String> {
        normalize(text).iter().map(Token::display).collect()
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
        assert_eq!(shown("There's a link's"), tokens("there's a link's"));
        assert_eq!(shown("there's a link's"), shown("There’s a link’s"));
        assert_eq!(shown("Niklas' dogs'"), tokens("niklas' dogs'"));
        assert_eq!(
            shown("I can't, won't, don't"),
            tokens("i can not will not do not")
        );
        assert_eq!(
            shown("We're, they've, you'll, I'm, she'd"),
            tokens("we are they have you will i am she would")
        );
        assert_eq!(shown("Let's go"), tokens("let us go"));
        assert_eq!(shown("cannot"), tokens("can not"));
    }

    #[test]
    fn normalizes_numbers_and_years() {
        assert_eq!(shown("1867"), tokens("eighteen sixty seven"));
        assert_eq!(shown("in 1900"), tokens("in nineteen hundred"));
        assert_eq!(shown("1905"), tokens("nineteen oh five"));
        assert_eq!(shown("2024"), tokens("twenty twenty four"));
        assert_eq!(shown("2005"), tokens("two thousand five"));
        assert_eq!(shown("0"), tokens("zero"));
        assert_eq!(shown("7 days"), tokens("seven days"));
        assert_eq!(shown("42"), tokens("forty two"));
        assert_eq!(shown("100"), tokens("one hundred"));
        assert_eq!(shown("305"), tokens("three hundred five"));
        assert_eq!(shown("1,000"), tokens("one thousand"));
        assert_eq!(
            shown("9999"),
            tokens("nine thousand nine hundred ninety nine")
        );
        assert_eq!(shown("3.5"), tokens("three point five"));
        assert_eq!(
            shown("1st 2nd 3rd 21st"),
            tokens("first second third twenty first")
        );
        assert_eq!(shown("50%"), tokens("fifty percent"));
        // Spoken and written forms agree.
        assert_eq!(shown("eighteen sixty-seven"), shown("1867"));
    }

    #[test]
    fn normalizes_punctuation_and_hyphens() {
        assert_eq!(shown("Open-source, free!"), tokens("open source free"));
        assert_eq!(
            shown("“Hello” — he said... (twice)"),
            tokens("hello he said twice")
        );
        assert_eq!(shown("'quoted' dogs"), tokens("quoted' dogs"));
        assert_eq!(
            shown("A daily quiz; made from records."),
            tokens("a daily quiz made from records")
        );
        assert_eq!(shown("Rock & roll"), tokens("rock and roll"));
        assert_eq!(shown("  "), Vec::<String>::new());
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
    fn align_hears_contracted_is_as_s() {
        // whisper writes the spoken "link's" as "links" and "it's" as "its".
        let expected = normalize("Try the demo; the link's below.");
        assert_eq!(
            align(&expected, &heard("Try the demo, the links below")),
            []
        );
        assert_eq!(
            align(&expected, &heard("Try the demo, the link's below")),
            []
        );
        let expected = normalize("It's free.");
        assert_eq!(align(&expected, &heard("Its free")), []);
        // A heard contraction stands for a written "is".
        let expected = normalize("There is a little fun.");
        assert_eq!(align(&expected, &heard("There's a little fun")), []);
        // A heard apostrophe on a plural that sounds the same.
        let expected = normalize("The links below.");
        assert_eq!(align(&expected, &heard("The link's below")), []);
        // A different word is still a difference.
        let expected = normalize("the link's below");
        let diffs = align(&expected, &heard("the lynx below"));
        assert_ne!(diffs, []);
    }

    #[test]
    fn align_accepts_possessive_forms_only_for_apostrophes() {
        let ok = |script: &str, said: &str| {
            assert_eq!(
                align(&normalize(script), &heard(said)),
                [],
                "{script} / {said}"
            );
        };
        ok("the family's tree", "the families tree");
        ok("the family's tree", "the family's tree");
        ok("Niklas's demo", "Niklas demo");
        ok("Niklas's demo", "Niklas' demo");
        ok("Niklas' demo", "Niklas demo");
        ok("Niklas' demo", "Niklas's demo");
        ok("Niklas demo", "Niklas' demo");
        ok("the link's below", "the links below");
        ok("the link's below", "the link is below");

        // Without an apostrophe in the script, a plural is not a separate
        // "is", nor the reverse.
        let diffs = align(&normalize("the light is below"), &heard("the lights below"));
        assert_ne!(diffs, []);
        let diffs = align(&normalize("the links below"), &heard("the link is below"));
        assert_ne!(diffs, []);
        let diffs = align(&normalize("the family tree"), &heard("the families tree"));
        assert_ne!(diffs, []);
        let diffs = align(&normalize("the link below"), &heard("the links below"));
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].expected.as_deref(), Some("link"));
    }

    #[test]
    fn align_matches_compounds_both_ways() {
        for (script, said) in [
            ("every day", "everyday"),
            ("every one", "everyone"),
            ("open source", "opensource"),
            (
                "Kindred is free and open-source.",
                "Kindred is free and opensource",
            ),
        ] {
            assert_eq!(
                align(&normalize(script), &heard(said)),
                [],
                "{script} / {said}"
            );
            assert_eq!(
                align(&normalize(said), &heard(script)),
                [],
                "{said} / {script}"
            );
        }
        // Only the two words that make it up.
        let diffs = align(&normalize("every day"), &heard("everyone"));
        assert_ne!(diffs, []);
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
