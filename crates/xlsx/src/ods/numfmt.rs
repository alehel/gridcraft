//! ODF data styles (`number:number-style`, `number:date-style`… ODF 1.3 part 3 §16.29) ↔
//! Excel number format codes, through one model of a format section.
//!
//! A code is split into [`Section`]s (`positive;negative;zero;text`), each a list of [`Part`]s.
//! Writing renders each section as a data style element, the first ones mapped from the last by
//! `style:map` conditions; reading builds the same sections from the elements, and one printer
//! spells them as a code (it alone decides what is quoted). LibreOffice's own translation of
//! format codes into data styles was observed to pick the same element for each code token.

use std::collections::HashMap;
use std::fmt::Write as _;

use super::dom::{Elem, Ns};
use crate::xml::{esc, esc_attr};

/// The colours a format code names, with the RGB they stand for.
static COLORS: [(&str, &str); 8] = [
    ("Red", "#ff0000"),
    ("Blue", "#0000ff"),
    ("Green", "#00ff00"),
    ("Black", "#000000"),
    ("White", "#ffffff"),
    ("Yellow", "#ffff00"),
    ("Magenta", "#ff00ff"),
    ("Cyan", "#00ffff"),
];

/// The most sections a code has.
const MAX_SECTIONS: usize = 4;

// ----------------------------------------------------------------------------------- the model

/// One section of a format code.
#[derive(Clone, Debug, Default, PartialEq)]
struct Section {
    /// A colour name from [`COLORS`].
    color: Option<&'static str>,
    /// An explicit condition (`[>=100]`): operator and number, as written.
    cond: Option<String>,
    parts: Vec<Part>,
}

#[derive(Clone, Debug, PartialEq)]
enum Part {
    /// LibreOffice's standard number format.
    General,
    Number(Number),
    /// `0.00E+00`.
    Scientific {
        number: Number,
        exp_digits: usize,
        plus: bool,
    },
    /// `# ?/8`: the whole part (if any), numerator digits and denominator.
    Fraction {
        int: Option<IntMask>,
        numerator: usize,
        denominator: Denominator,
    },
    Date {
        field: Field,
        long: bool,
        elapsed: bool,
    },
    AmPm,
    /// Text shown as is.
    Lit(String),
    /// The percent sign, which scales the value by 100.
    Percent,
    /// `$`, or `[$€-407]`: the symbol (ODF keeps a locale apart from it, as language and
    /// country, which GridCraft doesn't map: `[$€-407]` reads back as `[$€]`).
    Currency {
        symbol: String,
    },
    /// `@`: the cell's text.
    Text,
    /// `*c`: the character repeated to fill the cell.
    Fill(char),
    /// `_c`: a space as wide as the character.
    Blank(char),
}

/// The integer digits of a number: how many always show (`0` or `?`), how many of those show as
/// spaces when not needed (`?`), whether thousands are grouped, and how many digits the code
/// draws (`#` included).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct IntMask {
    min: usize,
    blank: usize,
    grouping: bool,
    width: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Number {
    int: IntMask,
    decimals: usize,
    min_decimals: usize,
    /// Trailing commas: each scales the value by a thousandth.
    thousands: usize,
    /// Text among the integer digits (`000-00-0000`), by the number of integer digits to its
    /// right.
    embedded: Vec<(usize, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Denominator {
    Fixed(u32),
    /// As many digits as needed, up to this many.
    Digits(usize),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Field {
    Year,
    Month { textual: bool },
    Day,
    Weekday,
    Era,
    Hours,
    Minutes,
    Seconds { decimals: usize },
}

// --------------------------------------------------------------------------- code → sections

/// What a scan of the code finds, before a section's kind gives letters their meaning.
#[derive(Clone, Debug)]
enum Raw {
    Quoted(String),
    Escaped(char),
    Bracket(String),
    Fill(char),
    Blank(char),
    At,
    Char(char),
}

/// Splits a code into its sections' raw parts at `;` outside quotes, brackets and escapes.
fn scan(code: &str) -> Vec<Vec<Raw>> {
    let mut out = vec![Vec::new()];
    let mut chars = code.chars();
    let until = |chars: &mut std::str::Chars<'_>, end: char| chars.by_ref().take_while(|c| *c != end).collect::<String>();
    while let Some(c) = chars.next() {
        let raw = match c {
            '"' => Raw::Quoted(until(&mut chars, '"')),
            '[' => Raw::Bracket(until(&mut chars, ']')),
            '\\' => Raw::Escaped(chars.next().unwrap_or(' ')),
            '_' => Raw::Blank(chars.next().unwrap_or(' ')),
            '*' => Raw::Fill(chars.next().unwrap_or(' ')),
            '@' => Raw::At,
            ';' => {
                out.push(Vec::new());
                continue;
            }
            c => Raw::Char(c),
        };
        if let Some(section) = out.last_mut() {
            section.push(raw);
        }
    }
    out
}

/// The sections of a format code (at most four).
fn parse_code(code: &str) -> Vec<Section> {
    scan(code).into_iter().take(MAX_SECTIONS).map(|raws| parse_section(&raws)).collect()
}

/// A section's tokens, letters already read as date parts or number characters.
#[derive(Debug)]
enum Tok {
    Part(Part),
    /// A date letter run: letter, count, elapsed (`[h]`).
    Date(char, usize, bool),
    /// `.0` after seconds.
    SecFrac(usize),
    /// Number characters: `0#?.,` and `E+`/`E-`.
    Num(String),
    /// `/` then the denominator of a fraction.
    Frac(Denominator),
}

fn push_lit(toks: &mut Vec<Tok>, s: &str) {
    match toks.last_mut() {
        Some(Tok::Part(Part::Lit(t))) => t.push_str(s),
        _ => toks.push(Tok::Part(Part::Lit(s.to_string()))),
    }
}

fn push_num(toks: &mut Vec<Tok>, s: &str) {
    match toks.last_mut() {
        Some(Tok::Num(t)) => t.push_str(s),
        _ => toks.push(Tok::Num(s.to_string())),
    }
}

fn parse_section(raws: &[Raw]) -> Section {
    let mut section = Section::default();
    // A section with date or time letters (or `[h]`) reads its letters as date parts.
    let date = raws.iter().any(|r| match r {
        Raw::Char(c) => matches!(c.to_ascii_lowercase(), 'y' | 'd' | 'h' | 's' | 'm'),
        Raw::Bracket(b) => {
            let lower = b.to_ascii_lowercase();
            lower.chars().next().is_some_and(|l| matches!(l, 'h' | 'm' | 's') && lower.chars().all(|c| c == l))
        }
        _ => false,
    });
    let chars: Vec<Option<char>> = raws.iter().map(|r| if let Raw::Char(c) = r { Some(*c) } else { None }).collect();
    let starts =
        |i: usize, word: &str| word.chars().enumerate().all(|(k, w)| chars.get(i + k).copied().flatten().is_some_and(|c| c.eq_ignore_ascii_case(&w)));
    let mut toks: Vec<Tok> = Vec::new();
    let mut i = 0;
    while let Some(raw) = raws.get(i) {
        i += 1;
        let c = match raw {
            Raw::Quoted(s) => {
                push_lit(&mut toks, s);
                continue;
            }
            Raw::Escaped(c) => {
                push_lit(&mut toks, c.encode_utf8(&mut [0; 4]));
                continue;
            }
            Raw::Fill(c) => {
                toks.push(Tok::Part(Part::Fill(*c)));
                continue;
            }
            Raw::Blank(c) => {
                toks.push(Tok::Part(Part::Blank(*c)));
                continue;
            }
            Raw::At => {
                toks.push(Tok::Part(Part::Text));
                continue;
            }
            Raw::Bracket(b) => {
                bracket(b, &mut section, &mut toks);
                continue;
            }
            Raw::Char(c) => *c,
        };
        let at = i - 1;
        if starts(at, "general") {
            toks.push(Tok::Part(Part::General));
            i = at + 7;
            continue;
        }
        if date {
            if starts(at, "am/pm") || starts(at, "a/p") {
                i = at + if starts(at, "am/pm") { 5 } else { 3 };
                toks.push(Tok::Part(Part::AmPm));
                continue;
            }
            let l = c.to_ascii_lowercase();
            if matches!(l, 'y' | 'm' | 'd' | 'h' | 's' | 'e' | 'g') {
                let mut n = 1;
                while chars.get(i).copied().flatten().is_some_and(|x| x.to_ascii_lowercase() == l) {
                    n += 1;
                    i += 1;
                }
                toks.push(Tok::Date(l, n, false));
                continue;
            }
            if c == '.' && chars.get(i).copied().flatten() == Some('0') {
                let mut n = 0;
                while chars.get(i).copied().flatten() == Some('0') {
                    n += 1;
                    i += 1;
                }
                toks.push(Tok::SecFrac(n));
                continue;
            }
        } else {
            match c {
                '0' | '#' | '?' | '.' | ',' => {
                    push_num(&mut toks, c.encode_utf8(&mut [0; 4]));
                    continue;
                }
                '%' => {
                    toks.push(Tok::Part(Part::Percent));
                    continue;
                }
                'E' | 'e' if matches!(chars.get(i).copied().flatten(), Some('+' | '-')) => {
                    let sign = chars.get(i).copied().flatten().unwrap_or('+');
                    i += 1;
                    push_num(&mut toks, &format!("E{sign}"));
                    continue;
                }
                '/' if matches!(toks.last(), Some(Tok::Num(_))) => {
                    let mut den = String::new();
                    while let Some(d) = chars.get(i).copied().flatten().filter(|c| c.is_ascii_digit() || matches!(c, '?' | '#')) {
                        den.push(d);
                        i += 1;
                    }
                    let fixed = den.chars().all(|c| c.is_ascii_digit()).then(|| den.parse::<u32>().ok()).flatten();
                    toks.push(Tok::Frac(match fixed {
                        Some(d) if d > 0 => Denominator::Fixed(d),
                        _ => Denominator::Digits(den.len().clamp(1, 9)),
                    }));
                    continue;
                }
                '$' | '€' | '£' | '¥' => {
                    toks.push(Tok::Part(Part::Currency { symbol: c.to_string() }));
                    continue;
                }
                _ => {}
            }
        }
        push_lit(&mut toks, c.encode_utf8(&mut [0; 4]));
    }
    section.parts = parts(&toks);
    section
}

/// A `[…]` part: a colour, a condition, a currency or locale, or elapsed time.
fn bracket(b: &str, section: &mut Section, toks: &mut Vec<Tok>) {
    let lower = b.to_ascii_lowercase();
    if let Some(cur) = b.strip_prefix('$') {
        let symbol = cur.split_once('-').map_or(cur, |(s, _)| s);
        // `[$-409]` names only a locale: it shows nothing.
        if !symbol.is_empty() {
            toks.push(Tok::Part(Part::Currency { symbol: symbol.to_string() }));
        }
    } else if b.starts_with(['<', '>', '=']) {
        // A condition is an operator and a number (LibreOffice's "any number" bound,
        // `<=1.79769313486232E+308`, rounds past the largest float); anything else is none.
        let op_len = b.chars().take_while(|c| matches!(c, '<' | '>' | '=')).count();
        let (op, num) = b.split_at(op_len);
        if matches!(op, "<" | ">" | "=" | "<=" | ">=" | "<>") && num.trim().parse::<f64>().is_ok_and(|v| !v.is_nan()) {
            section.cond = Some(format!("{op}{}", num.trim()));
        }
    } else if let Some(l @ ('h' | 'm' | 's')) = lower.chars().next()
        && lower.chars().all(|c| c == l)
    {
        toks.push(Tok::Date(l, lower.len(), true));
    } else if let Some((name, _)) = COLORS.iter().find(|(n, _)| n.eq_ignore_ascii_case(b)) {
        section.color = Some(*name);
    }
}

/// Counts the digit placeholders of number characters.
fn digit_count(s: &str) -> usize {
    s.chars().filter(|c| matches!(c, '0' | '#' | '?')).count()
}

fn int_mask(int: &str) -> IntMask {
    IntMask {
        min: int.chars().filter(|c| matches!(c, '0' | '?')).count(),
        blank: int.chars().filter(|c| *c == '?').count(),
        grouping: int.contains(','),
        width: digit_count(int),
    }
}

/// A run of number characters (`#,##0.00,,`) as a number.
fn number(run: &str) -> Number {
    // Commas after the last digit of the integer part (`0,.0`) or of the whole run (`0.0,,`)
    // scale by a thousand each; commas among the integer digits group them.
    let (int, dec) = run.split_once('.').unwrap_or((run, ""));
    let int_digits = int.trim_end_matches(',');
    let dec_digits_text = dec.trim_end_matches(',');
    let thousands = (int.len() - int_digits.len()) + (dec.len() - dec_digits_text.len());
    let dec_digits: Vec<char> = dec_digits_text.chars().filter(|c| matches!(c, '0' | '#' | '?')).collect();
    Number {
        int: int_mask(int_digits),
        decimals: dec_digits.len(),
        min_decimals: dec_digits.iter().take_while(|c| **c == '0').count(),
        thousands,
        embedded: Vec::new(),
    }
}

/// Which `m` runs of a section are minutes: after hours, before seconds, or elapsed.
fn minutes(toks: &[Tok]) -> Vec<bool> {
    let date_letters: Vec<(usize, char)> =
        toks.iter().enumerate().filter_map(|(i, t)| if let Tok::Date(l, ..) = t { Some((i, *l)) } else { None }).collect();
    let mut out = vec![false; toks.len()];
    for (k, &(i, l)) in date_letters.iter().enumerate() {
        if l != 'm' {
            continue;
        }
        let prev_h = k.checked_sub(1).and_then(|p| date_letters.get(p)).is_some_and(|(_, l)| *l == 'h');
        let next_s = date_letters.get(k + 1).is_some_and(|(_, l)| *l == 's');
        let elapsed = matches!(toks.get(i), Some(Tok::Date(_, _, true)));
        if let Some(m) = out.get_mut(i) {
            *m = prev_h || next_s || elapsed;
        }
    }
    out
}

/// A section's tokens as its parts: number runs joined into one number (or fraction), date
/// letters into fields.
fn parts(toks: &[Tok]) -> Vec<Part> {
    let minutes = minutes(toks);
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(t) = toks.get(i) {
        match t {
            Tok::Part(p) => out.push(p.clone()),
            Tok::Date(letter, n, elapsed) => {
                let (field, long) = match letter {
                    'y' => (Field::Year, *n >= 3),
                    'd' if *n >= 3 => (Field::Weekday, *n >= 4),
                    'd' => (Field::Day, *n >= 2),
                    'm' if minutes.get(i).copied().unwrap_or(false) => (Field::Minutes, *n >= 2),
                    'm' if *n >= 3 => (Field::Month { textual: true }, *n >= 4),
                    'm' => (Field::Month { textual: false }, *n >= 2),
                    'h' => (Field::Hours, *n >= 2),
                    's' => {
                        let decimals = if let Some(Tok::SecFrac(d)) = toks.get(i + 1) { *d } else { 0 };
                        (Field::Seconds { decimals }, *n >= 2)
                    }
                    // `e` is the year (in the Gregorian calendar, the four-digit year); `g` the era.
                    'e' => (Field::Year, true),
                    _ => (Field::Era, *n >= 3),
                };
                out.push(Part::Date { field, long, elapsed: *elapsed });
            }
            // Seconds took their decimals; a fraction's denominator is taken with its number.
            Tok::SecFrac(_) | Tok::Frac(_) => {}
            Tok::Num(n) => {
                i = number_run(toks, i, n, &mut out);
                continue;
            }
        }
        i += 1;
    }
    out
}

/// The number run starting at token `i` (a fraction, digits broken up by literals, or plain
/// digits) as one part; returns the index after it.
fn number_run(toks: &[Tok], i: usize, n: &str, out: &mut Vec<Part>) -> usize {
    // `?/?`, and `# ?/?`, which arrives as Num("#"), Lit(" "), Num("?"), Frac.
    if let Some(Tok::Frac(den)) = toks.get(i + 1) {
        out.push(Part::Fraction { int: None, numerator: digit_count(n).max(1), denominator: *den });
        return i + 2;
    }
    // A whole part joined to the fraction by any text (`0 ?/?`, `0" and "?/?`): ODF's fraction
    // holds no text there, so the text is dropped rather than the denominator.
    if let (Some(Tok::Part(Part::Lit(_))), Some(Tok::Num(numer)), Some(Tok::Frac(den))) = (toks.get(i + 1), toks.get(i + 2), toks.get(i + 3)) {
        out.push(Part::Fraction { int: Some(int_mask(n)), numerator: digit_count(numer).max(1), denominator: *den });
        return i + 4;
    }
    if digit_count(n) == 0 {
        out.push(Part::Lit(n.to_string()));
        return i + 1;
    }
    if let Some((mantissa, exp)) = n.split_once('E') {
        let number = number(mantissa);
        out.push(Part::Scientific { exp_digits: digit_count(exp).max(1), plus: exp.starts_with('+'), number });
        return i + 1;
    }
    // Integer digits broken up by short literals (`000-00-0000`): one number with embedded
    // text. The integer part ends at the first run with a decimal point.
    let mut runs = vec![n];
    let mut texts: Vec<&str> = Vec::new();
    let mut j = i + 1;
    while !runs.last().is_some_and(|r| r.contains('.'))
        && let (Some(Tok::Part(Part::Lit(l))), Some(Tok::Num(m))) = (toks.get(j), toks.get(j + 1))
        && !matches!(toks.get(j + 2), Some(Tok::Frac(_)))
    {
        texts.push(l);
        runs.push(m);
        j += 2;
    }
    let all: String = runs.concat();
    let mut num = number(&all);
    // Positions count the integer digits to the right of the text.
    let int_digits = |r: &str| digit_count(r.split('.').next().unwrap_or(r));
    let mut right: usize = runs.iter().map(|r| int_digits(r)).sum();
    for (run, text) in runs.iter().zip(&texts) {
        right = right.saturating_sub(int_digits(run));
        num.embedded.push((right, text.to_string()));
    }
    out.push(Part::Number(num));
    j
}

// -------------------------------------------------------------------------- sections → code

/// Text that reads back as text without quotes in a section of this kind.
fn plain_literal(c: char, date: bool) -> bool {
    matches!(c, ' ' | '-' | '+' | '(' | ')' | ':') || (date && matches!(c, ',' | '/' | '.'))
}

/// Literal text in Excel's spelling: bare when it can't be mistaken for format characters,
/// else quoted (a quote itself is escaped, as quotes can't hold one).
fn print_literal(out: &mut String, text: &str, date: bool) {
    // An empty text (`""`) shows nothing, which only quotes can say.
    if text.is_empty() {
        out.push_str("\"\"");
        return;
    }
    if text.chars().all(|c| plain_literal(c, date)) {
        out.push_str(text);
        return;
    }
    for (k, piece) in text.split('"').enumerate() {
        if k > 0 {
            out.push_str("\\\"");
        }
        if !piece.is_empty() {
            let _ = write!(out, "\"{piece}\"");
        }
    }
}

fn print_int(out: &mut String, int: &IntMask, embedded: &[(usize, String)]) {
    // A digit stands left of every embedded text, or the text would lead the number.
    let width = int.width.max(int.min).max(if int.grouping { 4 } else { 1 }).max(embedded.iter().map(|(p, _)| *p + 1).max().unwrap_or(0));
    for pos in (0..width).rev() {
        for (_, text) in embedded.iter().filter(|(p, _)| *p == pos + 1) {
            print_literal(out, text, false);
        }
        // Blank digits (`?`) are the leading ones of the minimum integer digits.
        out.push(if pos < int.min && pos + int.blank >= int.min {
            '?'
        } else if pos < int.min {
            '0'
        } else {
            '#'
        });
        if int.grouping && pos == 3 {
            out.push(',');
        }
    }
    for (_, text) in embedded.iter().filter(|(p, _)| *p == 0) {
        print_literal(out, text, false);
    }
}

fn print_number(out: &mut String, n: &Number) {
    print_int(out, &n.int, &n.embedded);
    if n.decimals > 0 {
        out.push('.');
        for i in 0..n.decimals {
            out.push(if i < n.min_decimals { '0' } else { '#' });
        }
    }
    out.push_str(&",".repeat(n.thousands.min(6)));
}

fn print_part(out: &mut String, p: &Part, date: bool) {
    match p {
        Part::General => out.push_str("General"),
        Part::Number(n) => print_number(out, n),
        Part::Scientific { number, exp_digits, plus, .. } => {
            print_number(out, number);
            out.push_str(if *plus { "E+" } else { "E-" });
            out.push_str(&"0".repeat(*exp_digits));
        }
        Part::Fraction { int, numerator, denominator } => {
            if let Some(int) = int {
                print_int(out, &IntMask { width: int.min, ..*int }, &[]);
                out.push(' ');
            }
            out.push_str(&"?".repeat(*numerator));
            out.push('/');
            match denominator {
                Denominator::Fixed(d) => {
                    let _ = write!(out, "{d}");
                }
                Denominator::Digits(n) => out.push_str(&"?".repeat(*n)),
            }
        }
        Part::Date { field, long, elapsed } => {
            let mut s = match (field, long) {
                (Field::Year, true) => "yyyy".to_string(),
                (Field::Year, false) => "yy".into(),
                (Field::Month { textual: true }, true) => "mmmm".into(),
                (Field::Month { textual: true }, false) => "mmm".into(),
                (Field::Month { textual: false } | Field::Minutes, true) => "mm".into(),
                (Field::Month { textual: false } | Field::Minutes, false) => "m".into(),
                (Field::Day, true) => "dd".into(),
                (Field::Day, false) => "d".into(),
                (Field::Weekday, true) => "dddd".into(),
                (Field::Weekday, false) => "ddd".into(),
                (Field::Era, true) => "ggg".into(),
                (Field::Era, false) => "g".into(),
                (Field::Hours, true) => "hh".into(),
                (Field::Hours, false) => "h".into(),
                (Field::Seconds { .. }, true) => "ss".into(),
                (Field::Seconds { .. }, false) => "s".into(),
            };
            if *elapsed {
                s = format!("[{s}]");
            }
            out.push_str(&s);
            if let Field::Seconds { decimals } = field
                && *decimals > 0
            {
                out.push('.');
                out.push_str(&"0".repeat(*decimals));
            }
        }
        Part::AmPm => out.push_str("AM/PM"),
        Part::Lit(t) => print_literal(out, t, date),
        Part::Percent => out.push('%'),
        Part::Currency { symbol } if symbol == "$" => out.push('$'),
        Part::Currency { symbol } => {
            let _ = write!(out, "[${symbol}]");
        }
        Part::Text => out.push('@'),
        Part::Fill(c) => {
            out.push('*');
            out.push(*c);
        }
        Part::Blank(c) => {
            out.push('_');
            out.push(*c);
        }
    }
}

fn print_code(sections: &[Section]) -> String {
    let mut out = String::new();
    for (k, s) in sections.iter().enumerate() {
        if k > 0 {
            out.push(';');
        }
        if let Some(c) = s.color {
            let _ = write!(out, "[{c}]");
        }
        if let Some(c) = &s.cond {
            let _ = write!(out, "[{c}]");
        }
        let date = s.parts.iter().any(|p| matches!(p, Part::Date { .. } | Part::AmPm));
        for p in &s.parts {
            print_part(&mut out, p, date);
        }
    }
    out
}

// -------------------------------------------------------------------------- sections → XML

/// The data style element a section calls for.
fn kind(s: &Section) -> &'static str {
    let has = |f: &dyn Fn(&Part) -> bool| s.parts.iter().any(f);
    if has(&|p| matches!(p, Part::Text)) {
        "text-style"
    } else if has(&|p| matches!(p, Part::Date { field: Field::Year | Field::Month { .. } | Field::Day | Field::Weekday | Field::Era, .. })) {
        "date-style"
    } else if has(&|p| matches!(p, Part::Date { .. } | Part::AmPm)) {
        "time-style"
    } else if has(&|p| matches!(p, Part::Percent)) {
        "percentage-style"
    } else if has(&|p| matches!(p, Part::Currency { .. })) {
        "currency-style"
    } else {
        "number-style"
    }
}

fn int_attrs(out: &mut String, int: &IntMask) {
    let _ = write!(out, " number:min-integer-digits=\"{}\"", int.min);
    if int.blank > 0 {
        let _ = write!(out, " loext:max-blank-integer-digits=\"{}\"", int.blank);
    }
    if int.grouping {
        out.push_str(" number:grouping=\"true\"");
    }
}

fn text_el(out: &mut String, t: &str) {
    if !t.is_empty() {
        let _ = write!(out, "<number:text>{}</number:text>", esc(t));
    }
}

fn part_xml(out: &mut String, p: &Part) {
    let long = |l: bool| if l { " number:style=\"long\"" } else { "" };
    match p {
        Part::General => out.push_str("<number:number number:min-integer-digits=\"1\"/>"),
        Part::Number(n) => {
            let _ = write!(out, "<number:number number:decimal-places=\"{}\" number:min-decimal-places=\"{}\"", n.decimals, n.min_decimals);
            int_attrs(out, &n.int);
            if n.thousands > 0 {
                let _ = write!(out, " number:display-factor=\"{}\"", 1000u64.saturating_pow(n.thousands.min(6) as u32));
            }
            if n.embedded.is_empty() {
                out.push_str("/>");
            } else {
                out.push('>');
                for (pos, t) in &n.embedded {
                    let _ = write!(out, "<number:embedded-text number:position=\"{pos}\">{}</number:embedded-text>", esc(t));
                }
                out.push_str("</number:number>");
            }
        }
        Part::Scientific { number: n, exp_digits, plus } => {
            // The exponent moves in steps of the integer digits drawn (`##0.0E+0`: engineering).
            let interval = if n.int.grouping { 1 } else { n.int.width.max(1) };
            let _ =
                write!(out, "<number:scientific-number number:decimal-places=\"{}\" number:min-decimal-places=\"{}\"", n.decimals, n.min_decimals);
            int_attrs(out, &IntMask { min: n.int.min.max(1), ..n.int });
            let _ = write!(
                out,
                " number:min-exponent-digits=\"{exp_digits}\" number:exponent-interval=\"{interval}\" number:forced-exponent-sign=\"{plus}\"/>"
            );
        }
        Part::Fraction { int, numerator, denominator } => {
            let _ = write!(out, "<number:fraction number:min-numerator-digits=\"{numerator}\"");
            if let Some(int) = int {
                int_attrs(out, int);
            }
            let _ = match denominator {
                Denominator::Fixed(d) => write!(out, " number:denominator-value=\"{d}\"/>"),
                Denominator::Digits(d) => write!(out, " number:min-denominator-digits=\"{d}\" number:max-denominator-value=\"{}\"/>", "9".repeat(*d)),
            };
        }
        Part::Date { field, long: l, .. } => {
            let _ = match field {
                Field::Year => write!(out, "<number:year{}/>", long(*l)),
                Field::Month { textual } => write!(out, "<number:month{}{}/>", long(*l), if *textual { " number:textual=\"true\"" } else { "" }),
                Field::Day => write!(out, "<number:day{}/>", long(*l)),
                Field::Weekday => write!(out, "<number:day-of-week{}/>", long(*l)),
                Field::Era => write!(out, "<number:era{}/>", long(*l)),
                Field::Hours => write!(out, "<number:hours{}/>", long(*l)),
                Field::Minutes => write!(out, "<number:minutes{}/>", long(*l)),
                Field::Seconds { decimals: 0 } => write!(out, "<number:seconds{}/>", long(*l)),
                Field::Seconds { decimals } => write!(out, "<number:seconds{} number:decimal-places=\"{decimals}\"/>", long(*l)),
            };
        }
        Part::AmPm => out.push_str("<number:am-pm/>"),
        // An empty text is written too: a section of only `""` shows nothing, not General.
        Part::Lit(t) if t.is_empty() => out.push_str("<number:text></number:text>"),
        Part::Lit(t) => text_el(out, t),
        Part::Percent => text_el(out, "%"),
        Part::Currency { symbol, .. } => {
            let _ = write!(out, "<number:currency-symbol>{}</number:currency-symbol>", esc(symbol));
        }
        Part::Text => out.push_str("<number:text-content/>"),
        Part::Fill(c) => {
            let _ = write!(out, "<number:fill-character>{}</number:fill-character>", esc(&c.to_string()));
        }
        Part::Blank(c) => {
            let _ = write!(out, "<number:text loext:blank-width-char=\"{}\"> </number:text>", esc_attr(&c.to_string()));
        }
    }
}

/// A section as a data style element named `name`, with `maps` (its `style:map` children).
fn section_xml(out: &mut String, name: &str, s: &Section, volatile: bool, maps: &str) {
    let kind = kind(s);
    let _ = write!(out, "<number:{kind} style:name=\"{}\"", esc_attr(name));
    if volatile {
        out.push_str(" style:volatile=\"true\"");
    }
    if s.parts.iter().any(|p| matches!(p, Part::Date { elapsed: true, .. })) {
        out.push_str(" number:truncate-on-overflow=\"false\"");
    }
    out.push('>');
    if let Some(rgb) = s.color.and_then(|c| COLORS.iter().find(|(n, _)| *n == c)).map(|(_, rgb)| rgb) {
        let _ = write!(out, "<style:text-properties fo:color=\"{rgb}\"/>");
    }
    for p in &s.parts {
        part_xml(out, p);
    }
    let _ = write!(out, "{maps}</number:{kind}>");
}

/// The data style elements for `code`, the main one named `name`. `None` for General.
pub(crate) fn write_style(name: &str, code: &str) -> Option<String> {
    if code.eq_ignore_ascii_case("general") || code.is_empty() {
        return None;
    }
    let mut sections = parse_code(code);
    // The last section is the main style; the others are mapped from it by their conditions:
    // stated ones, or Excel's implicit positive;negative;zero;text.
    let explicit = sections.iter().any(|s| s.cond.is_some());
    if sections.len() < MAX_SECTIONS && sections.last().is_some_and(|s| s.cond.is_some()) {
        // Every section has its condition: other values show as General, the main style.
        sections.push(Section { parts: vec![Part::General], ..Section::default() });
    }
    // Stated conditions that read back as Excel's implicit ones keep their meaning, except a
    // lone `>=0` before the other values (General) or text: read implicitly, its negatives
    // would take the first section without their sign. They are stated as the General they
    // show (`[>=0]0`, `[>=0]0;@`). A section of the user's own for them stays as it is.
    let general = Section { parts: vec![Part::General], ..Section::default() };
    if let [first, rest] = sections.as_slice()
        && first.cond.as_deref() == Some(">=0")
        && (*rest == general || rest.parts.contains(&Part::Text))
    {
        sections.insert(1, Section { cond: Some("<0".into()), parts: vec![Part::Lit("-".into()), Part::General], ..Section::default() });
    }
    let n = sections.len();
    // A last text section (`0.00;@`) takes text only: the sections before it share the numbers
    // as if it weren't there, a lone one showing negatives with a minus.
    let text_last = !explicit && (2..4).contains(&n) && sections.last().is_some_and(|s| s.parts.contains(&Part::Text));
    // (A date or time has no negative form: only its number of days turns it negative; a text
    // section shows a number as it is.)
    if text_last
        && n == 2
        && let Some(first) = sections.first().cloned()
        && !matches!(kind(&first), "date-style" | "time-style" | "text-style")
    {
        let mut parts = vec![Part::Lit("-".into())];
        parts.extend(first.parts.iter().cloned());
        sections.insert(1, Section { parts, ..first });
    }
    let n = sections.len();
    let mut out = String::new();
    let mut maps = String::new();
    for (i, s) in sections.iter().take(n - 1).enumerate() {
        let part = format!("{name}P{i}");
        let mut mapped = s.clone();
        let condition = match mapped.cond.take() {
            // ODF writes Excel's `<>` as `!=`.
            Some(c) => c.replacen("<>", "!=", 1),
            None if explicit => ">=0".into(),
            None => match (n, i, text_last) {
                (2, ..) | (3, 0, true) => ">=0".into(),
                (_, 0, _) => ">0".into(),
                (_, 1, _) => "<0".into(),
                _ => "=0".into(),
            },
        };
        section_xml(&mut out, &part, &mapped, true, "");
        let _ = write!(maps, "<style:map style:condition=\"value(){}\" style:apply-style-name=\"{}\"/>", esc_attr(&condition), esc_attr(&part));
    }
    let main = sections.last().cloned().unwrap_or_default();
    section_xml(&mut out, name, &Section { cond: None, ..main }, false, &maps);
    Some(out)
}

/// The ODF value type a number in format `code` is written as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValueKind {
    Date,
    Time,
    Percent,
    Float,
}

/// How a number formatted with `code` is typed in the file (its first section decides).
pub(crate) fn value_kind(code: &str) -> ValueKind {
    let Some(first) = parse_code(code).into_iter().next() else { return ValueKind::Float };
    match kind(&first) {
        "date-style" => ValueKind::Date,
        "time-style" => ValueKind::Time,
        "percentage-style" => ValueKind::Percent,
        _ => ValueKind::Float,
    }
}

// -------------------------------------------------------------------------- XML → sections

/// A data style read from the file: its own sections (one, or a boolean style's three) and the
/// conditions mapping other styles.
pub(crate) struct DataStyle {
    sections: Vec<Section>,
    /// `style:map` entries: condition and the applied style's name.
    maps: Vec<(String, String)>,
}

/// Reads every data style below `parent` (an `office:styles` or `office:automatic-styles`).
pub(crate) fn read_styles(parent: &Elem, out: &mut HashMap<String, DataStyle>) {
    for e in parent.elems() {
        if e.ns != Ns::Number {
            continue;
        }
        let Some(name) = e.attr(Ns::Style, "name") else { continue };
        if let Some(sections) = read_style(e) {
            let maps = e
                .children(Ns::Style, "map")
                .filter_map(|m| Some((m.attr(Ns::Style, "condition")?.to_string(), m.attr(Ns::Style, "apply-style-name")?.to_string())))
                .collect();
            out.insert(name.to_string(), DataStyle { sections, maps });
        }
    }
}

/// The full Excel format code of the data style `name`, with its mapped sections.
pub(crate) fn code(styles: &HashMap<String, DataStyle>, name: &str) -> Option<String> {
    let main = styles.get(name)?;
    let Some(own) = main.sections.first() else { return Some("General".into()) };
    if main.maps.is_empty() {
        // A style that shows nothing on its own is the default one.
        if main.sections.len() == 1 && own.parts.is_empty() {
            return Some(print_code(&[Section { parts: vec![Part::General], ..own.clone() }]));
        }
        return Some(print_code(&main.sections));
    }
    let mapped: Vec<(Option<String>, Section)> = main
        .maps
        .iter()
        .take(MAX_SECTIONS - 1)
        .map(|(cond, apply)| {
            let s =
                styles.get(apply).and_then(|s| s.sections.first().cloned()).unwrap_or(Section { parts: vec![Part::General], ..Section::default() });
            (condition(cond), s)
        })
        .collect();
    let conds: Vec<&str> = mapped.iter().map(|(c, _)| c.as_deref().unwrap_or("?")).collect();
    // Excel's implicit sections (positive;negative[;zero[;text]]) need no conditions.
    // (with a text section last, positive-and-zero;negative;text).
    let text_main = own.parts.contains(&Part::Text);
    let implicit = match conds.as_slice() {
        [">=0"] | [">0", "<0"] | [">0", "<0", "=0"] => true,
        [">=0", "<0"] => text_main,
        _ => false,
    };
    let mut sections: Vec<Section> = mapped.into_iter().map(|(cond, s)| Section { cond: if implicit { None } else { cond }, ..s }).collect();
    // A bare General main under explicit conditions is the "other values" the writer adds.
    let general = Section { parts: vec![Part::General], ..Section::default() };
    if !sections.iter().all(|s| s.cond.is_some()) || *own != general {
        sections.push(own.clone());
    }
    Some(print_code(&sections))
}

/// `value()>=0` → `>=0`.
fn condition(c: &str) -> Option<String> {
    let rest = c.trim().strip_prefix("value()")?.trim();
    let op_len = rest.chars().take_while(|c| matches!(c, '<' | '>' | '=' | '!')).count();
    let op = rest.get(..op_len)?;
    let num = rest.get(op_len..)?.trim();
    // As a code's own conditions: a number, LibreOffice's past-the-largest bound included.
    num.parse::<f64>().ok().filter(|v| !v.is_nan())?;
    let op = match op {
        "==" => "=",
        "!=" => "<>",
        o @ ("<" | ">" | "<=" | ">=" | "=" | "<>") => o,
        _ => return None,
    };
    Some(format!("{op}{num}"))
}

fn flag(e: &Elem, name: &str) -> bool {
    e.attr(Ns::Number, name) == Some("true")
}

fn int_attr(e: &Elem, name: &str) -> Option<usize> {
    e.attr(Ns::Number, name).or_else(|| e.attr(Ns::LoExt, name)).and_then(|v| v.trim().parse::<usize>().ok()).map(|n| n.min(30))
}

/// The integer digits an element states (`min_default` when it states no minimum).
fn read_int(e: &Elem, min_default: usize) -> IntMask {
    let min = int_attr(e, "min-integer-digits").unwrap_or(min_default);
    IntMask { min, blank: int_attr(e, "max-blank-integer-digits").unwrap_or(0).min(min), grouping: flag(e, "grouping"), width: min }
}

/// A `number:number` or `number:scientific-number`'s digits.
fn read_number(e: &Elem, scientific: bool) -> Number {
    let decimals = int_attr(e, "decimal-places").unwrap_or(if scientific { 2 } else { 0 });
    // Each trailing comma scales by a thousand; a factor past 1000^6 (or not a finite number)
    // is no format anyone writes.
    let mut thousands = 0;
    if let Some(mut f) = e.attr(Ns::Number, "display-factor").and_then(|f| f.parse::<f64>().ok()).filter(|f| f.is_finite()) {
        while f >= 1000.0 && thousands < 6 {
            thousands += 1;
            f /= 1000.0;
        }
    }
    let mut embedded: Vec<(usize, String)> = e
        .children(Ns::Number, "embedded-text")
        .filter_map(|t| Some((t.attr(Ns::Number, "position")?.parse::<usize>().ok()?.min(30), t.raw_text())))
        .collect();
    embedded.sort_by_key(|e| std::cmp::Reverse(e.0));
    Number {
        int: read_int(e, if scientific { 1 } else { 0 }),
        decimals,
        min_decimals: int_attr(e, "min-decimal-places").unwrap_or(decimals).min(decimals),
        thousands,
        embedded,
    }
}

/// `number:number` alone without decimal places is LibreOffice's General ("Standard").
fn is_standard(n: &Elem) -> bool {
    n.name == "number"
        && n.attr(Ns::Number, "decimal-places").is_none()
        && !flag(n, "grouping")
        && n.attr(Ns::Number, "display-factor").is_none()
        && int_attr(n, "min-integer-digits").unwrap_or(1) == 1
        && n.children(Ns::Number, "embedded-text").next().is_none()
}

/// A short date in the locale's order (`number:automatic-order`) shows month/day/year, as the
/// locale short date does in GridCraft's en-US display.
fn locale_short_date(style: &Elem) -> Option<Section> {
    if style.attr(Ns::Number, "automatic-order") != Some("true") {
        return None;
    }
    let elems: Vec<&Elem> = style.elems().filter(|e| e.ns == Ns::Number && e.name != "text").collect();
    let long = |e: &Elem| e.attr(Ns::Number, "style") == Some("long");
    let find = |n: &str| elems.iter().find(|e| e.name == n).copied();
    let (Some(d), Some(m), Some(y)) = (find("day"), find("month"), find("year")) else { return None };
    if elems.len() != 3 || flag(m, "textual") {
        return None;
    }
    let date = |field, long| Part::Date { field, long, elapsed: false };
    let slash = || Part::Lit("/".into());
    Some(Section {
        parts: vec![date(Field::Month { textual: false }, long(m)), slash(), date(Field::Day, long(d)), slash(), date(Field::Year, long(y))],
        ..Section::default()
    })
}

/// A data style element as its sections, or `None` for an element that isn't a data style.
fn read_style(style: &Elem) -> Option<Vec<Section>> {
    let kind = style.name.as_str();
    if !matches!(kind, "number-style" | "currency-style" | "percentage-style" | "date-style" | "time-style" | "text-style" | "boolean-style") {
        return None;
    }
    // Numbers in a boolean style show TRUE for anything but zero, as LibreOffice's own
    // translation of the style to a format code says.
    if kind == "boolean-style" {
        return Some(parse_code("\"TRUE\";\"TRUE\";\"FALSE\""));
    }
    if kind == "date-style"
        && let Some(s) = locale_short_date(style)
    {
        return Some(vec![s]);
    }
    let color = style
        .child(Ns::Style, "text-properties")
        .and_then(|t| t.attr(Ns::Fo, "color"))
        .and_then(|rgb| COLORS.iter().find(|(_, c)| c.eq_ignore_ascii_case(rgb)).map(|(n, _)| *n));
    // In an elapsed-time style the first time part is the one that isn't cut at its unit.
    let mut elapsed = style.attr(Ns::Number, "truncate-on-overflow") == Some("false");
    let mut parts = Vec::new();
    let mut percent_seen = false;
    for e in style.elems().filter(|e| e.ns == Ns::Number) {
        let long = e.attr(Ns::Number, "style") == Some("long");
        let mut date = |field: Field| {
            let first = elapsed && matches!(field, Field::Hours | Field::Minutes | Field::Seconds { .. });
            if first {
                elapsed = false;
            }
            Part::Date { field, long, elapsed: first }
        };
        let part = match e.name.as_str() {
            "number" if is_standard(e) => Part::General,
            "number" => Part::Number(read_number(e, false)),
            "scientific-number" => {
                let mut number = read_number(e, true);
                number.int.width = number.int.width.max(int_attr(e, "exponent-interval").unwrap_or(1));
                Part::Scientific {
                    number,
                    exp_digits: int_attr(e, "min-exponent-digits").unwrap_or(2).max(1),
                    plus: e.attr(Ns::Number, "forced-exponent-sign") != Some("false"),
                }
            }
            "fraction" => Part::Fraction {
                int: e.attr(Ns::Number, "min-integer-digits").map(|_| read_int(e, 0)),
                numerator: int_attr(e, "min-numerator-digits").unwrap_or(1).max(1),
                denominator: match e.attr(Ns::Number, "denominator-value").and_then(|d| d.trim().parse::<u32>().ok()).filter(|d| *d > 0) {
                    Some(d) => Denominator::Fixed(d),
                    None => Denominator::Digits(
                        int_attr(e, "min-denominator-digits")
                            .or_else(|| e.attr(Ns::Number, "max-denominator-value").map(str::len))
                            .unwrap_or(1)
                            .clamp(1, 9),
                    ),
                },
            },
            "currency-symbol" => {
                let symbol = e.raw_text();
                // An empty symbol is only a locale.
                if symbol.is_empty() {
                    continue;
                }
                Part::Currency { symbol: symbol.replace(']', "") }
            }
            "text" => {
                let t = e.raw_text();
                match e.attr(Ns::LoExt, "blank-width-char").and_then(|c| c.chars().next()) {
                    Some(c) if t == " " => Part::Blank(c),
                    // In a percentage style the first `%` is the percent sign; any other is text.
                    _ if kind == "percentage-style" && !percent_seen && t.contains('%') => {
                        percent_seen = true;
                        let (before, after) = t.split_once('%').unwrap_or((&t, ""));
                        if !before.is_empty() {
                            parts.push(Part::Lit(before.to_string()));
                        }
                        parts.push(Part::Percent);
                        if after.is_empty() {
                            continue;
                        }
                        Part::Lit(after.to_string())
                    }
                    _ => Part::Lit(t),
                }
            }
            "fill-character" => Part::Fill(e.raw_text().chars().next().unwrap_or(' ')),
            "text-content" => Part::Text,
            "day" => date(Field::Day),
            "month" => date(Field::Month { textual: flag(e, "textual") }),
            "year" => date(Field::Year),
            "day-of-week" => date(Field::Weekday),
            "era" => date(Field::Era),
            "hours" => date(Field::Hours),
            "minutes" => date(Field::Minutes),
            "seconds" => date(Field::Seconds { decimals: int_attr(e, "decimal-places").unwrap_or(0) }),
            "am-pm" => Part::AmPm,
            // Quarters have no Excel token.
            "quarter" => Part::Lit("Q".into()),
            _ => continue,
        };
        parts.push(part);
    }
    Some(vec![Section { color, cond: None, parts }])
}

#[cfg(test)]
mod tests {
    use super::super::dom::parse_tree;
    use super::*;

    fn round_trip(src: &str) -> String {
        let Some(xml) = write_style("N1", src) else { return "General".into() };
        let doc = format!(
            r#"<office:styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:loext="urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0">{xml}</office:styles>"#
        );
        let Ok(tree) = parse_tree(doc.as_bytes()) else { return format!("bad xml: {doc}") };
        let mut styles = HashMap::new();
        read_styles(&tree, &mut styles);
        code(&styles, "N1").unwrap_or_default()
    }

    #[test]
    fn codes_survive_a_round_trip() {
        for code in [
            "0",
            "0.00",
            "#,##0",
            "#,##0.00",
            "#,##0.00;[Red]-#,##0.00",
            "0%",
            "0.00%",
            "0.00E+00",
            "# ?/?",
            "# ??/??",
            "?/?",
            "# ?/8",
            "\"Total: \"0.00",
            "0.00\" kg\"",
            "#,##0,",
            "000-00-0000",
            "@",
            "yyyy-mm-dd",
            "m/d/yy",
            "d-mmm-yy",
            "dddd, mmmm d, yyyy",
            "h:mm AM/PM",
            "h:mm:ss",
            "[h]:mm:ss",
            "mm:ss.0",
            "yyyy-mm-dd hh:mm:ss",
            "[Red]0.00",
            "0.0_);(0.0)",
            "0.###",
            "$#,##0.00",
        ] {
            assert_eq!(round_trip(code), code, "{code}");
        }
        assert_eq!(round_trip("General"), "General");
        assert_eq!(value_kind("yyyy-mm-dd hh:mm"), ValueKind::Date);
        assert_eq!(value_kind("[h]:mm:ss"), ValueKind::Time);
        assert_eq!(value_kind("mm:ss.0"), ValueKind::Time);
        assert_eq!(value_kind("0.00%"), ValueKind::Percent);
        assert_eq!(value_kind("#,##0.00 [$€]"), ValueKind::Float);
    }

    /// What the text-to-text translation lost: empty sections, quoted format characters, quotes
    /// in text, embedded text next to decimals, locale tags, a fourth section without `@`,
    /// coloured General, a single conditional section, and denominators of a million.
    #[test]
    fn sections_keep_their_meaning() {
        for (code, back) in [
            ("0;-0;", "0;-0;"),
            ("0;;", "0;;"),
            (";;;@", ";;;@"),
            ("0\"%\"", "0\"%\""),
            ("#\",\"", "#\",\""),
            ("0\"/\"", "0\"/\""),
            ("0\" %\"", "0\" %\""),
            ("0\"a\"\\\"\"b\"", "0\"a\"\\\"\"b\""),
            ("[Magenta]0.00", "[Magenta]0.00"),
            ("0-0.00", "0-0.00"),
            ("00-00.0", "00-00.0"),
            ("[$-409]h:mm:ss", "h:mm:ss"),
            ("0;-0;0;\"text\"", "0;-0;0;\"text\""),
            ("[Red]General", "[Red]General"),
            ("General;[Red]-General", "General;[Red]-General"),
            ("[>0]0", "[>0]0"),
            // Round four: a lone `>=0` keeps its negatives' sign.
            ("[>=0]0", "[>=0]0;[<0]-General"),
            // Round five: the same with a text section, and text shown as text.
            ("[>=0]0.00;@", "0.00;-General;@"),
            ("[>=0]\"p\";@", "\"p\";-General;@"),
            ("@;@", "@;@"),
            // Round six: a negative section of the user's own is kept.
            ("[>=0]0;[Red]0", "0;[Red]0"),
            ("[>=0]0.00;\"x\"", "0.00;\"x\""),
            // Round three: a last conditional section keeps its condition; a time has no
            // made-up negative section.
            ("[>100]0;[<-100]-0", "[>100]0;[<-100]-0"),
            ("[Red][>0]0;[Blue][<0]0", "[Red]0;[Blue]0;General"),
            ("h:mm;@", "h:mm;@"),
            ("[h]:mm;@", "[h]:mm;@"),
            ("[Blue][<=0]0;0", "[Blue][<=0]0;0"),
            ("# ?/1000000", "# ?/1000000"),
            ("[$€-407]#,##0.00", "[$€]#,##0.00"),
            // Round two: engineering notation, a literal % next to the percent sign, the era
            // year, long embedded text, a scaling comma before the point, an empty text, and
            // elapsed time in a date.
            ("##0.0E+0", "##0.0E+0"),
            ("0% \"%\"", "0%\" %\""),
            ("0%\"%\"", "0%\"%\""),
            ("e-mm-dd", "yyyy-mm-dd"),
            ("0\" to \"0", "0\" to \"0"),
            ("0,.0", "0.0,"),
            ("\"\"", "\"\""),
            ("d [h]:mm", "d [h]:mm"),
            // Round three: leading # before embedded text, a fraction joined by text, a text
            // section after fewer than three number sections, and conditions.
            ("###-####", "#-####"),
            ("(###) ###-####", "(#) ###-####"),
            ("0\" and \"?/?", "0 ?/?"),
            ("0.00;@", "0.00;-0.00;@"),
            ("0;-0;@", "0;-0;@"),
            ("[<>0]0;\"zero\"", "[<>0]0;\"zero\""),
            ("[>abc]0", "0"),
        ] {
            assert_eq!(round_trip(code), back, "{code}");
        }
        // A quote in text reads back as the same text.
        let sections = parse_code(&round_trip("0\"a\"\\\"\"b\""));
        assert_eq!(sections.first().and_then(|s| s.parts.get(1)), Some(&Part::Lit("a\"b".into())));
    }
}
