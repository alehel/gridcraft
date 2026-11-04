//! OpenFormula (ODF 1.2+ part 2, `of:=`) ↔ GridCraft's A1 formula syntax.
//!
//! Reading translates the file's text token by token into A1 text that the formula parser takes:
//! bracketed references (`[.A1:.B2]`, `[$'Sheet 1'.A1]`) become A1 references, `;` separators
//! become `,`, inline-array separators (`;` and `|`) become `,` and `;`, the union `~` and the
//! intersection `!` operators become `,` and a space, and ODF function names become Excel names.
//! Writing prints the parsed expression the other way.

use std::fmt::Write as _;

use gridcraft_core::{CellRef, col_to_letters, letters_to_col, number_to_text};
use gridcraft_formula::{BinOp, Expr, RefKind, Reference, SheetSel, StructItem, UnOp};

use super::names::{excel_to_odf, odf_to_excel};

/// Longest formula text translated; longer text is kept unevaluated.
const MAX_LEN: usize = 64 * 1024;

/// Translates an ODF formula attribute (`of:=SUM([.A1:.B2])`) into A1 text without the `=`.
/// `None` when the text is not an OpenFormula formula GridCraft can translate.
pub(crate) fn from_odf(src: &str) -> Option<String> {
    if src.len() > MAX_LEN {
        return None;
    }
    let body = strip_namespace(src)?;
    let body = body.strip_prefix('=').unwrap_or(body);
    translate(body)
}

/// The formula text after its namespace prefix (`of:`, `oooc:`, `msoxl:`). Formulas in another
/// syntax than OpenFormula (or the legacy OpenOffice syntax it grew from) are refused.
fn strip_namespace(src: &str) -> Option<&str> {
    let src = src.trim_start();
    // A prefix without `=` (`of:#N/A`) is still the namespace.
    for p in ["of:", "oooc:", "ooow:", "msoxl:"] {
        if let Some(rest) = src.strip_prefix(p)
            && !rest.starts_with('=')
        {
            return Some(rest);
        }
    }
    if let Some(i) = src.find(":=")
        && src.get(..i).is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    {
        let prefix = src.get(..i)?;
        let rest = src.get(i + 1..)?;
        return match prefix {
            "of" | "oooc" | "ooow" => Some(rest),
            // Excel's own syntax (written by Excel into ODS) is already what the parser takes.
            "msoxl" => Some(rest),
            _ => None,
        };
    }
    Some(src)
}

fn translate(body: &str) -> Option<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len());
    let mut i = 0usize;
    // Brace depth: separators inside an inline array are column/row separators.
    let mut braces = 0usize;
    while let Some(&c) = chars.get(i) {
        match c {
            '"' => {
                out.push('"');
                i += 1;
                loop {
                    let ch = *chars.get(i)?;
                    out.push(ch);
                    i += 1;
                    if ch == '"' {
                        if chars.get(i) == Some(&'"') {
                            out.push('"');
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            '[' => {
                let start = i + 1;
                let mut j = start;
                let mut quoted = false;
                loop {
                    let ch = *chars.get(j)?;
                    if ch == '\'' {
                        // '' inside quotes is an escaped quote.
                        if quoted && chars.get(j + 1) == Some(&'\'') {
                            j += 2;
                            continue;
                        }
                        quoted = !quoted;
                    } else if ch == ']' && !quoted {
                        break;
                    }
                    j += 1;
                }
                let inner: String = chars.get(start..j)?.iter().collect();
                out.push_str(&reference(&inner));
                i = j + 1;
            }
            '{' => {
                braces += 1;
                out.push('{');
                i += 1;
            }
            '}' => {
                braces = braces.saturating_sub(1);
                out.push('}');
                i += 1;
            }
            ';' => {
                out.push(',');
                i += 1;
            }
            '|' if braces > 0 => {
                out.push(';');
                i += 1;
            }
            '~' => {
                out.push(',');
                i += 1;
            }
            '!' => {
                out.push(' ');
                i += 1;
            }
            '#' => {
                // An error literal (`#N/A`, `#DIV/0!`, `#NAME?`): copied whole, so its `!` is kept.
                let mut j = i + 1;
                while let Some(&ch) = chars.get(j) {
                    if ch.is_alphanumeric() || ch == '/' || ch == '_' {
                        j += 1;
                    } else {
                        if ch == '!' || ch == '?' {
                            j += 1;
                        }
                        break;
                    }
                }
                out.extend(chars.get(i..j)?.iter());
                i = j;
            }
            c if c.is_alphabetic() || c == '_' || c == '\\' => {
                let mut j = i;
                while let Some(&ch) = chars.get(j) {
                    if ch.is_alphanumeric() || ch == '_' || ch == '.' || ch == '\\' {
                        j += 1;
                    } else {
                        break;
                    }
                }
                let word: String = chars.get(i..j)?.iter().collect();
                // A function name is followed (possibly after spaces) by `(`.
                let mut k = j;
                while chars.get(k).is_some_and(|c| *c == ' ') {
                    k += 1;
                }
                if chars.get(k) == Some(&'(') {
                    out.push_str(&odf_to_excel(&word));
                } else {
                    out.push_str(&word);
                }
                i = j;
            }
            c if c.is_ascii_digit() => {
                // Numbers, including exponents (`1E+5`), copied whole so `E` isn't a name.
                let mut j = i;
                while let Some(&ch) = chars.get(j) {
                    let exp_sign = (ch == '+' || ch == '-') && j > i && matches!(chars.get(j - 1), Some('e' | 'E'));
                    if ch.is_ascii_digit() || ch == '.' || ch == 'e' || ch == 'E' || exp_sign {
                        j += 1;
                    } else {
                        break;
                    }
                }
                out.extend(chars.get(i..j)?.iter());
                i = j;
            }
            '\n' | '\r' | '\t' => {
                out.push(' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some(out)
}

/// One end of an ODF reference: optional sheet, then column and row parts.
struct OdfEnd {
    sheet: Option<String>,
    col: Option<(u32, bool)>,
    row: Option<(u32, bool)>,
    error: bool,
}

/// Splits `s` at the last `.` outside quotes: sheet part and cell part.
fn split_sheet(s: &str) -> (Option<&str>, &str) {
    let mut quoted = false;
    let mut at = None;
    let bytes = s.as_bytes();
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'\'' => quoted = !quoted,
            b'.' if !quoted => at = Some(i),
            _ => {}
        }
        i += 1;
    }
    match at {
        Some(i) => (s.get(..i), s.get(i + 1..).unwrap_or_default()),
        None => (None, s),
    }
}

fn unquote_sheet(s: &str) -> String {
    let s = s.strip_prefix('$').unwrap_or(s);
    match s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
        Some(inner) => inner.replace("''", "'"),
        None => s.to_string(),
    }
}

fn parse_end(s: &str) -> Option<OdfEnd> {
    let (sheet, cell) = split_sheet(s.trim());
    let sheet = match sheet {
        // `[.A1]`: the formula's own sheet.
        Some("") | None => None,
        Some(sh) => Some(unquote_sheet(sh)),
    };
    // A deleted sheet or cell, in any language (`#REF!`, `#RIF!`, `#BEZUG!`).
    if cell.contains('#') || sheet.as_deref().is_some_and(|s| s.starts_with('#')) {
        return Some(OdfEnd { sheet, col: None, row: None, error: true });
    }
    let b = cell.as_bytes();
    let mut i = 0;
    let col_abs = b.first() == Some(&b'$');
    if col_abs {
        i += 1;
    }
    let cs = i;
    while b.get(i).is_some_and(u8::is_ascii_alphabetic) {
        i += 1;
    }
    let col = if i > cs { Some((letters_to_col(&cell.get(cs..i)?.to_ascii_uppercase())?, col_abs)) } else { None };
    let row_abs = b.get(i) == Some(&b'$') || (col.is_none() && col_abs);
    if b.get(i) == Some(&b'$') {
        i += 1;
    }
    let rs = i;
    while b.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let row = if i > rs {
        let n: u32 = cell.get(rs..i)?.parse().ok()?;
        if n == 0 || n > gridcraft_core::MAX_ROWS {
            return None;
        }
        Some((n - 1, row_abs))
    } else {
        None
    };
    if i != b.len() || (col.is_none() && row.is_none()) {
        return None;
    }
    Some(OdfEnd { sheet, col, row, error: false })
}

fn a1_end(e: &OdfEnd) -> String {
    let mut s = String::new();
    if let Some((c, abs)) = e.col {
        if abs {
            s.push('$');
        }
        s.push_str(&col_to_letters(c));
    }
    if let Some((r, abs)) = e.row {
        if abs {
            s.push('$');
        }
        let _ = write!(s, "{}", r as u64 + 1);
    }
    s
}

/// A bracketed ODF reference (without brackets) as A1 text; `#REF!` for a reference that
/// doesn't translate (a deleted cell, an external document, a malformed reference).
pub(crate) fn reference(inner: &str) -> String {
    let inner = inner.trim();
    // External references (`'file:///x.ods'#$Sheet1.A1`) are not modelled.
    if inner.starts_with('\'') && inner.contains("'#") {
        return "#REF!".into();
    }
    let (a, b) = match split_range(inner) {
        Some((a, b)) => (a, Some(b)),
        None => (inner, None),
    };
    let Some(first) = parse_end(a) else { return "#REF!".into() };
    let second = match b {
        Some(b) => match parse_end(b) {
            Some(e) => Some(e),
            None => return "#REF!".into(),
        },
        None => None,
    };
    if first.error || second.as_ref().is_some_and(|s| s.error) {
        return "#REF!".into();
    }
    let sheet_prefix = |sheet: &Option<String>| sheet.as_ref().map(|s| format!("{}!", gridcraft_formula::quote_sheet(s))).unwrap_or_default();
    match second {
        None => {
            if first.col.is_none() || first.row.is_none() {
                return "#REF!".into();
            }
            format!("{}{}", sheet_prefix(&first.sheet), a1_end(&first))
        }
        Some(second) => {
            let prefix = match (&first.sheet, &second.sheet) {
                (Some(x), Some(y)) if x != y => {
                    let joined = format!("{x}:{y}");
                    if gridcraft_formula::printer::needs_quotes(x) || gridcraft_formula::printer::needs_quotes(y) {
                        format!("'{}'!", joined.replace('\'', "''"))
                    } else {
                        format!("{joined}!")
                    }
                }
                (Some(_), _) => sheet_prefix(&first.sheet),
                (None, Some(_)) => sheet_prefix(&second.sheet),
                (None, None) => String::new(),
            };
            // Both ends must be the same kind: cells, whole columns or whole rows.
            let kind = |e: &OdfEnd| (e.col.is_some(), e.row.is_some());
            if kind(&first) != kind(&second) {
                return "#REF!".into();
            }
            // A whole column (rows 1 to the last row) is written as a column reference.
            let last = gridcraft_core::MAX_ROWS - 1;
            if let (Some((0, a0)), Some((l, a1)), Some(c0), Some(c1)) = (first.row, second.row, first.col, second.col)
                && l == last
                && a0 == a1
            {
                let col = |(c, abs): (u32, bool)| format!("{}{}", if abs { "$" } else { "" }, col_to_letters(c));
                return format!("{prefix}{}:{}", col(c0), col(c1));
            }
            format!("{prefix}{}:{}", a1_end(&first), a1_end(&second))
        }
    }
}

/// The addresses of a space-separated list (`Sheet1.A1:Sheet1.B2 'My sheet'.C3`); spaces inside
/// quoted sheet names belong to the name.
pub(crate) fn split_addresses(list: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (i, c) in list.char_indices() {
        match c {
            '\'' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if let Some(part) = list.get(start..i).filter(|p| !p.is_empty()) {
                    out.push(part);
                }
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    if let Some(part) = list.get(start..).filter(|p| !p.is_empty()) {
        out.push(part);
    }
    out
}

/// Splits a range at its `:` outside quotes.
fn split_range(s: &str) -> Option<(&str, &str)> {
    let mut quoted = false;
    for (i, ch) in s.char_indices() {
        match ch {
            '\'' => quoted = !quoted,
            ':' if !quoted => return Some((s.get(..i)?, s.get(i + 1..)?)),
            _ => {}
        }
    }
    None
}

// ------------------------------------------------------------------------------------- writing

/// Looks up a table's ranges for structured references, which ODF formulas have no syntax for.
pub(crate) trait Tables {
    /// The sheet and range a structured reference covers at `at`, or `None` (written as `#REF!`).
    fn resolve(&self, s: &gridcraft_formula::StructRef, at: CellRef) -> Option<(String, gridcraft_core::RangeRef)>;
}

/// Prints `e` as OpenFormula with the `of:=` prefix.
pub(crate) fn to_odf(e: &Expr, at: CellRef, tables: &dyn Tables) -> String {
    let mut s = String::from("of:=");
    write(&mut s, e, at, tables);
    s
}

fn quote_odf_sheet(name: &str) -> String {
    let plain = name.chars().all(|c| c.is_alphanumeric() || c == '_') && !name.chars().next().is_some_and(|c| c.is_ascii_digit());
    if plain { name.to_string() } else { format!("'{}'", name.replace('\'', "''")) }
}

fn push_col(s: &mut String, col: u32, abs: bool) {
    if abs {
        s.push('$');
    }
    s.push_str(&col_to_letters(col));
}

fn push_row(s: &mut String, row: u32, abs: bool) {
    if abs {
        s.push('$');
    }
    let _ = write!(s, "{}", row as u64 + 1);
}

fn push_sheet(s: &mut String, sheet: Option<&str>) {
    match sheet {
        Some(name) => {
            s.push('$');
            s.push_str(&quote_odf_sheet(name));
            s.push('.');
        }
        None => s.push('.'),
    }
}

pub(crate) fn write_ref(s: &mut String, r: &Reference) {
    let (first, last) = match &r.sheet {
        SheetSel::Current => (None, None),
        SheetSel::Named(n) => (Some(n.as_str()), None),
        SheetSel::Span(a, b) => (Some(a.as_str()), Some(b.as_str())),
    };
    s.push('[');
    push_sheet(s, first);
    match &r.kind {
        RefKind::Cell(a) => {
            push_col(s, a.col, a.col_abs);
            push_row(s, a.row, a.row_abs);
            if let Some(last) = last {
                s.push(':');
                push_sheet(s, Some(last));
                push_col(s, a.col, a.col_abs);
                push_row(s, a.row, a.row_abs);
            }
        }
        RefKind::Range(a, b) => {
            push_col(s, a.col, a.col_abs);
            push_row(s, a.row, a.row_abs);
            s.push(':');
            push_sheet(s, last);
            push_col(s, b.col, b.col_abs);
            push_row(s, b.row, b.row_abs);
        }
        RefKind::Cols(c0, a0, c1, a1) => {
            push_col(s, *c0, *a0);
            s.push(':');
            push_sheet(s, last);
            push_col(s, *c1, *a1);
        }
        RefKind::Rows(r0, a0, r1, a1) => {
            push_row(s, *r0, *a0);
            s.push(':');
            push_sheet(s, last);
            push_row(s, *r1, *a1);
        }
    }
    s.push(']');
}

fn number(n: f64) -> String {
    let t = number_to_text(n);
    if t.parse::<f64>().is_ok_and(f64::is_finite) { t } else { format!("{n:E}") }
}

fn write(s: &mut String, e: &Expr, at: CellRef, tables: &dyn Tables) {
    match e {
        Expr::Number(n) => s.push_str(&number(*n)),
        Expr::Text(t) => {
            s.push('"');
            s.push_str(&t.replace('"', "\"\""));
            s.push('"');
        }
        Expr::Bool(b) => s.push_str(if *b { "TRUE()" } else { "FALSE()" }),
        Expr::Error(err) => s.push_str(err.as_str()),
        Expr::Array(rows) => {
            s.push('{');
            for (ri, row) in rows.iter().enumerate() {
                if ri > 0 {
                    s.push('|');
                }
                for (ci, el) in row.iter().enumerate() {
                    if ci > 0 {
                        s.push(';');
                    }
                    write(s, el, at, tables);
                }
            }
            s.push('}');
        }
        Expr::Ref(r) => write_ref(s, r),
        // A sheet-qualified name has no ODF spelling: the name alone.
        Expr::Name(n) => s.push_str(n.rsplit('!').next().unwrap_or(n)),
        Expr::Struct(st) => match tables.resolve(st, at) {
            Some((sheet, range)) => {
                let a = gridcraft_formula::Anchor { row: range.start.row, col: range.start.col, row_abs: true, col_abs: true };
                let b = gridcraft_formula::Anchor { row: range.end.row, col: range.end.col, row_abs: true, col_abs: true };
                let kind =
                    if range.start == range.end && !st.specifiers.contains(&StructItem::All) { RefKind::Cell(a) } else { RefKind::Range(a, b) };
                write_ref(s, &Reference { sheet: SheetSel::Named(sheet), kind });
            }
            None => s.push_str("#REF!"),
        },
        Expr::Unary(op, x) => match op {
            UnOp::Neg => {
                s.push('-');
                write(s, x, at, tables);
            }
            UnOp::Plus => {
                s.push('+');
                write(s, x, at, tables);
            }
            // Implicit intersection is what ODF does anyway; a spill reference has no ODF form.
            UnOp::At | UnOp::Spill => write(s, x, at, tables),
            UnOp::Percent => {
                write(s, x, at, tables);
                s.push('%');
            }
            UnOp::Trim(mode) => {
                s.push_str("COM.MICROSOFT.TRIMRANGE(");
                write(s, x, at, tables);
                let _ = write!(s, ";{0};{0})", mode.code());
            }
        },
        Expr::Binary(..) => {
            let mut rest: Vec<(&BinOp, &Expr)> = Vec::new();
            let mut first = e;
            while let Expr::Binary(op, a, b) = first {
                rest.push((op, b));
                first = a;
            }
            write(s, first, at, tables);
            for (op, right) in rest.into_iter().rev() {
                s.push_str(match op {
                    BinOp::Union => "~",
                    BinOp::Intersect => "!",
                    other => other.symbol(),
                });
                write(s, right, at, tables);
            }
        }
        Expr::Call(name, args) => {
            s.push_str(&excel_to_odf(name));
            s.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    s.push(';');
                }
                write(s, a, at, tables);
            }
            s.push(')');
        }
        Expr::Invoke(c, args) => {
            write(s, c, at, tables);
            s.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    s.push(';');
                }
                write(s, a, at, tables);
            }
            s.push(')');
        }
        Expr::Missing => {}
        Expr::Paren(x) => {
            s.push('(');
            write(s, x, at, tables);
            s.push(')');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoTables;
    impl Tables for NoTables {
        fn resolve(&self, _: &gridcraft_formula::StructRef, _: CellRef) -> Option<(String, gridcraft_core::RangeRef)> {
            None
        }
    }

    fn rt(odf: &str, a1: &str) {
        let got = from_odf(odf);
        assert_eq!(got.as_deref(), Some(a1), "reading {odf}");
        let e = gridcraft_formula::parse(a1).unwrap_or(Expr::Missing);
        assert_eq!(to_odf(&e, CellRef::new(0, 0), &NoTables), odf, "writing {a1}");
    }

    #[test]
    fn references_and_separators() {
        rt("of:=SUM([.A1:.B2])", "SUM(A1:B2)");
        rt("of:=[$Sheet2.$A$1]+[.C3]", "Sheet2!$A$1+C3");
        rt("of:=SUM([$'My sheet'.A1:.B2];1)", "SUM('My sheet'!A1:B2,1)");
        rt("of:=SUM([.A:.A])", "SUM(A:A)");
        rt("of:=SUM([.$1:.$3])", "SUM($1:$3)");
        rt("of:=SUM([$Sheet1.A1:$Sheet3.B2])", "SUM(Sheet1:Sheet3!A1:B2)");
        rt("of:=IF([.A1]=\"a;b\";TRUE();FALSE())", "IF(A1=\"a;b\",TRUE(),FALSE())");
        rt("of:=SUM({1;2|3;4})", "SUM({1,2;3,4})");
        rt("of:=[.A1]&\"x\"\"y\"", "A1&\"x\"\"y\"");
        rt("of:=1E+20*-[.B1]%", "1E+20*-B1%");
        assert_eq!(from_odf("of:=SUM([.A1:.B2]~[.C1])").as_deref(), Some("SUM(A1:B2,C1)"));
        assert_eq!(from_odf("of:=[.A1:.B2]![.B1:.C3]").as_deref(), Some("A1:B2 B1:C3"));
        assert_eq!(from_odf("of:=[.#REF!]+1").as_deref(), Some("#REF!+1"));
        assert_eq!(from_odf("of:=['file:///x.ods'#$Sheet1.A1]").as_deref(), Some("#REF!"));
        assert_eq!(from_odf("of:=ISNA(#N/A)").as_deref(), Some("ISNA(#N/A)"));
        assert_eq!(from_odf("msoxl:=SUM(A1:B2)").as_deref(), Some("SUM(A1:B2)"));
        assert_eq!(from_odf("foo:=1"), None);
        assert_eq!(from_odf("of:#N/A").as_deref(), Some("#N/A"));
        assert_eq!(split_addresses("'My sheet'.A1:'My sheet'.B2  Data.C3"), vec!["'My sheet'.A1:'My sheet'.B2", "Data.C3"]);
    }

    #[test]
    fn hostile_text_does_not_panic() {
        for s in ["of:=[", "of:=\"", "of:=['", "of:=[.ZZZZ1]", "of:=[.A0]", "of:=[.A99999999999]", "of:=#", "of:=[:]", "of:=[.A1:]", "of:={|;}"] {
            let _ = from_odf(s);
        }
    }
}
