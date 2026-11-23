//! Text normalization and word alignment of a transcript against the script.
//!
//! Both the script line and the transcript go through [`normalize`], so that
//! spoken and written forms compare equal: case, punctuation and hyphens are
//! dropped, contractions expand ("don't" is `do not`), and numbers become
//! values, however they are written or said: "1,500", "1500", "fifteen
//! hundred" and "one thousand five hundred" are all `1500`; "one hundred and
//! five" is `105`, "two thousand and five" `2005`; a year said in two parts
//! ("eighteen sixty seven", "nineteen oh five") is its value; "1990s" and
//! "nineteen nineties" are `1990s`, "21st" and "twenty first" `21st`, "3.5"
//! and "three point five" `3.5`, "$5" and "five dollars" `5 dollars`. A lone
//! whole one stays the word `one`, since it is often not a number ("every
//! one"). A number said over several heard words is one token, timed from
//! its first word to its last.
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
/// `everyday`), a contracted "is" (`link is` and `link's`), or a time read
/// like a year (`11:30` and "eleven thirty", which is `1130`).
fn pair_is_one(first: &Token, second: &Token, one: &Token) -> bool {
    let contracted =
        one.apostrophe == Apostrophe::S && first.is_plain(&one.text) && second.is_plain("is");
    let joined = || {
        one.text.len() == first.text.len() + second.text.len()
            && one.text.starts_with(&first.text)
            && one.text.ends_with(&second.text)
    };
    let compound =
        first.is_plain_word() && second.is_plain_word() && one.is_plain_word() && joined();
    let digits = |token: &Token, lengths: std::ops::RangeInclusive<usize>| {
        token.apostrophe == Apostrophe::None
            && lengths.contains(&token.text.len())
            && token.text.chars().all(|c| c.is_ascii_digit())
    };
    let time = digits(first, 1..=2) && digits(second, 2..=2) && digits(one, 3..=4) && joined();
    contracted || compound || time
}

/// Lowercase words with punctuation removed, contractions expanded and
/// numbers as values.
pub fn normalize(text: &str) -> Vec<Token> {
    normalize_parts(&[text])
        .into_iter()
        .map(|(token, _, _)| token)
        .collect()
}

/// [`normalize`] over the consecutive parts of one text (the heard words),
/// each token with the first and last part it came from: a number said in
/// several heard words is one token.
fn normalize_parts(parts: &[&str]) -> Vec<(Token, usize, usize)> {
    let mut items: Vec<(Item, usize)> = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        for raw in raw_tokens(part) {
            lex(&raw, index, &mut items);
        }
    }
    let words: Vec<Option<&str>> = items
        .iter()
        .map(|(item, _)| match item {
            Item::Word(token) if token.apostrophe == Apostrophe::None => Some(token.text.as_str()),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    let mut k = 0;
    while let Some((item, part)) = items.get(k) {
        if let Some((tokens, next)) = number_phrase(&items, &words, k) {
            let last = items
                .get(next.saturating_sub(1))
                .map_or(*part, |(_, last)| *last);
            out.extend(tokens.into_iter().map(|token| (token, *part, last)));
            k = next.max(k + 1);
            continue;
        }
        if let Item::Word(token) | Item::Other(token) = item {
            out.push((token.clone(), *part, *part));
        }
        k += 1;
    }
    out
}

/// A raw token before numbers are read.
enum Item {
    Word(Token),
    Number(Number),
    /// Has digits but is not a number, such as `mp3`.
    Other(Token),
}

/// A number written with digits.
struct Number {
    value: Value,
    form: Form,
    /// The singular and plural of a currency symbol before it.
    currency: Option<(&'static str, &'static str)>,
}

/// A number's value as written: digits before and after the decimal point.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Value {
    int: u64,
    frac: String,
}

impl Value {
    fn whole(int: u64) -> Value {
        Value {
            int,
            frac: String::new(),
        }
    }

    /// The value times a power of ten, when that is whole and fits: `1.5`
    /// million is 1500000.
    fn scaled(&self, scale: u64) -> Option<Value> {
        let zeros = usize::try_from(scale.checked_ilog10()?).ok()?;
        let pad = zeros.checked_sub(self.frac.len())?;
        let frac = if self.frac.is_empty() {
            0
        } else {
            self.frac.parse::<u64>().ok()?
        };
        let pad = 10_u64.checked_pow(u32::try_from(pad).ok()?)?;
        let int = self
            .int
            .checked_mul(scale)?
            .checked_add(frac.checked_mul(pad)?)?;
        Some(Value::whole(int))
    }

    /// The canonical token: `1867`, `3.5`, `21st`, `1990s`; a lone whole
    /// one stays the word `one` ("every one", "no one").
    fn render(&self, form: Form) -> String {
        match form {
            Form::Cardinal if self.frac.is_empty() && self.int == 1 => "one".to_owned(),
            Form::Cardinal if self.frac.is_empty() => self.int.to_string(),
            Form::Cardinal => format!("{}.{}", self.int, self.frac),
            Form::Ordinal => {
                let suffix = match (self.int % 10, self.int % 100) {
                    (_, 11..=13) => "th",
                    (1, _) => "st",
                    (2, _) => "nd",
                    (3, _) => "rd",
                    _ => "th",
                };
                format!("{}{suffix}", self.int)
            }
            Form::Plural => format!("{}s", self.int),
        }
    }
}

/// How a number is said: `five`, `fifth`, or `fifties` (a decade).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Cardinal,
    Ordinal,
    Plural,
}

/// Where a number word may stand in a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Zero to nine.
    Unit,
    /// Ten to nineteen.
    Teen,
    /// Twenty to ninety.
    Tens,
    Hundred,
    /// Thousand and up.
    Scale,
}

#[derive(Debug, Clone, Copy)]
struct NumberWord {
    value: u64,
    kind: Kind,
    form: Form,
}

const SCALES: [(&str, u64); 5] = [
    ("hundred", 100),
    ("thousand", 1_000),
    ("million", 1_000_000),
    ("billion", 1_000_000_000),
    ("trillion", 1_000_000_000_000),
];

fn number_word(word: &str) -> Option<NumberWord> {
    let found = |value, kind, form| Some(NumberWord { value, kind, form });
    for (value, name) in (0_u64..).zip(ONES) {
        let kind = if value < 10 { Kind::Unit } else { Kind::Teen };
        if word == name {
            return found(value, kind, Form::Cardinal);
        }
        if word == ordinal(name) {
            return found(value, kind, Form::Ordinal);
        }
    }
    for (tens, name) in (0_u64..).zip(TENS).skip(2) {
        let value = tens * 10;
        if word == name {
            return found(value, Kind::Tens, Form::Cardinal);
        }
        if word == ordinal(name) {
            return found(value, Kind::Tens, Form::Ordinal);
        }
        if name
            .strip_suffix('y')
            .is_some_and(|stem| word.strip_suffix("ies") == Some(stem))
        {
            return found(value, Kind::Tens, Form::Plural);
        }
    }
    for (name, value) in SCALES {
        let kind = if value == 100 {
            Kind::Hundred
        } else {
            Kind::Scale
        };
        if word == name {
            return found(value, kind, Form::Cardinal);
        }
        if word == ordinal(name) {
            return found(value, kind, Form::Ordinal);
        }
        if value == 100 && word == "hundreds" {
            return found(value, kind, Form::Plural);
        }
    }
    None
}

fn word_at<'a>(words: &[Option<&'a str>], i: usize) -> Option<&'a str> {
    words.get(i).copied().flatten()
}

fn number_at(words: &[Option<&str>], i: usize) -> Option<NumberWord> {
    word_at(words, i).and_then(number_word)
}

/// A digit said as a word in a decimal: `five`, `zero` or `oh`.
fn digit_at(words: &[Option<&str>], i: usize) -> Option<char> {
    if word_at(words, i) == Some("oh") {
        return Some('0');
    }
    let word = number_at(words, i).filter(|w| w.kind == Kind::Unit && w.form == Form::Cardinal)?;
    char::from_digit(u32::try_from(word.value).ok()?, 10)
}

/// A number from 0 to 99 in words starting at `i`: its value, form and the
/// index after it.
fn small(words: &[Option<&str>], i: usize) -> Option<(u64, Form, usize)> {
    let word = number_at(words, i)?;
    match word.kind {
        Kind::Unit | Kind::Teen => Some((word.value, word.form, i + 1)),
        Kind::Tens => {
            if word.form == Form::Cardinal
                && let Some(unit) = number_at(words, i + 1)
                && unit.kind == Kind::Unit
                && unit.value > 0
            {
                return Some((word.value + unit.value, unit.form, i + 2));
            }
            Some((word.value, word.form, i + 1))
        }
        Kind::Hundred | Kind::Scale => None,
    }
}

/// A number below ten thousand without a scale word: up to 99 hundreds and
/// a rest ("fifteen hundred", "three hundred and five", "a hundred"), or a
/// year said in two parts ("eighteen sixty seven", "nineteen oh five",
/// "nineteen nineties").
fn group(words: &[Option<&str>], i: usize) -> Option<(u64, Form, usize)> {
    let a_hundred = word_at(words, i) == Some("a")
        && number_at(words, i + 1).is_some_and(|w| w.kind == Kind::Hundred);
    let (value, form, j) = if a_hundred {
        (1, Form::Cardinal, i + 1)
    } else {
        small(words, i)?
    };
    if form != Form::Cardinal {
        return Some((value, form, j));
    }
    if let Some(hundred) = number_at(words, j)
        && hundred.kind == Kind::Hundred
        && value > 0
    {
        let hundreds = value * 100;
        if hundred.form != Form::Cardinal {
            return Some((hundreds, hundred.form, j + 1));
        }
        let rest_at = if word_at(words, j + 1) == Some("and") {
            j + 2
        } else {
            j + 1
        };
        if let Some((rest, rest_form, next)) = small(words, rest_at)
            && rest > 0
            && rest_form != Form::Plural
        {
            return Some((hundreds + rest, rest_form, next));
        }
        return Some((hundreds, Form::Cardinal, j + 1));
    }
    // A year in two parts starts with one word from ten to twenty.
    if (10..=20).contains(&value) && j == i + 1 {
        if word_at(words, j) == Some("oh")
            && let Some(unit) = number_at(words, j + 1)
            && unit.kind == Kind::Unit
            && unit.value > 0
        {
            return Some((value * 100 + unit.value, unit.form, j + 2));
        }
        if let Some((low, low_form, next)) = small(words, j)
            && low >= 10
        {
            return Some((value * 100 + low, low_form, next));
        }
    }
    Some((value, form, j))
}

/// A whole number in words starting at `i`: groups joined by falling scale
/// words, as in "two thousand and five" or "a million two hundred
/// thousand". `None` when no number starts there or it does not fit.
fn cardinal(words: &[Option<&str>], i: usize) -> Option<(u64, Form, usize)> {
    let a_scale = word_at(words, i) == Some("a")
        && number_at(words, i + 1).is_some_and(|w| w.kind == Kind::Scale);
    let (mut value, mut form, mut next) = if a_scale {
        (1, Form::Cardinal, i + 1)
    } else {
        group(words, i)?
    };
    let mut total: u64 = 0;
    let mut last_scale = u64::MAX;
    loop {
        let scale = number_at(words, next).filter(|w| {
            w.kind == Kind::Scale && w.value < last_scale && form == Form::Cardinal && value > 0
        });
        let Some(scale) = scale else {
            return Some((total.checked_add(value)?, form, next));
        };
        total = total.checked_add(value.checked_mul(scale.value)?)?;
        last_scale = scale.value;
        next += 1;
        if scale.form != Form::Cardinal {
            return Some((total, scale.form, next));
        }
        let rest_at = if word_at(words, next) == Some("and") {
            next + 1
        } else {
            next
        };
        let larger_scale_after = |at: usize| {
            number_at(words, at).is_some_and(|w| w.kind == Kind::Scale && w.value >= last_scale)
        };
        match group(words, rest_at) {
            Some((rest, rest_form, after)) if !larger_scale_after(after) => {
                value = rest;
                form = rest_form;
                next = after;
            }
            _ => return Some((total, Form::Cardinal, next)),
        }
    }
}

/// The canonical tokens of a number starting at item `k` and the index after
/// it, or `None` when no number starts there.
fn number_phrase(
    items: &[(Item, usize)],
    words: &[Option<&str>],
    k: usize,
) -> Option<(Vec<Token>, usize)> {
    let (value, form, mut next, currency) = match &items.get(k)?.0 {
        Item::Number(number) => (number.value.clone(), number.form, k + 1, number.currency),
        Item::Word(_) => {
            let (int, form, mut next) = cardinal(words, k)?;
            let mut value = Value::whole(int);
            if form == Form::Cardinal && word_at(words, next) == Some("point") {
                let digits: String = (next + 1..).map_while(|i| digit_at(words, i)).collect();
                if !digits.is_empty() {
                    next += 1 + digits.len();
                    value.frac = digits;
                }
            }
            (value, form, next, None)
        }
        Item::Other(_) => return None,
    };
    let mut value = value;
    if form == Form::Cardinal
        && (matches!(items.get(k).map(|(item, _)| item), Some(Item::Number(_)))
            || !value.frac.is_empty())
        && let Some(scale) = number_at(words, next).filter(|w| w.form == Form::Cardinal)
        && matches!(scale.kind, Kind::Hundred | Kind::Scale)
        && let Some(scaled) = value.scaled(scale.value)
    {
        value = scaled;
        next += 1;
    }
    let text = value.render(form);
    let mut tokens = Vec::new();
    if let Some((one, many)) = currency {
        let unit = if text == "one" { one } else { many };
        tokens.push(Token::plain(text));
        tokens.push(Token::plain(unit));
    } else {
        tokens.push(Token::plain(text));
    }
    Some((tokens, next))
}

/// One raw token as items: words, contractions, or a number in digits.
fn lex(raw: &str, part: usize, items: &mut Vec<(Item, usize)>) {
    let token = raw.trim_start_matches('\'');
    let bare = token.trim_end_matches('\'');
    if bare.is_empty() {
        return;
    }
    if bare.chars().any(|c| c.is_ascii_digit()) {
        let item = lex_number(bare).map_or_else(|| Item::Other(Token::plain(bare)), Item::Number);
        items.push((item, part));
    } else if bare.len() < token.len() {
        let token = Token {
            text: bare.to_owned(),
            apostrophe: Apostrophe::Trailing,
        };
        items.push((Item::Word(token), part));
    } else {
        let mut tokens = Vec::new();
        expand_contraction(bare, &mut tokens);
        items.extend(tokens.into_iter().map(|token| (Item::Word(token), part)));
    }
}

/// A number in digits: `1,500`, `3.5`, `21st`, `1990s`, `$5`; `None` for
/// anything else (`mp3`) or a number too large to hold.
fn lex_number(token: &str) -> Option<Number> {
    let mut chars = token.chars();
    let currency = match chars.next() {
        Some('$') => Some(("dollar", "dollars")),
        Some('€') => Some(("euro", "euros")),
        Some('£') => Some(("pound", "pounds")),
        _ => None,
    };
    let rest = if currency.is_some() {
        chars.as_str()
    } else {
        token
    };
    let ends_in_digit = |s: &str| s.ends_with(|c: char| c.is_ascii_digit());
    let ordinal = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|suffix| rest.strip_suffix(suffix).filter(|d| ends_in_digit(d)));
    let plural = rest
        .strip_suffix("'s")
        .or_else(|| rest.strip_suffix('s'))
        .filter(|d| ends_in_digit(d));
    let (digits, form) = match (ordinal, plural) {
        (Some(digits), _) => (digits, Form::Ordinal),
        (None, Some(digits)) => (digits, Form::Plural),
        (None, None) => (rest, Form::Cardinal),
    };
    let plain: String = digits.chars().filter(|&c| c != ',').collect();
    let (whole, frac) = plain.split_once('.').unwrap_or((&plain, ""));
    let all_digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    if whole.is_empty() || !all_digits(whole) || !all_digits(frac) {
        return None;
    }
    if form != Form::Cardinal && !frac.is_empty() {
        return None;
    }
    Some(Number {
        value: Value {
            int: whole.parse().ok()?,
            frac: frac.to_owned(),
        },
        form,
        currency,
    })
}

/// Splits lowercased text into runs of letters, digits and apostrophes;
/// `.` and `,` stay inside a run between two digits, a currency symbol
/// starts a run before a digit; `%` and `&` become words of their own.
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
        if matches!(c, '$' | '€' | '£') && chars.get(i + 1).is_some_and(char::is_ascii_digit) {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
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

/// The ordinal of a number word: `five` is `fifth`, `twenty` `twentieth`.
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
    fn normalizes_numbers_to_values() {
        assert_eq!(shown("1867"), tokens("1867"));
        assert_eq!(shown("eighteen sixty-seven"), tokens("1867"));
        assert_eq!(shown("in nineteen hundred"), tokens("in 1900"));
        assert_eq!(shown("nineteen oh five"), tokens("1905"));
        assert_eq!(shown("twenty twenty four"), tokens("2024"));
        assert_eq!(shown("two thousand and five"), tokens("2005"));
        assert_eq!(shown("zero"), tokens("0"));
        assert_eq!(shown("seven days"), tokens("7 days"));
        assert_eq!(shown("forty two"), tokens("42"));
        assert_eq!(shown("a hundred"), tokens("100"));
        assert_eq!(shown("three hundred and five"), tokens("305"));
        assert_eq!(shown("1,000"), tokens("1000"));
        assert_eq!(
            shown("nine thousand nine hundred ninety nine"),
            tokens("9999")
        );
        assert_eq!(shown("three point five"), tokens("3.5"));
        assert_eq!(
            shown("first second third twenty first"),
            tokens("1st 2nd 3rd 21st")
        );
        assert_eq!(
            shown("1st 2nd 3rd 21st 11th"),
            tokens("1st 2nd 3rd 21st 11th")
        );
        assert_eq!(shown("50%"), tokens("50 percent"));
        assert_eq!(shown("$5 and $1"), tokens("5 dollars and one dollar"));
        assert_eq!(shown("the '90s"), tokens("the 90s"));
        // A lone "one" stays a word, so "every one" can be "everyone".
        assert_eq!(shown("1 and one"), tokens("one and one"));
        // "and" joins only the parts of one number.
        assert_eq!(shown("one and two"), tokens("one and 2"));
        // Not numbers.
        assert_eq!(shown("mp3 a point oh"), tokens("mp3 a point oh"));
        // Too large for a number stays as words.
        assert_eq!(
            shown("ninety nine trillion trillion"),
            tokens("99000000000000 trillion")
        );
        assert_eq!(
            shown("99999999999999999999"),
            tokens("99999999999999999999")
        );
        assert_eq!(
            shown("99999999999 trillion"),
            tokens("99999999999 trillion")
        );
    }

    #[test]
    fn align_matches_numbers_in_any_spelling() {
        for (written, said) in [
            ("1,500", "1500"),
            ("1,500", "fifteen hundred"),
            ("1500", "one thousand five hundred"),
            ("105", "one hundred and five"),
            ("105", "one hundred five"),
            ("2005", "two thousand and five"),
            ("2005", "two thousand five"),
            ("1867", "eighteen sixty seven"),
            ("1867", "eighteen sixty-seven"),
            ("1990", "nineteen ninety"),
            ("1905", "nineteen oh five"),
            ("1500", "fifteen hundred"),
            ("the 1990s", "the nineteen nineties"),
            ("the 1990's", "the nineteen nineties"),
            ("the 90s", "the nineties"),
            ("$5", "five dollars"),
            ("$1", "one dollar"),
            ("$1.5 million", "one point five million dollars"),
            ("5 million", "five million"),
            ("21st", "twenty first"),
            ("3.5", "three point five"),
            ("50%", "fifty percent"),
            ("at 11:30", "at eleven thirty"),
            ("a hundred", "100"),
        ] {
            assert_eq!(
                align(&normalize(written), &heard(said)),
                [],
                "{written} / {said}"
            );
            assert_eq!(
                align(&normalize(said), &heard(written)),
                [],
                "{said} / {written}"
            );
        }
        let diffs = align(&normalize("$5"), &heard("six dollars"));
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].expected.as_deref(), Some("5"));
        assert_eq!(diffs[0].heard.as_deref(), Some("6"));
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
    fn align_spans_numbers_over_heard_words() {
        // "eighteen sixty seven" is one token from three heard words; a
        // missing token after it is placed at the last word's end.
        let expected = normalize("born in 1867 here");
        let diffs = align(&expected, &heard("born in eighteen sixty seven"));
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].expected.as_deref(), Some("here"));
        assert_eq!(diffs[0].at, Some(4.5));
        // A substituted number is placed at its first word.
        let diffs = align(&normalize("in 1867"), &heard("in eighteen sixty eight"));
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].heard.as_deref(), Some("1868"));
        assert_eq!(diffs[0].at, Some(1.0));
    }

    #[test]
    fn align_splits_heard_words_into_tokens_with_their_times() {
        // One heard word "it's" covers two tokens; a missing token after it
        // is placed at that word's end.
        let expected = normalize("so it is here");
        let diffs = align(&expected, &heard("so it's"));
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].expected.as_deref(), Some("here"));
        assert_eq!(diffs[0].at, Some(1.5));
    }
}
