//! ODF data styles (`number:number-style`, `number:date-style`… ODF 1.3 part 3 §16.29) ↔
//! Excel number format codes. LibreOffice's own translation of format codes into data styles
//! was observed to pick the same element for each code token.

use std::collections::HashMap;
use std::fmt::Write as _;

use super::dom::{Elem, Ns};
use crate::xml::esc_attr;

/// A data style read from the file: its format code and the conditions mapping other styles.
pub(crate) struct DataStyle {
    /// The code of this style alone (one section, without the mapped ones).
    code: String,
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
        if let Some(code) = section(e) {
            let maps = e
                .children(Ns::Style, "map")
                .filter_map(|m| Some((m.attr(Ns::Style, "condition")?.to_string(), m.attr(Ns::Style, "apply-style-name")?.to_string())))
                .collect();
            out.insert(name.to_string(), DataStyle { code, maps });
        }
    }
}

/// The full Excel format code of the data style `name`, with its mapped sections.
pub(crate) fn code(styles: &HashMap<String, DataStyle>, name: &str) -> Option<String> {
    let main = styles.get(name)?;
    if main.maps.is_empty() {
        return Some(main.code.clone());
    }
    let mut parts: Vec<(Option<String>, &str)> = Vec::new();
    for (cond, apply) in main.maps.iter().take(3) {
        let mapped = styles.get(apply).map(|s| s.code.as_str()).unwrap_or("General");
        parts.push((condition(cond), mapped));
    }
    let conds: Vec<&str> = parts.iter().map(|(c, _)| c.as_deref().unwrap_or("?")).collect();
    let main_is_text = main.code.contains('@');
    let mut out = String::new();
    match conds.as_slice() {
        // Excel's implicit sections: positive;negative[;zero[;text]].
        [">=0"] => {
            let _ = write!(out, "{};{}", parts.first().map_or("", |p| p.1), main.code);
        }
        [">0", "<0"] => {
            let _ = write!(out, "{};{};{}", parts.first().map_or("", |p| p.1), parts.get(1).map_or("", |p| p.1), main.code);
        }
        [">0", "<0", "=0"] if main_is_text => {
            let _ = write!(
                out,
                "{};{};{};{}",
                parts.first().map_or("", |p| p.1),
                parts.get(1).map_or("", |p| p.1),
                parts.get(2).map_or("", |p| p.1),
                main.code
            );
        }
        _ => {
            for (cond, code) in parts.iter().take(2) {
                if let Some(c) = cond {
                    let _ = write!(out, "[{c}]");
                }
                out.push_str(code);
                out.push(';');
            }
            out.push_str(&main.code);
        }
    }
    Some(out)
}

/// `value()>=0` → `>=0`.
fn condition(c: &str) -> Option<String> {
    let rest = c.trim().strip_prefix("value()")?.trim();
    let op_len = rest.chars().take_while(|c| matches!(c, '<' | '>' | '=' | '!')).count();
    let op = rest.get(..op_len)?;
    let num = rest.get(op_len..)?.trim();
    num.parse::<f64>().ok()?;
    let op = match op {
        "==" => "=",
        "!=" => "<>",
        o @ ("<" | ">" | "<=" | ">=" | "=" | "<>") => o,
        _ => return None,
    };
    Some(format!("{op}{num}"))
}

fn flag(e: &Elem, ns: Ns, name: &str) -> bool {
    e.attr(ns, name) == Some("true")
}

fn int_attr(e: &Elem, name: &str) -> Option<usize> {
    e.attr(Ns::Number, name).or_else(|| e.attr(Ns::LoExt, name)).and_then(|v| v.trim().parse::<usize>().ok()).map(|n| n.min(30))
}

/// Excel spelling of literal text.
fn literal(text: &str, out: &mut String) {
    if text.is_empty() {
        return;
    }
    if text.chars().all(|c| matches!(c, ' ' | '-' | '/' | ':' | ',' | '(' | ')' | '$' | '+' | '%')) {
        out.push_str(text);
    } else {
        out.push('"');
        out.push_str(&text.replace('"', "\\\""));
        out.push('"');
    }
}

fn color_name(rgb: &str) -> Option<&'static str> {
    Some(match rgb.to_ascii_lowercase().as_str() {
        "#ff0000" => "[Red]",
        "#0000ff" => "[Blue]",
        "#00ff00" => "[Green]",
        "#000000" => "[Black]",
        "#ffffff" => "[White]",
        "#ffff00" => "[Yellow]",
        "#ff00ff" => "[Magenta]",
        "#00ffff" => "[Cyan]",
        _ => return None,
    })
}

/// The integer and decimal digits of a `number:number` or `number:scientific-number`.
fn digits(e: &Elem, out: &mut String, scientific: bool) {
    let decimals = int_attr(e, "decimal-places").unwrap_or(if scientific { 2 } else { 0 });
    let min_decimals = int_attr(e, "min-decimal-places").unwrap_or(decimals).min(decimals);
    let min_int = int_attr(e, "min-integer-digits").unwrap_or(if scientific { 1 } else { 0 });
    let blank_int = int_attr(e, "max-blank-integer-digits").unwrap_or(0).min(min_int);
    let grouping = flag(e, Ns::Number, "grouping");
    let interval = if scientific { int_attr(e, "exponent-interval").unwrap_or(1).max(1) } else { 1 };
    // Embedded text: (position counted from the decimal point leftwards, text).
    let mut embedded: Vec<(usize, String)> = e
        .children(Ns::Number, "embedded-text")
        .filter_map(|t| Some((t.attr(Ns::Number, "position")?.parse::<usize>().ok()?.min(30), t.raw_text())))
        .collect();
    embedded.sort_by_key(|e| std::cmp::Reverse(e.0));
    let int_len = min_int.max(interval).max(if grouping { 4 } else { 1 }).max(embedded.first().map_or(0, |(p, _)| *p));
    for pos in (0..int_len).rev() {
        for (_, text) in embedded.iter().filter(|(p, _)| *p == pos + 1) {
            literal(text, out);
        }
        // Blank digits (`?`) are the leading ones of the minimum integer digits.
        out.push(if pos < min_int && pos >= min_int - blank_int {
            '?'
        } else if pos < min_int {
            '0'
        } else {
            '#'
        });
        if grouping && pos == 3 {
            out.push(',');
        }
    }
    // A text at position 0 sits between the integer and the decimals.
    for (_, text) in embedded.iter().filter(|(p, _)| *p == 0) {
        literal(text, out);
    }
    if decimals > 0 {
        out.push('.');
        for i in 0..decimals {
            out.push(if i < min_decimals { '0' } else { '#' });
        }
    }
    // Each trailing comma scales by a thousand; a factor past 1000^6 (or not a finite number)
    // is no format anyone writes.
    if let Some(mut f) = e.attr(Ns::Number, "display-factor").and_then(|f| f.parse::<f64>().ok()).filter(|f| f.is_finite()) {
        for _ in 0..6 {
            if f < 1000.0 {
                break;
            }
            out.push(',');
            f /= 1000.0;
        }
    }
}

/// A short date in the locale's order (`number:automatic-order`) shows month/day/year, as the
/// locale short date does in GridCraft's en-US display.
fn locale_short_date(style: &Elem) -> Option<String> {
    if style.attr(Ns::Number, "automatic-order") != Some("true") {
        return None;
    }
    let parts: Vec<&Elem> = style.elems().filter(|e| e.ns == Ns::Number && e.name != "text").collect();
    let long = |e: &Elem| e.attr(Ns::Number, "style") == Some("long");
    let find = |n: &str| parts.iter().find(|e| e.name == n).copied();
    let (Some(d), Some(m), Some(y)) = (find("day"), find("month"), find("year")) else { return None };
    if parts.len() != 3 || flag(m, Ns::Number, "textual") {
        return None;
    }
    Some(format!("{}/{}/{}", if long(m) { "mm" } else { "m" }, if long(d) { "dd" } else { "d" }, if long(y) { "yyyy" } else { "yy" }))
}

/// `number:fraction` (`# ?/?`, `0 ??/16`).
fn fraction(e: &Elem, out: &mut String) {
    if let Some(min_int) = int_attr(e, "min-integer-digits") {
        let blank = int_attr(e, "max-blank-integer-digits").unwrap_or(0).min(min_int);
        let grouping = flag(e, Ns::Number, "grouping");
        if min_int == 0 {
            out.push('#');
        } else {
            let len = if grouping { min_int.max(4) } else { min_int };
            for pos in (0..len).rev() {
                out.push(if pos >= min_int {
                    '#'
                } else if pos >= min_int - blank {
                    '?'
                } else {
                    '0'
                });
                if grouping && pos == 3 {
                    out.push(',');
                }
            }
        }
        out.push(' ');
    }
    let num = int_attr(e, "min-numerator-digits").unwrap_or(1).max(1);
    out.push_str(&"?".repeat(num));
    out.push('/');
    match e.attr(Ns::Number, "denominator-value") {
        Some(d) if d.chars().all(|c| c.is_ascii_digit()) && !d.is_empty() => out.push_str(d),
        _ => {
            let den =
                int_attr(e, "min-denominator-digits").or_else(|| e.attr(Ns::Number, "max-denominator-value").map(|v| v.len())).unwrap_or(1).max(1);
            out.push_str(&"?".repeat(den));
        }
    }
}

/// Elapsed-time state of a time style: whether the first time part is still to be bracketed.
struct Elapsed(bool);

impl Elapsed {
    fn push(&mut self, out: &mut String, letter: &str, long: bool) {
        let s = letter.repeat(if long { 2 } else { 1 });
        if self.0 {
            self.0 = false;
            let _ = write!(out, "[{s}]");
        } else {
            out.push_str(&s);
        }
    }
}

/// One element of a data style, appended to `out`. `Err` = the style is a fixed code (boolean);
/// `Ok(false)` = the element has no code.
fn element(e: &Elem, out: &mut String, elapsed: &mut Elapsed) -> Result<bool, String> {
    let long = e.attr(Ns::Number, "style") == Some("long");
    let pick = |a: &'static str, b: &'static str| if long { a } else { b };
    match e.name.as_str() {
        "number" => digits(e, out, false),
        "scientific-number" => {
            digits(e, out, true);
            out.push_str(if e.attr(Ns::Number, "forced-exponent-sign") == Some("false") { "E-" } else { "E+" });
            out.push_str(&"0".repeat(int_attr(e, "min-exponent-digits").unwrap_or(2).max(1)));
        }
        "fraction" => fraction(e, out),
        "currency-symbol" => {
            let sym = e.raw_text();
            if sym.is_empty() || sym == "$" {
                out.push('$');
            } else {
                let _ = write!(out, "[${}]", sym.replace(']', ""));
            }
        }
        "text" => {
            let t = e.raw_text();
            match e.attr(Ns::LoExt, "blank-width-char") {
                Some(c) if t == " " && c.chars().count() == 1 => {
                    out.push('_');
                    out.push_str(c);
                }
                _ => literal(&t, out),
            }
        }
        "fill-character" => {
            out.push('*');
            out.push(e.raw_text().chars().next().unwrap_or(' '));
        }
        "text-content" => out.push('@'),
        "day" => out.push_str(pick("dd", "d")),
        "month" => out.push_str(match (flag(e, Ns::Number, "textual"), long) {
            (true, true) => "mmmm",
            (true, false) => "mmm",
            (false, true) => "mm",
            (false, false) => "m",
        }),
        "year" => out.push_str(pick("yyyy", "yy")),
        "day-of-week" => out.push_str(pick("dddd", "ddd")),
        "era" => out.push_str(pick("ggg", "g")),
        "hours" => elapsed.push(out, "h", long),
        "minutes" => elapsed.push(out, "m", long),
        "seconds" => {
            elapsed.push(out, "s", long);
            if let Some(d) = int_attr(e, "decimal-places").filter(|d| *d > 0) {
                out.push('.');
                out.push_str(&"0".repeat(d));
            }
        }
        "am-pm" => out.push_str("AM/PM"),
        // Numbers in a boolean style show TRUE for anything but zero, as LibreOffice's own
        // translation of the style to a format code says.
        "boolean" => return Err("\"TRUE\";\"TRUE\";\"FALSE\"".into()),
        // Quarters have no Excel token.
        "quarter" => out.push_str("\"Q\""),
        _ => return Ok(false),
    }
    Ok(true)
}

/// `number:number` alone without decimal places is LibreOffice's General ("Standard").
fn is_standard(style: &Elem) -> bool {
    style.name == "number-style"
        && style.elems().filter(|e| e.ns == Ns::Number).count() == 1
        && style.child(Ns::Number, "number").is_some_and(|n| {
            n.attr(Ns::Number, "decimal-places").is_none()
                && !flag(n, Ns::Number, "grouping")
                && n.attr(Ns::Number, "display-factor").is_none()
                && int_attr(n, "min-integer-digits").unwrap_or(1) == 1
        })
}

/// One data style element as an Excel section, or `None` for an element that isn't a data style.
fn section(style: &Elem) -> Option<String> {
    let kind = style.name.as_str();
    if !matches!(kind, "number-style" | "currency-style" | "percentage-style" | "date-style" | "time-style" | "text-style" | "boolean-style") {
        return None;
    }
    if kind == "date-style"
        && let Some(code) = locale_short_date(style)
    {
        return Some(code);
    }
    let mut out = String::new();
    if let Some(color) = style.child(Ns::Style, "text-properties").and_then(|t| t.attr(Ns::Fo, "color")).and_then(color_name) {
        out.push_str(color);
    }
    let mut elapsed = Elapsed(kind == "time-style" && style.attr(Ns::Number, "truncate-on-overflow") == Some("false"));
    let mut any = false;
    for e in style.elems().filter(|e| e.ns == Ns::Number) {
        match element(e, &mut out, &mut elapsed) {
            Ok(used) => any |= used,
            Err(fixed) => return Some(fixed),
        }
    }
    if !any || out.is_empty() || is_standard(style) {
        return Some("General".into());
    }
    Some(out)
}

// ------------------------------------------------------------------------------------- writing

/// Splits a format code into sections at `;` outside quotes, brackets and escapes.
fn sections(code: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut chars = code.chars().peekable();
    let mut quoted = false;
    let mut bracket = false;
    while let Some(c) = chars.next() {
        let cur = out.last_mut();
        let Some(cur) = cur else { break };
        match c {
            '"' if !bracket => {
                quoted = !quoted;
                cur.push(c);
            }
            '[' if !quoted => {
                bracket = true;
                cur.push(c);
            }
            ']' if !quoted => {
                bracket = false;
                cur.push(c);
            }
            '\\' | '_' | '*' if !quoted && !bracket => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ';' if !quoted && !bracket => out.push(String::new()),
            c => cur.push(c),
        }
    }
    out
}

#[derive(Debug)]
enum Tok {
    Lit(String),
    Color(String),
    Cond(String),
    Currency(String),
    /// Date/time letter run (`yyyy`, `mm`, `[h]`…): letter, count, elapsed.
    Date(char, usize, bool),
    Frac(usize),
    AmPm,
    /// `.0` after seconds.
    SecFrac(usize),
    /// Number part characters: `0#?,.%E+-/`.
    Num(String),
    Text,
    Fill(char),
    Blank(char),
}

/// Whether a section holds date or time letters outside quotes and brackets.
fn is_date_section(chars: &[char]) -> bool {
    let mut quoted = false;
    let mut bracket = false;
    let mut k = 0;
    while let Some(&c) = chars.get(k) {
        match c {
            '"' if !bracket => quoted = !quoted,
            '[' if !quoted => {
                bracket = true;
                // `[h]`, `[mm]`, `[ss]` are elapsed time.
                if chars.get(k + 1).is_some_and(|c| matches!(c.to_ascii_lowercase(), 'h' | 'm' | 's')) {
                    return true;
                }
            }
            ']' if !quoted => bracket = false,
            '\\' | '_' | '*' if !quoted && !bracket => k += 1,
            c if !quoted && !bracket && matches!(c.to_ascii_lowercase(), 'y' | 'd' | 'h' | 's' | 'm') => return true,
            _ => {}
        }
        k += 1;
    }
    false
}

/// A `[…]` part: currency/locale, condition, elapsed time or colour.
fn bracket_token(s: String) -> Tok {
    let lower = s.to_ascii_lowercase();
    let first = lower.chars().next();
    if let Some(cur) = s.strip_prefix('$') {
        Tok::Currency(cur.split('-').next().unwrap_or("").to_string())
    } else if s.starts_with(['<', '>', '=']) {
        Tok::Cond(s)
    } else if let Some(l @ ('h' | 'm' | 's')) = first
        && lower.chars().all(|c| c == l)
    {
        Tok::Date(l, lower.len(), true)
    } else {
        Tok::Color(s)
    }
}

struct Lexer {
    chars: Vec<char>,
    i: usize,
    toks: Vec<Tok>,
    date: bool,
}

impl Lexer {
    fn lit(&mut self, s: &str) {
        match self.toks.last_mut() {
            Some(Tok::Lit(t)) => t.push_str(s),
            _ => self.toks.push(Tok::Lit(s.to_string())),
        }
    }
    fn until(&mut self, end: char) -> String {
        let mut s = String::new();
        while let Some(&ch) = self.chars.get(self.i) {
            self.i += 1;
            if ch == end {
                break;
            }
            s.push(ch);
        }
        s
    }
    fn starts(&self, word: &str) -> bool {
        self.chars.get(self.i..self.i + word.len()).is_some_and(|w| w.iter().collect::<String>().eq_ignore_ascii_case(word))
    }
    fn num(&mut self, s: &str) {
        match self.toks.last_mut() {
            Some(Tok::Num(t)) => t.push_str(s),
            _ => self.toks.push(Tok::Num(s.to_string())),
        }
    }
    /// A date or time token at the cursor, if any.
    fn date_token(&mut self, c: char) -> bool {
        if c.eq_ignore_ascii_case(&'a') && (self.starts("am/pm") || self.starts("a/p")) {
            self.i += if self.starts("am/pm") { 5 } else { 3 };
            self.toks.push(Tok::AmPm);
            return true;
        }
        let l = c.to_ascii_lowercase();
        if matches!(l, 'y' | 'm' | 'd' | 'h' | 's' | 'e' | 'g') {
            let mut n = 0;
            while self.chars.get(self.i).is_some_and(|x| x.to_ascii_lowercase() == l) {
                n += 1;
                self.i += 1;
            }
            self.toks.push(Tok::Date(l, n, false));
            return true;
        }
        if c == '.' && self.chars.get(self.i + 1) == Some(&'0') {
            let mut n = 0;
            self.i += 1;
            while self.chars.get(self.i) == Some(&'0') {
                n += 1;
                self.i += 1;
            }
            self.toks.push(Tok::SecFrac(n));
            return true;
        }
        false
    }
    /// A number token at the cursor, if any.
    fn number_token(&mut self, c: char) -> bool {
        match c {
            '0' | '#' | '?' | '.' | ',' | '%' => {
                self.num(c.encode_utf8(&mut [0; 4]));
                self.i += 1;
            }
            'E' | 'e' if matches!(self.chars.get(self.i + 1), Some('+' | '-')) => {
                let sign = self.chars.get(self.i + 1).copied().unwrap_or('+');
                self.i += 2;
                self.num(&format!("E{sign}"));
            }
            '/' if matches!(self.toks.last(), Some(Tok::Num(_))) => {
                let mut den = String::new();
                self.i += 1;
                while let Some(&d) = self.chars.get(self.i).filter(|c| c.is_ascii_digit() || matches!(c, '?' | '#')) {
                    den.push(d);
                    self.i += 1;
                }
                let fixed = !den.is_empty() && den.chars().all(|c| c.is_ascii_digit());
                self.toks.push(Tok::Frac(if fixed { den.parse().unwrap_or(0) } else { den.len().max(1) + 1_000_000 }));
            }
            _ => return false,
        }
        true
    }
}

fn tokenize(section: &str) -> Vec<Tok> {
    let chars: Vec<char> = section.chars().collect();
    let date = is_date_section(&chars);
    let mut lx = Lexer { chars, i: 0, toks: Vec::new(), date };
    while let Some(&c) = lx.chars.get(lx.i) {
        match c {
            '"' => {
                lx.i += 1;
                let s = lx.until('"');
                lx.lit(&s);
            }
            '\\' => {
                if let Some(&n) = lx.chars.get(lx.i + 1) {
                    lx.lit(n.encode_utf8(&mut [0; 4]));
                }
                lx.i += 2;
            }
            '_' | '*' => {
                let n = lx.chars.get(lx.i + 1).copied().unwrap_or(' ');
                lx.toks.push(if c == '_' { Tok::Blank(n) } else { Tok::Fill(n) });
                lx.i += 2;
            }
            '[' => {
                lx.i += 1;
                let s = lx.until(']');
                lx.toks.push(bracket_token(s));
            }
            '@' => {
                lx.toks.push(Tok::Text);
                lx.i += 1;
            }
            c if lx.date && lx.date_token(c) => {}
            c if !lx.date && lx.number_token(c) => {}
            c => {
                lx.lit(c.encode_utf8(&mut [0; 4]));
                lx.i += 1;
            }
        }
    }
    lx.toks
}

fn text_el(out: &mut String, t: &str) {
    if !t.is_empty() {
        let _ = write!(out, "<number:text>{}</number:text>", crate::xml::esc(t));
    }
}

/// Writes the elements of a number part (`#,##0.00`, `0.0E+00`, `# ?/?`).
/// The integer part of a fraction (`#`, `0`, `??0`, `#,##0`): min-integer-digits, blank
/// digits and grouping.
fn fraction_int(int: &str) -> String {
    let min = int.chars().filter(|c| *c == '0' || *c == '?').count();
    let blank = int.chars().filter(|c| *c == '?').count();
    let mut o = format!(" number:min-integer-digits=\"{min}\"");
    if blank > 0 {
        let _ = write!(o, " loext:max-blank-integer-digits=\"{blank}\"");
    }
    if int.contains(',') {
        o.push_str(" number:grouping=\"true\"");
    }
    o
}

fn number_el(out: &mut String, num: &str, frac: Option<usize>, int_lits: &[(usize, String)]) {
    let (mantissa, exp) = match num.find(['E', 'e']) {
        Some(i) => (num.get(..i).unwrap_or(num), num.get(i..)),
        None => (num, None),
    };
    let trimmed = mantissa.trim_end_matches(',');
    let factor_commas = mantissa.len() - trimmed.len();
    let (int, dec) = match trimmed.split_once('.') {
        Some((a, b)) => (a, Some(b)),
        None => (trimmed, None),
    };
    let grouping = int.contains(',');
    let int_digits: Vec<char> = int.chars().filter(|c| matches!(c, '0' | '#' | '?')).collect();
    let min_int = int_digits.iter().filter(|c| **c == '0' || **c == '?').count();
    let blank = int_digits.iter().filter(|c| **c == '?').count();
    let dec_digits: Vec<char> = dec.unwrap_or("").chars().filter(|c| matches!(c, '0' | '#' | '?')).collect();
    let decimals = dec_digits.len();
    let min_dec = dec_digits.iter().take_while(|c| **c == '0').count();
    if let Some(den) = frac {
        // `# ?/?`: the integer digits were split off by the tokenizer; `num` is the numerator.
        let _ = write!(out, "<number:fraction number:min-numerator-digits=\"{}\"", int_digits.len().max(1));
        if let Some((_, int)) = int_lits.first() {
            out.push_str(&fraction_int(int));
        }
        if den >= 1_000_000 {
            let d = den - 1_000_000;
            let _ = write!(out, " number:min-denominator-digits=\"{d}\" number:max-denominator-value=\"{}\"", "9".repeat(d.max(1)));
        } else {
            let _ = write!(out, " number:denominator-value=\"{den}\"");
        }
        out.push_str("/>");
        return;
    }
    if let Some(exp) = exp {
        let exp_digits = exp.chars().filter(|c| *c == '0').count().max(1);
        let interval = if grouping { 1 } else { int_digits.len().max(1) };
        if grouping {
            let _ = write!(
                out,
                "<number:scientific-number number:decimal-places=\"{decimals}\" number:min-decimal-places=\"{min_dec}\" number:min-integer-digits=\"{}\" number:grouping=\"true\" number:min-exponent-digits=\"{exp_digits}\" number:exponent-interval=\"1\" number:forced-exponent-sign=\"{}\"/>",
                min_int.max(1),
                exp.contains('+')
            );
            return;
        }
        let blank_attr = if blank > 0 { format!(" loext:max-blank-integer-digits=\"{blank}\"") } else { String::new() };
        let _ = write!(
            out,
            "<number:scientific-number number:decimal-places=\"{decimals}\" number:min-decimal-places=\"{min_dec}\" number:min-integer-digits=\"{}\"{blank_attr} number:min-exponent-digits=\"{exp_digits}\" number:exponent-interval=\"{interval}\" number:forced-exponent-sign=\"{}\"/>",
            min_int.max(1),
            exp.contains('+')
        );
        return;
    }
    let _ = write!(
        out,
        "<number:number number:decimal-places=\"{decimals}\" number:min-decimal-places=\"{min_dec}\" number:min-integer-digits=\"{min_int}\""
    );
    if blank > 0 {
        let _ = write!(out, " loext:max-blank-integer-digits=\"{blank}\"");
    }
    if grouping {
        out.push_str(" number:grouping=\"true\"");
    }
    if factor_commas > 0 {
        let _ = write!(out, " number:display-factor=\"{}\"", 1000u64.saturating_pow(factor_commas.min(6) as u32));
    }
    if int_lits.is_empty() {
        out.push_str("/>");
    } else {
        out.push('>');
        for (pos, t) in int_lits {
            let _ = write!(out, "<number:embedded-text number:position=\"{pos}\">{}</number:embedded-text>", crate::xml::esc(t));
        }
        out.push_str("</number:number>");
    }
}

fn color_rgb(name: &str) -> Option<&'static str> {
    Some(match name.to_ascii_lowercase().as_str() {
        "red" => "#ff0000",
        "blue" => "#0000ff",
        "green" => "#00ff00",
        "black" => "#000000",
        "white" => "#ffffff",
        "yellow" => "#ffff00",
        "magenta" => "#ff00ff",
        "cyan" => "#00ffff",
        _ => return None,
    })
}

/// One section as a data style element named `name`; returns the element and its condition.
/// Which tokens of a section are `m` meaning minutes: after hours, before seconds, or elapsed.
fn minutes_context(toks: &[Tok]) -> Vec<bool> {
    let mut ctx = vec![false; toks.len()];
    for (i, t) in toks.iter().enumerate() {
        if let Tok::Date('m', _, elapsed) = t {
            let prev_h =
                toks.get(..i).and_then(|p| p.iter().rev().find(|t| matches!(t, Tok::Date(..)))).is_some_and(|t| matches!(t, Tok::Date('h', ..)));
            let next_s =
                toks.get(i + 1..).and_then(|p| p.iter().find(|t| matches!(t, Tok::Date(..)))).is_some_and(|t| matches!(t, Tok::Date('s', ..)));
            if let Some(m) = ctx.get_mut(i) {
                *m = prev_h || next_s || *elapsed;
            }
        }
    }
    ctx
}

/// The data style element a section's tokens call for.
fn section_kind(toks: &[Tok], minutes: &[bool]) -> &'static str {
    let has = |f: &dyn Fn(&Tok) -> bool| toks.iter().any(f);
    let has_date_part = toks.iter().enumerate().any(|(i, t)| match t {
        Tok::Date('y' | 'd' | 'e' | 'g', ..) => true,
        Tok::Date('m', ..) => !minutes.get(i).copied().unwrap_or(false),
        _ => false,
    });
    if has(&|t| matches!(t, Tok::Text)) {
        "text-style"
    } else if has(&|t| matches!(t, Tok::Date(..) | Tok::AmPm)) {
        if has_date_part { "date-style" } else { "time-style" }
    } else if has(&|t| matches!(t, Tok::Num(n) if n.contains('%'))) {
        "percentage-style"
    } else if has(&|t| matches!(t, Tok::Currency(_))) || has(&|t| matches!(t, Tok::Lit(l) if l.contains(['$', '€', '£', '¥']))) {
        "currency-style"
    } else {
        "number-style"
    }
}

/// A date or time token as its element.
fn date_el(body: &mut String, letter: char, n: usize, minutes: bool, next: Option<&Tok>) {
    let long = if n >= 2 { " number:style=\"long\"" } else { "" };
    let longer = |at: usize| if n >= at { " number:style=\"long\"" } else { "" };
    let _ = match letter {
        'y' => write!(body, "<number:year{}/>", longer(3)),
        'd' if n >= 3 => write!(body, "<number:day-of-week{}/>", longer(4)),
        'd' => write!(body, "<number:day{long}/>"),
        'm' if minutes => write!(body, "<number:minutes{long}/>"),
        'm' if n >= 3 => write!(body, "<number:month{} number:textual=\"true\"/>", longer(4)),
        'm' => write!(body, "<number:month{long}/>"),
        'h' => write!(body, "<number:hours{long}/>"),
        's' => {
            let frac = match next {
                Some(Tok::SecFrac(d)) => format!(" number:decimal-places=\"{d}\""),
                _ => String::new(),
            };
            write!(body, "<number:seconds{long}{frac}/>")
        }
        'e' | 'g' => write!(body, "<number:era{long}/>"),
        _ => Ok(()),
    };
}

/// The number run starting at token `i` (fraction, digits broken by literals, plain digits);
/// returns the index after it.
fn number_run(body: &mut String, toks: &[Tok], i: usize, n: &str) -> usize {
    // A fraction: `# ?/?` arrives as Num("#"), Lit(" "), Num("?"), Frac(den).
    if let Some(Tok::Frac(den)) = toks.get(i + 1) {
        number_el(body, n, Some(*den), &[]);
        return i + 2;
    }
    if let (Some(Tok::Lit(sp)), Some(Tok::Num(numer)), Some(Tok::Frac(den))) = (toks.get(i + 1), toks.get(i + 2), toks.get(i + 3))
        && sp.trim().is_empty()
    {
        number_el(body, numer, Some(*den), &[(0, n.to_string())]);
        return i + 4;
    }
    // Integer digits broken up by literals (`000-00-0000`): one number with embedded text.
    let mut j = i + 1;
    let mut parts: Vec<(String, Option<String>)> = vec![(n.to_string(), None)];
    while let (Some(Tok::Lit(l)), Some(Tok::Num(m))) = (toks.get(j), toks.get(j + 1)) {
        if l.chars().count() > 3 || n.contains('.') {
            break;
        }
        parts.push((m.clone(), Some(l.clone())));
        j += 2;
    }
    let count = |d: &str| d.chars().filter(|c| matches!(c, '0' | '#' | '?')).count();
    if parts.len() > 1 {
        let mut all = String::new();
        let mut lits: Vec<(usize, String)> = Vec::new();
        // Positions count integer digits from the right.
        let total: usize = parts.iter().map(|(d, _)| count(d)).sum();
        let mut seen = 0;
        for (digits, lit) in &parts {
            if let Some(l) = lit {
                lits.push((total - seen, l.clone()));
            }
            seen += count(digits);
            all.push_str(digits);
        }
        number_el(body, &all.replace('%', ""), None, &lits);
        if all.contains('%') {
            text_el(body, "%");
        }
        return j;
    }
    let digits = n.replace('%', "");
    if count(&digits) > 0 {
        number_el(body, &digits, None, &[]);
    } else {
        text_el(body, &digits);
    }
    if n.contains('%') {
        text_el(body, "%");
    }
    i + 1
}

/// A General section, which is LibreOffice's standard number, possibly coloured.
fn general_section(name: &str, section: &str, vol: &str) -> Option<String> {
    let bare = section.trim_start_matches(|c| c != ']').trim_start_matches(']');
    if !(section.eq_ignore_ascii_case("general") || (section.starts_with('[') && bare.eq_ignore_ascii_case("general"))) {
        return None;
    }
    let color = tokenize(section).iter().find_map(|t| if let Tok::Color(c) = t { color_rgb(c) } else { None });
    let props = color.map(|c| format!("<style:text-properties fo:color=\"{c}\"/>")).unwrap_or_default();
    Some(format!("<number:number-style style:name=\"{}\"{vol}>{props}<number:number number:min-integer-digits=\"1\"/>", esc_attr(name)))
}

/// One section as a data style element named `name`; returns the element and its condition.
fn section_xml(name: &str, section: &str, volatile: bool) -> (String, Option<String>) {
    let vol = if volatile { " style:volatile=\"true\"" } else { "" };
    if let Some(xml) = general_section(name, section, vol) {
        return (xml, None);
    }
    let toks = tokenize(section);
    let minutes = minutes_context(&toks);
    let kind = section_kind(&toks, &minutes);
    let explicit_currency = toks.iter().any(|t| matches!(t, Tok::Currency(_)));
    let mut body = String::new();
    let mut cond = None;
    let mut i = 0;
    while let Some(t) = toks.get(i) {
        match t {
            // A bare `$` of a currency format is its symbol.
            Tok::Lit(l) if kind == "currency-style" && !explicit_currency && l.contains('$') => {
                let (a, b) = l.split_once('$').unwrap_or((l, ""));
                text_el(&mut body, a);
                body.push_str("<number:currency-symbol>$</number:currency-symbol>");
                text_el(&mut body, b);
            }
            Tok::Lit(l) => text_el(&mut body, l),
            Tok::Color(c) => {
                if let Some(rgb) = color_rgb(c) {
                    body.insert_str(0, &format!("<style:text-properties fo:color=\"{rgb}\"/>"));
                }
            }
            Tok::Cond(c) => cond = Some(c.clone()),
            Tok::Currency(sym) => {
                let _ = write!(body, "<number:currency-symbol>{}</number:currency-symbol>", crate::xml::esc(sym));
            }
            Tok::Blank(c) => {
                let _ = write!(body, "<number:text loext:blank-width-char=\"{}\"> </number:text>", esc_attr(&c.to_string()));
            }
            Tok::Fill(c) => {
                let _ = write!(body, "<number:fill-character>{}</number:fill-character>", crate::xml::esc(&c.to_string()));
            }
            Tok::Text => body.push_str("<number:text-content/>"),
            Tok::AmPm => body.push_str("<number:am-pm/>"),
            Tok::SecFrac(_) | Tok::Frac(_) => {}
            Tok::Date(letter, n, _) => date_el(&mut body, *letter, *n, minutes.get(i).copied().unwrap_or(false), toks.get(i + 1)),
            Tok::Num(n) => {
                i = number_run(&mut body, &toks, i, n);
                continue;
            }
        }
        i += 1;
    }
    let elapsed = toks.iter().any(|t| matches!(t, Tok::Date(_, _, true)));
    let truncate = if kind == "time-style" && elapsed { " number:truncate-on-overflow=\"false\"" } else { "" };
    (format!("<number:{kind} style:name=\"{}\"{vol}{truncate}>{body}", esc_attr(name)), cond.map(|c| format!("{kind}:{c}")))
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
    if code.eq_ignore_ascii_case("general") || code.is_empty() {
        return ValueKind::Float;
    }
    let first = sections(code).into_iter().next().unwrap_or_default();
    let toks = tokenize(&first);
    match section_kind(&toks, &minutes_context(&toks)) {
        "date-style" => ValueKind::Date,
        "time-style" => ValueKind::Time,
        "percentage-style" => ValueKind::Percent,
        _ => ValueKind::Float,
    }
}

/// The data style elements for `code`, the main one named `name`. `None` for General.
pub(crate) fn write_style(name: &str, code: &str) -> Option<String> {
    if code.eq_ignore_ascii_case("general") || code.is_empty() {
        return None;
    }
    let secs: Vec<String> = sections(code).into_iter().take(4).collect();
    let mut out = String::new();
    let n = secs.len();
    let conds_explicit = secs.first().is_some_and(|s| s.contains("[<") || s.contains("[>") || s.contains("[="));
    let mut maps = String::new();
    for (i, s) in secs.iter().enumerate().take(n.saturating_sub(1)) {
        let part = format!("{name}P{i}");
        let (xml, cond) = section_xml(&part, s, true);
        let kind = xml.split_whitespace().next().unwrap_or("<number:number-style").trim_start_matches('<').to_string();
        let _ = write!(out, "{xml}</{kind}>");
        let condition = if conds_explicit {
            cond.and_then(|c| c.split_once(':').map(|(_, c)| c.to_string())).unwrap_or_else(|| ">=0".into())
        } else {
            match (n, i) {
                (2, 0) => ">=0".into(),
                (_, 0) => ">0".into(),
                (_, 1) => "<0".into(),
                _ => "=0".into(),
            }
        };
        let _ = write!(maps, "<style:map style:condition=\"value(){}\" style:apply-style-name=\"{}\"/>", esc_attr(&condition), esc_attr(&part));
    }
    let main = secs.last().map(String::as_str).unwrap_or("General");
    let main = if main.is_empty() && n > 1 { "\"\"" } else { main };
    let (xml, _) = section_xml(name, main, false);
    let kind = xml.split_whitespace().next().unwrap_or("<number:number-style").trim_start_matches('<').to_string();
    let _ = write!(out, "{xml}{maps}</{kind}>");
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::super::dom::parse_tree;
    use super::*;

    fn round_trip(code: &str) -> String {
        let Some(xml) = write_style("N1", code) else { return "General".into() };
        let doc = format!(
            r#"<office:styles xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:loext="urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0">{xml}</office:styles>"#
        );
        let Ok(tree) = parse_tree(doc.as_bytes()) else { return format!("bad xml: {doc}") };
        let mut styles = HashMap::new();
        read_styles(&tree, &mut styles);
        code_of(&styles, "N1").unwrap_or_default()
    }

    fn code_of(styles: &HashMap<String, DataStyle>, name: &str) -> Option<String> {
        code(styles, name)
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
}
