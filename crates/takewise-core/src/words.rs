//! Text normalization and word alignment of a transcript against the script.
//!
//! Both the script line and the transcript go through [`normalize`], so that
//! spoken and written forms compare equal: case, punctuation and hyphens are
//! dropped, contractions expand ("don't" is `do not`), and numbers become
//! values, however they are written or said: "1,500", "1500", "fifteen
//! hundred" and "one thousand five hundred" are all `1500`; "one hundred and
//! five" is `105`, "two thousand and five" `2005`; a year said in two parts
//! ("eighteen sixty seven", "nineteen oh five") is its value; "1990s" and
//! "nineteen nineties" are `1990s`, "21st" and "twenty-first" `21st`, "3.5"
//! and "three point five zero" `3.5` (a decimal is its value), "$5" and
//! "five dollars" `5 dollars`; a sign is kept, so "-5", "minus five" and
//! "negative five" are `-5`, never `5`. A lone whole one stays the word `one`, since
//! it is often not a number ("every one"), and "first" and "second" are
//! ordinals only when attached ("twenty-first"), never in "a sixty second
//! timer". A number said over
//! several heard words is one token, timed from its first word to its last,
//! but never across punctuation: "pick twenty, one at a time" is `20 one`.
//! [`align`] still matches a number split in two on one side against the
//! joined form on the other (`60 second` and `62nd`, `20 first` and `21st`),
//! and a script clock time such as `11:30` against "eleven thirty" or `11
//! 30`; without a colon in the script, two numbers are never a time.
//!
//! An `'s` or a trailing apostrophe is kept as a mark on its [`Token`], since
//! it changes what the token may be heard as. [`align`] accepts a heard
//! `links`, `link is` or `link's` for a script `link's`, `families` for
//! `family's`, and `Niklas` for `Niklas's` or `Niklas'`; a plain script word
//! only accepts its own spelling (with or without a heard apostrophe), so
//! "the light is below" heard as "the lights below" is a difference. Two
//! words on one side against their concatenation on the other ("every day"
//! and "everyday", "every one" and "everyone") are one substitution, a
//! possible mishearing, since they often mean different things; joined by a
//! hyphen they are one word spelled two ways ("voice-over" and "voiceover",
//! "e-mail" and "email") and match. Diffs name
//! the words as written in the script or said in the transcript, lowercased
//! and without punctuation: `second` (not `2nd`), `1,867`, `eighteen
//! sixty-eight`, `link's`.

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
    /// Punctuation such as a comma stands between this token and the one
    /// before it, so the two are never one number.
    after_break: bool,
    /// A hyphen joins this token to the one before it: "voice-over".
    after_hyphen: bool,
    /// The words as written or said, lowercased and without punctuation,
    /// which diffs name: `second`, `1,867`, `eighteen sixty-eight`, `$5`.
    written: String,
}

impl Token {
    fn plain(text: impl Into<String>) -> Token {
        Token::new(text, Apostrophe::None)
    }

    fn new(text: impl Into<String>, apostrophe: Apostrophe) -> Token {
        let mut token = Token {
            text: text.into(),
            apostrophe,
            after_break: false,
            after_hyphen: false,
            written: String::new(),
        };
        token.written = token.display();
        token
    }

    /// The normalized token: `link's`, `niklas'`, `kindred`, `1867`.
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
    if let Some((hours, minutes)) = clock(&expected.text)
        && heard.apostrophe == None
        && heard.text == format!("{hours}{minutes:02}")
    {
        return true;
    }
    if let (Some(a), Some(b)) = (whole_number(expected), whole_number(heard))
        && a == b
    {
        return true;
    }
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

/// A clock time with a colon, such as `11:30`: its hours and minutes.
fn clock(text: &str) -> Option<(u64, u64)> {
    let (hours, minutes) = text.split_once(':')?;
    let digits = |s: &str, lengths: std::ops::RangeInclusive<usize>| {
        lengths.contains(&s.len()) && s.chars().all(|c| c.is_ascii_digit())
    };
    if !digits(hours, 1..=2) || !digits(minutes, 2..=2) {
        return None;
    }
    let (hours, minutes) = (hours.parse().ok()?, minutes.parse().ok()?);
    (hours <= 23 && minutes <= 59).then_some((hours, minutes))
}

/// A whole number token's value and form: `21` and `one` are cardinals,
/// `21st`, `first` and `second` ordinals, `1990s` a plural.
fn whole_number(token: &Token) -> Option<(u64, Form)> {
    if token.apostrophe != Apostrophe::None {
        return None;
    }
    let text = token.text.as_str();
    let parse = |digits: &str| -> Option<u64> {
        let all = !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit());
        all.then(|| digits.parse().ok()).flatten()
    };
    match text {
        "one" => return Some((1, Form::Cardinal)),
        "first" => return Some((1, Form::Ordinal)),
        "second" => return Some((2, Form::Ordinal)),
        _ => {}
    }
    if let Some(value) = parse(text) {
        return Some((value, Form::Cardinal));
    }
    if let Some(value) = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|suffix| text.strip_suffix(suffix).and_then(parse))
    {
        return Some((value, Form::Ordinal));
    }
    text.strip_suffix('s')
        .and_then(parse)
        .map(|value| (value, Form::Plural))
}

/// How two adjacent tokens on one side match one token on the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Join {
    /// The same words, written differently: a contracted "is" (`link is`
    /// and `link's`), a number in two parts (`sixty second` and `62nd`, `20
    /// first` and `21st`), or a script clock time (`11:30` heard as `11 30`
    /// or "nine thirty" for `9:30`).
    Same,
    /// Two separate words heard or written as one (`every day` and
    /// `everyday`): a possible mishearing.
    Compound,
}

/// Whether `first` and `second` are `one` on the other side; `one_is_script`
/// says that `one` is the script's token, which a clock time must be.
fn pair_is_one(first: &Token, second: &Token, one: &Token, one_is_script: bool) -> Option<Join> {
    let contracted =
        one.apostrophe == Apostrophe::S && first.is_plain(&one.text) && second.is_plain("is");
    if contracted {
        return Some(Join::Same);
    }
    let joined = one.text.len() == first.text.len() + second.text.len()
        && one.text.starts_with(&first.text)
        && one.text.ends_with(&second.text);
    if first.is_plain_word() && second.is_plain_word() && one.is_plain_word() && joined {
        // A hyphenated pair is one word spelled two ways ("voice-over",
        // "voiceover"); an open pair may mean something else ("every one").
        return Some(if second.after_hyphen {
            Join::Same
        } else {
            Join::Compound
        });
    }
    if second.after_break {
        return None;
    }
    let (Some((a, a_form)), Some((b, b_form))) = (whole_number(first), whole_number(second)) else {
        return None;
    };
    if one_is_script
        && let Some((hours, minutes)) = clock(&one.text)
        && a_form == Form::Cardinal
        && b_form == Form::Cardinal
        && (a, b) == (hours, minutes)
        && minutes >= 10
    {
        return Some(Join::Same);
    }
    let split = a_form == Form::Cardinal
        && a >= 20
        && a % 10 == 0
        && (1..=9).contains(&b)
        && b_form != Form::Plural
        && whole_number(one) == Some((a + b, b_form));
    split.then_some(Join::Same)
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
    let mut items: Vec<Lexed> = Vec::new();
    let mut pending = Sep::Space;
    for (index, part) in parts.iter().enumerate() {
        let (raws, trailing) = raw_tokens(part);
        for (raw, sep) in raws {
            lex(&raw, index, pending.max(sep), &mut items);
            pending = Sep::Space;
        }
        pending = pending.max(trailing);
    }
    let words = number_words(&items);
    let mut out = Vec::new();
    let mut k = 0;
    while let Some(lexed) = items.get(k) {
        // A number ends at punctuation.
        let end = (k + 1..items.len())
            .find(|&i| items[i].sep == Sep::Break)
            .unwrap_or(items.len());
        let after_break = lexed.sep == Sep::Break;
        let after_hyphen = lexed.sep == Sep::Hyphen;
        if let Some((tokens, next)) = number_phrase(&items[..end], &words[..end], k) {
            let last = items
                .get(next.saturating_sub(1))
                .map_or(lexed.part, |l| l.part);
            let mut written = String::new();
            for (n, said) in items.get(k..next).unwrap_or_default().iter().enumerate() {
                if n > 0 {
                    written.push(if said.sep == Sep::Hyphen { '-' } else { ' ' });
                }
                written.push_str(&said.raw);
            }
            // A currency's unit token keeps its own name.
            out.extend(tokens.into_iter().enumerate().map(|(n, token)| {
                let written = if n == 0 {
                    written.clone()
                } else {
                    token.written
                };
                let token = Token {
                    after_break,
                    after_hyphen,
                    written,
                    ..token
                };
                (token, lexed.part, last)
            }));
            k = next.max(k + 1);
            continue;
        }
        if let Item::Word(token) | Item::Other(token) = &lexed.item {
            let token = Token {
                after_break,
                after_hyphen,
                ..token.clone()
            };
            out.push((token, lexed.part, lexed.part));
        }
        k += 1;
    }
    out
}

/// The plain words that may be part of a number, by item. "first" and
/// "second" are only ordinals when attached to a number word before them
/// ("twenty-first"); alone they are words ("a sixty second timer", "first
/// we go").
fn number_words(items: &[Lexed]) -> Vec<Option<&str>> {
    fn plain(lexed: &Lexed) -> Option<&str> {
        match &lexed.item {
            Item::Word(token) if token.apostrophe == Apostrophe::None => Some(token.text.as_str()),
            _ => None,
        }
    }
    items
        .iter()
        .enumerate()
        .map(|(k, lexed)| {
            let word = plain(lexed)?;
            let attached = lexed.sep == Sep::Hyphen
                && k.checked_sub(1)
                    .and_then(|before| items.get(before))
                    .and_then(plain)
                    .is_some_and(|before| number_word(before).is_some());
            let loose = matches!(word, "first" | "second") && !attached;
            (!loose).then_some(word)
        })
        .collect()
}

/// What stands between a raw token and the one before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Sep {
    /// Space, or nothing.
    Space,
    /// A hyphen right between two words: "twenty-first".
    Hyphen,
    /// Punctuation such as a comma, a full stop or a dash.
    Break,
}

/// A raw token read: what it is, the heard word it came from and what
/// stands before it.
struct Lexed {
    item: Item,
    part: usize,
    sep: Sep,
    /// As written: `1,500`, `$5`, `twenty`, `link's`.
    raw: String,
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
    negative: bool,
    int: u64,
    frac: String,
}

impl Value {
    fn whole(int: u64) -> Value {
        Value {
            negative: false,
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
        Some(Value {
            negative: self.negative,
            ..Value::whole(int)
        })
    }

    /// The canonical token: `1867`, `-5`, `3.5`, `21st`, `1990s`; a lone
    /// whole one stays the word `one` ("every one", "no one").
    fn render(&self, form: Form) -> String {
        let sign = if self.negative { "-" } else { "" };
        match form {
            Form::Cardinal if self.frac.is_empty() && self.int == 1 && !self.negative => {
                "one".to_owned()
            }
            Form::Cardinal if self.frac.is_empty() => format!("{sign}{}", self.int),
            Form::Cardinal => format!("{sign}{}.{}", self.int, self.frac),
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
fn number_phrase(items: &[Lexed], words: &[Option<&str>], k: usize) -> Option<(Vec<Token>, usize)> {
    // "minus five" and "negative five" are `-5`.
    if matches!(word_at(words, k), Some("minus" | "negative"))
        && items.get(k + 1).is_some_and(|l| l.sep != Sep::Break)
        && let Some((mut tokens, next)) = number_phrase(items, words, k + 1)
        && let Some(first) = tokens.first_mut()
        && let Some(negated) = negate(&first.text)
    {
        first.text = negated;
        return Some((tokens, next));
    }
    let (value, form, mut next, currency) = match &items.get(k)?.item {
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
    // A decimal compares by value: "3.50" is "three point five".
    value.frac.truncate(value.frac.trim_end_matches('0').len());
    if form == Form::Cardinal
        && (matches!(items.get(k).map(|l| &l.item), Some(Item::Number(_)))
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

/// A cardinal token with a minus sign: `5` is `-5`, `one` `-1`; `None` for
/// one that has a sign already or is no cardinal.
fn negate(text: &str) -> Option<String> {
    if text == "one" {
        return Some("-1".to_owned());
    }
    let cardinal = text.starts_with(|c: char| c.is_ascii_digit())
        && text.chars().all(|c| c.is_ascii_digit() || c == '.');
    cardinal.then(|| format!("-{text}"))
}

/// One raw token as items: words, contractions, or a number in digits. A
/// clock time such as `11:30` stays one token; other digits around a colon
/// are separate numbers.
fn lex(raw: &str, part: usize, sep: Sep, items: &mut Vec<Lexed>) {
    let token = raw.trim_start_matches('\'');
    let bare = token.trim_end_matches('\'');
    if bare.is_empty() {
        return;
    }
    let mut push = |item: Item, sep: Sep, raw: String| {
        items.push(Lexed {
            item,
            part,
            sep,
            raw,
        });
    };
    if bare.contains(':') && clock(bare).is_none() {
        for (n, piece) in bare.split(':').enumerate() {
            lex(piece, part, if n == 0 { sep } else { Sep::Break }, items);
        }
    } else if bare.chars().any(|c| c.is_ascii_digit()) {
        let item = lex_number(bare).map_or_else(|| Item::Other(Token::plain(bare)), Item::Number);
        push(item, sep, bare.to_owned());
    } else if bare.len() < token.len() {
        let token = Token::new(bare, Apostrophe::Trailing);
        let raw = token.written.clone();
        push(Item::Word(token), sep, raw);
    } else {
        let mut tokens = Vec::new();
        expand_contraction(bare, &mut tokens);
        for (n, token) in tokens.into_iter().enumerate() {
            let raw = token.written.clone();
            push(
                Item::Word(token),
                if n == 0 { sep } else { Sep::Space },
                raw,
            );
        }
    }
}

/// A number in digits: `1,500`, `-5`, `3.5`, `21st`, `1990s`, `$5`; `None`
/// for anything else (`mp3`) or a number too large to hold.
fn lex_number(token: &str) -> Option<Number> {
    let (negative, token) = match token.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, token),
    };
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
    if form != Form::Cardinal && (!frac.is_empty() || negative) {
        return None;
    }
    Some(Number {
        value: Value {
            negative,
            int: whole.parse().ok()?,
            frac: frac.to_owned(),
        },
        form,
        currency,
    })
}

/// Splits lowercased text into runs of letters, digits and apostrophes,
/// each with what stands before it, and what follows the last run; `.`, `,`
/// and `:` stay inside a run between two digits, a currency symbol starts a
/// run before a digit; `%` and `&` become words of their own.
fn raw_tokens(text: &str) -> (Vec<(String, Sep)>, Sep) {
    let chars: Vec<char> = text
        .chars()
        .map(|c| match c {
            '\u{2019}' | '\u{2018}' | '\u{02bc}' => '\'',
            '\u{2212}' => '-',
            other => other,
        })
        .flat_map(char::to_lowercase)
        .collect();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut sep = Sep::Space;
    let mut flush = |current: &mut String, sep: &mut Sep| {
        if !current.is_empty() {
            tokens.push((std::mem::take(current), *sep));
            *sep = Sep::Space;
        }
    };
    for (i, &c) in chars.iter().enumerate() {
        let before = i.checked_sub(1).and_then(|before| chars.get(before));
        let after = chars.get(i + 1);
        let between_digits =
            || before.is_some_and(char::is_ascii_digit) && after.is_some_and(char::is_ascii_digit);
        if c.is_alphanumeric() || c == '\'' || (matches!(c, '.' | ',' | ':') && between_digits()) {
            current.push(c);
            continue;
        }
        // A minus sign starts a number: "-5", but not "10-15".
        let sign = c == '-' && !before.is_some_and(|b| b.is_alphanumeric());
        if (sign || matches!(c, '$' | '€' | '£')) && after.is_some_and(char::is_ascii_digit) {
            flush(&mut current, &mut sep);
            current.push(c);
            continue;
        }
        flush(&mut current, &mut sep);
        match c {
            '%' => current.push_str("percent"),
            '&' => current.push_str("and"),
            '-' if before.is_some_and(|b| b.is_alphanumeric())
                && after.is_some_and(|a| a.is_alphanumeric()) =>
            {
                sep = sep.max(Sep::Hyphen);
            }
            c if c.is_whitespace() || c == '-' => {}
            _ => sep = Sep::Break,
        }
        flush(&mut current, &mut sep);
    }
    flush(&mut current, &mut sep);
    (tokens, sep)
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
        out.push(Token::new(stem, Apostrophe::S));
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

/// Removes the bracketed or parenthesized annotations whisper writes for
/// sounds, such as `[BLANK_AUDIO]`, `(sighs)` or `[Music]`, before
/// alignment; an annotation may span several words. Words keep their times.
/// When a bracket is never closed, nothing is removed.
pub fn drop_annotations(words: Vec<Word>) -> Vec<Word> {
    isolate_annotations(words)
        .into_iter()
        .filter(|word| !is_annotation(&word.text))
        .collect()
}

/// Whether a word is an annotation for a sound, such as `(coughs)`.
pub fn is_annotation(text: &str) -> bool {
    text.starts_with(['[', '(']) && text.ends_with([']', ')'])
}

/// Makes each annotation whisper writes for a sound a word of its own,
/// such as `(clears throat)` from the words `(clears` and `throat)`, timed
/// from the start of its first word to the end of its last; the spoken rest
/// of a word stays a word with that word's times, before the annotations
/// that close in it. When a bracket is never closed, nothing changes.
pub fn isolate_annotations(words: Vec<Word>) -> Vec<Word> {
    let mut depth = 0_usize;
    let mut note: Option<Word> = None;
    let mut out = Vec::new();
    for word in &words {
        let mut text = String::new();
        // A word inside an open annotation belongs to it, brackets or not.
        let mut bracketed = note.is_some();
        let mut closed = Vec::new();
        if let Some(note) = note.as_mut() {
            note.text.push(' ');
        }
        for c in word.text.chars() {
            match c {
                '[' | '(' => {
                    depth += 1;
                    bracketed = true;
                    note.get_or_insert_with(|| Word {
                        text: String::new(),
                        ..word.clone()
                    })
                    .text
                    .push(c);
                }
                ']' | ')' => {
                    bracketed = true;
                    if let Some(open) = note.as_mut() {
                        open.text.push(c);
                        depth -= 1;
                        if depth == 0
                            && let Some(mut done) = note.take()
                        {
                            done.end = word.end;
                            done.text = tidy_annotation(&done.text);
                            closed.push(done);
                        }
                    }
                }
                _ => match note.as_mut() {
                    Some(open) => open.text.push(c),
                    None => text.push(c),
                },
            }
        }
        if !bracketed {
            out.push(word.clone());
        } else if text.chars().any(char::is_alphanumeric) {
            out.push(Word {
                text: text.trim().to_owned(),
                ..word.clone()
            });
        }
        out.extend(closed);
    }
    if depth > 0 {
        return words;
    }
    out
}

/// An annotation's words with single spaces and none inside its brackets:
/// `[ soft  music ]` is `[soft music]`.
fn tidy_annotation(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(joined.len());
    for c in joined.chars() {
        if matches!(c, ']' | ')') && out.ends_with(' ') {
            out.pop();
        }
        if c == ' ' && out.ends_with(['[', '(']) {
            continue;
        }
        out.push(c);
    }
    out
}

/// A normalized heard token with the times of the words it came from.
struct Heard {
    token: Token,
    start: f64,
    end: f64,
}

/// Aligns normalized expected tokens with the heard words by minimum edit
/// distance (a substitution costs 1, a missing or extra token 1 each, so a
/// substitution always beats a missing+extra pair; two tokens that are one
/// on the other side cost nothing, or a substitution for a compound, which
/// is reported as one). `at` is the heard word's start for extra
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
    Alignment {
        expected,
        heard: &heard,
    }
    .diffs()
}

/// The edit-distance alignment of expected tokens with heard ones.
struct Alignment<'a> {
    expected: &'a [Token],
    heard: &'a [Heard],
}

impl Alignment<'_> {
    /// Two expected tokens heard as one, ending at expected[i - 1] and
    /// heard[j - 1].
    fn two_heard_as_one(&self, i: usize, j: usize) -> Option<Join> {
        let (Some(i), Some(j)) = (i.checked_sub(2), j.checked_sub(1)) else {
            return None;
        };
        let e = self.expected;
        pair_is_one(&e[i], &e[i + 1], &self.heard[j].token, false)
    }

    /// One expected token heard as two, ending at expected[i - 1] and
    /// heard[j - 1].
    fn one_heard_as_two(&self, i: usize, j: usize) -> Option<Join> {
        let (Some(i), Some(j)) = (i.checked_sub(1), j.checked_sub(2)) else {
            return None;
        };
        let h = self.heard;
        pair_is_one(&h[j].token, &h[j + 1].token, &self.expected[i], true)
    }

    fn matches(&self, i: usize, j: usize) -> bool {
        same(&self.expected[i - 1], &self.heard[j - 1].token)
    }

    /// cost[i][j]: the distance between expected[..i] and heard[..j].
    fn costs(&self) -> Vec<Vec<usize>> {
        let (n, m) = (self.expected.len(), self.heard.len());
        let mut cost = vec![vec![0_usize; m + 1]; n + 1];
        for (i, row) in cost.iter_mut().enumerate() {
            row[0] = i;
        }
        for (j, cell) in cost[0].iter_mut().enumerate() {
            *cell = j;
        }
        for i in 1..=n {
            for j in 1..=m {
                let differs = usize::from(!self.matches(i, j));
                let mut best = (cost[i - 1][j - 1] + differs)
                    .min(cost[i - 1][j] + 1)
                    .min(cost[i][j - 1] + 1);
                if let Some(join) = self.two_heard_as_one(i, j) {
                    best = best.min(cost[i - 2][j - 1] + join_cost(join));
                }
                if let Some(join) = self.one_heard_as_two(i, j) {
                    best = best.min(cost[i - 1][j - 2] + join_cost(join));
                }
                cost[i][j] = best;
            }
        }
        cost
    }

    fn diffs(&self) -> Vec<WordDiff> {
        let (expected, heard) = (self.expected, self.heard);
        let cost = self.costs();
        let mut diffs = Vec::new();
        let (mut i, mut j) = (expected.len(), heard.len());
        while i > 0 || j > 0 {
            if let Some(join) = self.two_heard_as_one(i, j)
                && cost[i][j] == cost[i - 2][j - 1] + join_cost(join)
            {
                if join == Join::Compound {
                    diffs.push(WordDiff {
                        kind: WordDiffKind::Substituted,
                        expected: Some(format!(
                            "{} {}",
                            expected[i - 2].written,
                            expected[i - 1].written
                        )),
                        heard: Some(heard[j - 1].token.written.clone()),
                        at: Some(heard[j - 1].start),
                    });
                }
                i -= 2;
                j -= 1;
                continue;
            }
            if let Some(join) = self.one_heard_as_two(i, j)
                && cost[i][j] == cost[i - 1][j - 2] + join_cost(join)
            {
                if join == Join::Compound {
                    diffs.push(WordDiff {
                        kind: WordDiffKind::Substituted,
                        expected: Some(expected[i - 1].written.clone()),
                        heard: Some(format!(
                            "{} {}",
                            heard[j - 2].token.written,
                            heard[j - 1].token.written
                        )),
                        at: Some(heard[j - 2].start),
                    });
                }
                i -= 1;
                j -= 2;
                continue;
            }
            if i > 0 && j > 0 {
                let matches = self.matches(i, j);
                if cost[i][j] == cost[i - 1][j - 1] + usize::from(!matches) {
                    if !matches {
                        diffs.push(WordDiff {
                            kind: WordDiffKind::Substituted,
                            expected: Some(expected[i - 1].written.clone()),
                            heard: Some(heard[j - 1].token.written.clone()),
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
                    expected: Some(expected[i - 1].written.clone()),
                    heard: None,
                    at,
                });
                i -= 1;
            } else {
                diffs.push(WordDiff {
                    kind: WordDiffKind::Extra,
                    expected: None,
                    heard: Some(heard[j - 1].token.written.clone()),
                    at: Some(heard[j - 1].start),
                });
                j -= 1;
            }
        }
        diffs.reverse();
        diffs
    }
}

/// What a join of two tokens with one costs in the alignment: nothing for
/// the same words, a substitution for a compound.
fn join_cost(join: Join) -> usize {
    match join {
        Join::Same => 0,
        Join::Compound => 1,
    }
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
            shown("first second third twenty-first"),
            tokens("first second 3rd 21st")
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
        assert_eq!(diffs[0].expected.as_deref(), Some("$5"));
        assert_eq!(diffs[0].heard.as_deref(), Some("six"));
    }

    #[test]
    fn numbers_never_fuse_across_punctuation() {
        assert_eq!(
            shown("Pick twenty, one at a time"),
            tokens("pick 20 one at a time")
        );
        assert_eq!(
            shown("I ordered 10, 20, and 30"),
            tokens("i ordered 10 20 and 30")
        );
        assert_eq!(
            shown("I ordered ten, twenty, and thirty"),
            tokens("i ordered 10 20 and 30")
        );
        assert_eq!(
            shown("chapter twenty, first part"),
            tokens("chapter 20 first part")
        );
        // Heard words carry their punctuation; a comma ends the number.
        let said = |text: &str| -> Vec<String> {
            let parts: Vec<&str> = text.split_whitespace().collect();
            normalize_parts(&parts)
                .iter()
                .map(|(token, _, _)| token.display())
                .collect()
        };
        assert_eq!(
            said("pick twenty, one at a time"),
            tokens("pick 20 one at a time")
        );
        assert_eq!(said("ten, twenty, and thirty"), tokens("10 20 and 30"));
        // Without punctuation a number still spans heard words.
        assert_eq!(said("eighteen sixty seven"), tokens("1867"));
        // The pairing rule does not cross punctuation either.
        let diffs = align(
            &normalize("Pick twenty, one at a time"),
            &heard("Pick 21 at a time"),
        );
        assert_ne!(diffs, []);
    }

    #[test]
    fn first_and_second_are_ordinals_only_when_attached() {
        assert_eq!(shown("a sixty second timer"), tokens("a 60 second timer"));
        assert_eq!(shown("first we go"), tokens("first we go"));
        assert_eq!(shown("twenty-first"), tokens("21st"));
        assert_eq!(shown("sixty-second"), tokens("62nd"));
        assert_eq!(shown("twenty first"), tokens("20 first"));
        assert_eq!(shown("2nd and 1st"), tokens("2nd and 1st"));
        // Other ordinals are unambiguous.
        assert_eq!(shown("twenty third"), tokens("23rd"));
        assert_eq!(shown("the third"), tokens("the 3rd"));
    }

    #[test]
    fn align_matches_split_and_joined_numbers() {
        for (script, said) in [
            ("a sixty second timer", "a 60-second timer"),
            ("a 60-second timer", "a sixty-second timer"),
            ("the 21st century", "the twenty first century"),
            ("the 2nd day", "the second day"),
            ("the 1st day", "the first day"),
            ("the 62nd floor", "the sixty second floor"),
            ("twenty one", "21"),
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
    }

    #[test]
    fn diffs_name_words_as_written() {
        let named = |script: &str, said: &str| -> Vec<(Option<String>, Option<String>)> {
            align(&normalize(script), &heard(said))
                .into_iter()
                .map(|d| (d.expected, d.heard))
                .collect()
        };
        let pair = |e: &str, h: &str| (Some(e.to_owned()), Some(h.to_owned()));
        assert_eq!(
            named("the second day", "the 3rd day"),
            [pair("second", "3rd")]
        );
        assert_eq!(
            named("the 2nd day", "the third day"),
            [pair("2nd", "third")]
        );
        assert_eq!(
            named("in 1,867", "in eighteen sixty-eight"),
            [pair("1,867", "eighteen sixty-eight")]
        );
        assert_eq!(named("$5", "six dollars"), [pair("$5", "six")]);
        assert_eq!(
            named("3.50%", "three point six percent"),
            [pair("3.50", "three point six")]
        );
        assert_eq!(
            named("Niklas's link's below", "Niklas lynx below"),
            [pair("link's", "lynx")]
        );
        let missing = align(&normalize("the twenty-first day"), &heard("the day"));
        assert_eq!(missing[0].expected.as_deref(), Some("twenty-first"));
    }

    #[test]
    fn decimals_compare_by_value() {
        assert_eq!(shown("3.50%"), tokens("3.5 percent"));
        assert_eq!(shown("three point five zero"), tokens("3.5"));
        assert_eq!(shown("2.0"), tokens("2"));
        for (script, said) in [
            ("3.50%", "three point five percent"),
            ("3.5", "three point five zero"),
            ("$1.50", "one point five dollars"),
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
        assert_ne!(align(&normalize("3.5"), &heard("three point five one")), []);
    }

    #[test]
    fn negative_numbers_keep_their_sign() {
        assert_eq!(shown("-5 and −3.5"), tokens("-5 and -3.5"));
        assert_eq!(shown("minus five"), tokens("-5"));
        assert_eq!(shown("negative one"), tokens("-1"));
        assert_eq!(shown("-1"), tokens("-1"));
        // A hyphen between words or numbers is no sign.
        assert_eq!(shown("pages 10-15"), tokens("pages 10 15"));
        for (script, said) in [
            ("It was -5 degrees", "It was minus five degrees"),
            ("It was -5 degrees", "It was negative five degrees"),
            ("It was -5 degrees", "It was -5 degrees"),
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
        let diffs = align(&normalize("It was -5 degrees"), &heard("It was 5 degrees"));
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].expected.as_deref(), Some("-5"));
        let diffs = align(&normalize("minus five"), &heard("five"));
        assert_ne!(diffs, []);
    }

    #[test]
    fn time_rule_needs_a_clock_time_in_the_script() {
        for said in ["at eleven thirty", "at 11:30", "at 11 30", "at nine thirty"] {
            let script = if said.contains("nine") {
                "at 9:30"
            } else {
                "at 11:30"
            };
            assert_eq!(align(&normalize(script), &heard(said)), [], "{said}");
        }
        // Without a colon, two numbers are not a time.
        assert_ne!(align(&normalize("in 1130"), &heard("in 11 30")), []);
        assert_ne!(align(&normalize("10 20"), &heard("1020")), []);
        assert_ne!(
            align(&normalize("at eleven thirty"), &heard("at 11 30")),
            []
        );
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
    fn drops_bracketed_annotations() {
        let said =
            |words: Vec<Word>| -> Vec<String> { words.into_iter().map(|w| w.text).collect() };
        let words =
            heard("[BLANK_AUDIO] Nobody (sighs) misses [Music] a (clears throat) birthday.");
        let kept = drop_annotations(words);
        assert_eq!(said(kept.clone()), ["Nobody", "misses", "a", "birthday."]);
        // Each word keeps its own times.
        assert_eq!(kept[1].start, 3.0);
        // An annotation inside a word goes, the word stays.
        assert_eq!(
            said(drop_annotations(heard("hello(laughs) there"))),
            ["hello", "there"]
        );
        // An unclosed bracket is not an annotation.
        assert_eq!(said(drop_annotations(heard("a (b c"))), ["a", "(b", "c"]);
    }

    #[test]
    fn isolates_annotations_as_words_of_their_own() {
        let said =
            |words: &[Word]| -> Vec<String> { words.iter().map(|w| w.text.clone()).collect() };
        let words = isolate_annotations(heard(
            "[BLANK_AUDIO] Nobody (clears throat) misses hello(laughs) birthday.",
        ));
        assert_eq!(
            said(&words),
            [
                "[BLANK_AUDIO]",
                "Nobody",
                "(clears throat)",
                "misses",
                "hello",
                "(laughs)",
                "birthday."
            ]
        );
        // A multi-word annotation spans its words; others keep their times.
        assert_eq!((words[2].start, words[2].end), (2.0, 3.5));
        assert_eq!((words[5].start, words[5].end), (5.0, 5.5));
        assert!(is_annotation("(clears throat)") && is_annotation("[Music]"));
        assert!(!is_annotation("hello") && !is_annotation("(b"));
        // An unclosed bracket is not an annotation.
        assert_eq!(
            said(&isolate_annotations(heard("a (b c"))),
            ["a", "(b", "c"]
        );
    }

    #[test]
    fn an_annotation_over_several_words_keeps_its_middle_words() {
        let said =
            |words: &[Word]| -> Vec<String> { words.iter().map(|w| w.text.clone()).collect() };
        let words = isolate_annotations(heard("Nobody [background music playing] misses"));
        assert_eq!(
            said(&words),
            ["Nobody", "[background music playing]", "misses"]
        );
        assert_eq!((words[1].start, words[1].end), (1.0, 3.5));
        // Brackets as words of their own.
        let words = isolate_annotations(heard("a [ soft music ] b"));
        assert_eq!(said(&words), ["a", "[soft music]", "b"]);
        assert_eq!(
            said(&drop_annotations(heard("a (clears his throat) b"))),
            ["a", "b"]
        );
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
    fn align_flags_compounds_both_ways() {
        let substituted = |expected: &str, heard: &str, at: f64| WordDiff {
            kind: WordDiffKind::Substituted,
            expected: Some(expected.to_owned()),
            heard: Some(heard.to_owned()),
            at: Some(at),
        };
        for (script, said, two, one, at) in [
            ("every day", "everyday", "every day", "everyday", 0.0),
            ("every one", "everyone", "every one", "everyone", 0.0),
            (
                "Kindred is free and open source.",
                "Kindred is free and opensource",
                "open source",
                "opensource",
                4.0,
            ),
        ] {
            assert_eq!(
                align(&normalize(script), &heard(said)),
                [substituted(two, one, at)],
                "{script} / {said}"
            );
            assert_eq!(
                align(&normalize(said), &heard(script)),
                [substituted(one, two, at)],
                "{said} / {script}"
            );
        }
        // Only the two words that make it up.
        let diffs = align(&normalize("every day"), &heard("everyone"));
        assert_ne!(diffs, []);
    }

    #[test]
    fn align_joins_hyphenated_words() {
        for (script, said) in [
            ("a voice-over", "a voiceover"),
            ("send an e-mail", "send an email"),
            ("free and open-source", "free and opensource"),
            ("a voice-over", "a voice-over"),
            ("a voice-over", "a voice over"),
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
        // An open compound is still a possible mishearing.
        assert_eq!(align(&normalize("every one"), &heard("everyone")).len(), 1);
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
        assert_eq!(diffs[0].heard.as_deref(), Some("eighteen sixty eight"));
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
