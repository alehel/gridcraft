//! OpenFormula (ODF 1.2+ part 2, `of:=`) ↔ GridCraft's formulas.
//!
//! Reading tokenizes the file's text into the formula parser's own tokens, so the same grammar
//! builds the expression: bracketed references (`[.A1:.B2]`, `[$'Sheet 1'.A1]`) arrive already
//! read, `;` separates arguments and an inline array's columns, `|` its rows, `~` is the union and
//! `!` the intersection, and ODF function names become Excel's. Writing prints the expression
//! with an OpenFormula [`Dialect`] of the shared printer.

use std::borrow::Cow;
use std::fmt::Write as _;

use gridcraft_core::{CellError, CellRef, RangeRef, col_to_letters, letters_to_col};
use gridcraft_formula::lexer::{Tok, Token};
use gridcraft_formula::{Anchor, BinOp, Dialect, Expr, RefKind, Reference, SheetSel, StructItem, StructRef, UnOp};

use super::names::{excel_to_odf, odf_to_excel};

/// Longest formula text translated; longer text is kept unevaluated.
const MAX_LEN: usize = 64 * 1024;

/// The syntax a formula attribute's namespace prefix names.
enum Syntax<'a> {
    /// OpenFormula, or the legacy OpenOffice syntax it grew from (`of:`, `oooc:`, or none).
    Open(&'a str),
    /// Excel's own (`msoxl:`), as Excel writes it into ODS: what the formula parser reads.
    Excel(&'a str),
}

/// The formula text after its namespace prefix (`of:=…`, and `of:#N/A` without `=`). Formulas in
/// another syntax (Writer's `ooow:` table formulas, an unknown prefix) are refused.
fn syntax(src: &str) -> Option<Syntax<'_>> {
    let src = src.trim_start();
    let (prefix, body) = match src.split_once(':') {
        Some((p, rest)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') => (Some(p), rest),
        _ => (None, src),
    };
    let body = body.strip_prefix('=').unwrap_or(body);
    match prefix {
        None | Some("of" | "oooc") => Some(Syntax::Open(body)),
        Some("msoxl") => Some(Syntax::Excel(body)),
        Some(_) => None,
    }
}

/// Reads an ODF formula attribute (`of:=SUM([.A1:.B2])`). `None` when the text is not a formula
/// GridCraft reads.
pub(crate) fn parse_odf(src: &str) -> Option<Expr> {
    if src.len() > MAX_LEN {
        return None;
    }
    let e = match syntax(src)? {
        Syntax::Excel(body) => gridcraft_formula::parse(body).ok()?,
        Syntax::Open(body) => gridcraft_formula::parse_tokens(tokens(body)?).ok()?,
    };
    Some(in_parens(e.map(&mut |e| enclose_unions(default_arguments(e)))))
}

/// [`parse_odf`] as A1 formula text without the `=`.
pub(crate) fn from_odf(src: &str) -> Option<String> {
    parse_odf(src).map(|e| gridcraft_formula::print(&e))
}

/// OpenFormula's `CHISQDIST` and `FDIST` default their `Cumulative` argument to TRUE; the Excel
/// functions they become require it.
fn default_arguments(e: Expr) -> Expr {
    match e {
        Expr::Call(name, mut args) if (name == "CHISQ.DIST" && args.len() == 2) || (name == "F.DIST" && args.len() == 3) => {
            // As OpenFormula writes the constant, so it reads back the same.
            args.push(Expr::Call("TRUE".into(), Vec::new()));
            Expr::Call(name, args)
        }
        other => other,
    }
}

/// A union in parentheses: A1 text writes a union only there, while OpenFormula's `~` stands
/// anywhere (`SUM([.A1]~[.B1])`).
fn in_parens(e: Expr) -> Expr {
    match e {
        Expr::Binary(BinOp::Union, ..) => Expr::Paren(Box::new(e)),
        other => other,
    }
}

/// `e` with each union directly under it (not in a union or parentheses) put in parentheses.
fn enclose_unions(e: Expr) -> Expr {
    let all = |v: Vec<Expr>| v.into_iter().map(in_parens).collect::<Vec<_>>();
    match e {
        Expr::Binary(op, a, b) if op != BinOp::Union => Expr::Binary(op, Box::new(in_parens(*a)), Box::new(in_parens(*b))),
        Expr::Unary(op, x) => Expr::Unary(op, Box::new(in_parens(*x))),
        Expr::Call(name, args) => Expr::Call(name, all(args)),
        Expr::Invoke(f, args) => Expr::Invoke(Box::new(in_parens(*f)), all(args)),
        Expr::Array(rows) => Expr::Array(rows.into_iter().map(all).collect()),
        other => other,
    }
}

/// OpenFormula text as the formula parser's tokens; `None` for text it can't be.
fn tokens(body: &str) -> Option<Vec<Token>> {
    let chars: Vec<(usize, char)> = body.char_indices().collect();
    let at = |i: usize| chars.get(i).map(|c| c.1);
    let pos = |i: usize| chars.get(i).map_or(body.len(), |c| c.0);
    let mut out = Vec::new();
    let mut braces = 0usize;
    let mut i = 0usize;
    while let Some(c) = at(i) {
        let start = i;
        let tok = match c {
            ' ' | '\t' | '\n' | '\r' => {
                i += 1;
                continue;
            }
            '"' => {
                let mut text = String::new();
                i += 1;
                loop {
                    let ch = at(i)?;
                    i += 1;
                    if ch == '"' {
                        if at(i) != Some('"') {
                            break;
                        }
                        i += 1;
                    }
                    text.push(ch);
                }
                Tok::Text(text)
            }
            '[' => {
                let (inner_end, next) = closing_bracket(&chars, i + 1)?;
                let inner = body.get(pos(i + 1)..pos(inner_end))?;
                i = next;
                match parse_odf_ref(inner) {
                    Some(r) => Tok::Ref(r),
                    None => Tok::Error(CellError::Ref),
                }
            }
            // A sheet-local name: `'Sheet 2'.Total`.
            '\'' => {
                let (sheet, next) = quoted(&chars, i)?;
                if at(next) != Some('.') {
                    return None;
                }
                let (word, after) = word(&chars, next + 1)?;
                out.push(Token { tok: Tok::Sheet(sheet, None), start: pos(start), end: pos(next) });
                out.push(Token { tok: Tok::Word(word), start: pos(next + 1), end: pos(after) });
                i = after;
                continue;
            }
            '{' => {
                braces += 1;
                i += 1;
                Tok::LBrace
            }
            '}' => {
                braces = braces.saturating_sub(1);
                i += 1;
                Tok::RBrace
            }
            // Arguments and an inline array's columns.
            ';' => {
                i += 1;
                Tok::Comma
            }
            '|' if braces > 0 => {
                i += 1;
                Tok::Semicolon
            }
            '~' => {
                i += 1;
                Tok::Op("~")
            }
            '!' => {
                i += 1;
                Tok::Space
            }
            ':' => {
                i += 1;
                Tok::Colon
            }
            '(' => {
                i += 1;
                Tok::LParen
            }
            ')' => {
                i += 1;
                Tok::RParen
            }
            '#' => {
                // An error literal (`#N/A`, `#DIV/0!`, `#NAME?`).
                let mut j = i + 1;
                while at(j).is_some_and(|ch| ch.is_alphanumeric() || ch == '/' || ch == '_') {
                    j += 1;
                }
                if matches!(at(j), Some('!' | '?')) {
                    j += 1;
                }
                let text = body.get(pos(i)..pos(j))?;
                i = j;
                Tok::Error(CellError::parse(text)?)
            }
            '<' | '>' | '=' | '+' | '-' | '*' | '/' | '^' | '&' | '%' => {
                let two = match (c, at(i + 1)) {
                    ('<', Some('=')) => Some("<="),
                    ('<', Some('>')) => Some("<>"),
                    ('>', Some('=')) => Some(">="),
                    _ => None,
                };
                let op = match two {
                    Some(op) => {
                        i += 2;
                        op
                    }
                    None => {
                        i += 1;
                        match c {
                            '<' => "<",
                            '>' => ">",
                            '=' => "=",
                            '+' => "+",
                            '-' => "-",
                            '*' => "*",
                            '/' => "/",
                            '^' => "^",
                            '&' => "&",
                            _ => "%",
                        }
                    }
                };
                Tok::Op(op)
            }
            c if c.is_ascii_digit() || (c == '.' && at(i + 1).is_some_and(|d| d.is_ascii_digit())) => {
                // Numbers, with exponents (`1E+5`).
                let mut j = i;
                while let Some(ch) = at(j) {
                    let exp_sign = matches!(ch, '+' | '-') && j > i && matches!(at(j - 1), Some('e' | 'E'));
                    if ch.is_ascii_digit() || matches!(ch, '.' | 'e' | 'E') || exp_sign {
                        j += 1;
                    } else {
                        break;
                    }
                }
                let n = body.get(pos(i)..pos(j))?.parse::<f64>().ok().filter(|n| n.is_finite())?;
                i = j;
                Tok::Number(n)
            }
            c if c.is_alphabetic() || c == '_' || c == '\\' => {
                let (w, j) = word(&chars, i)?;
                // A function name is followed (possibly after spaces) by `(`, which it takes.
                let mut k = j;
                while at(k) == Some(' ') {
                    k += 1;
                }
                if at(k) == Some('(') {
                    i = k + 1;
                    Tok::Func(odf_to_excel(&w))
                } else {
                    i = j;
                    match w.to_ascii_uppercase().as_str() {
                        "TRUE" => Tok::Bool(true),
                        "FALSE" => Tok::Bool(false),
                        _ => Tok::Word(w),
                    }
                }
            }
            _ => return None,
        };
        out.push(Token { tok, start: pos(start), end: pos(i) });
    }
    Some(out)
}

/// The index of the `]` closing a reference opened before `from` (quoted sheet names may hold
/// one), and the index after it.
fn closing_bracket(chars: &[(usize, char)], from: usize) -> Option<(usize, usize)> {
    let mut quoted = false;
    let mut j = from;
    loop {
        let ch = chars.get(j)?.1;
        if ch == '\'' {
            // '' inside quotes is an escaped quote.
            if quoted && chars.get(j + 1).map(|c| c.1) == Some('\'') {
                j += 2;
                continue;
            }
            quoted = !quoted;
        } else if ch == ']' && !quoted {
            return Some((j, j + 1));
        }
        j += 1;
    }
}

/// A `'…'` sheet name starting at `from` (with `''` for a quote), and the index after it.
fn quoted(chars: &[(usize, char)], from: usize) -> Option<(String, usize)> {
    let mut out = String::new();
    let mut j = from + 1;
    loop {
        let ch = chars.get(j)?.1;
        j += 1;
        if ch == '\'' {
            if chars.get(j).map(|c| c.1) != Some('\'') {
                return Some((out, j));
            }
            j += 1;
        }
        out.push(ch);
    }
}

/// A name or function name starting at `from`, and the index after it.
fn word(chars: &[(usize, char)], from: usize) -> Option<(String, usize)> {
    let mut j = from;
    while chars.get(j).is_some_and(|c| c.1.is_alphanumeric() || matches!(c.1, '_' | '.' | '\\')) {
        j += 1;
    }
    let w: String = chars.get(from..j)?.iter().map(|c| c.1).collect();
    (!w.is_empty()).then_some((w, j))
}

/// One end of an ODF reference: optional sheet, then column and row parts.
struct OdfEnd {
    sheet: Option<String>,
    col: Option<(u32, bool)>,
    row: Option<(u32, bool)>,
}

impl OdfEnd {
    fn anchor(&self) -> Option<Anchor> {
        let ((col, col_abs), (row, row_abs)) = (self.col?, self.row?);
        Some(Anchor { row, col, row_abs, col_abs })
    }
}

/// Splits `s` at the last `.` outside quotes: sheet part and cell part.
fn split_sheet(s: &str) -> (Option<&str>, &str) {
    let mut quoted = false;
    let mut at = None;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'\'' => quoted = !quoted,
            b'.' if !quoted => at = Some(i),
            _ => {}
        }
    }
    match at {
        Some(i) => (s.get(..i), s.get(i + 1..).unwrap_or_default()),
        None => (None, s),
    }
}

/// `None` for a deleted sheet or cell, in any language (`#REF!`, `#RIF!`, `#BEZUG!`), and for
/// text that isn't one end of a reference.
fn parse_end(s: &str) -> Option<OdfEnd> {
    let (sheet, cell) = split_sheet(s.trim());
    let sheet = match sheet {
        // `[.A1]`: the formula's own sheet.
        Some("") | None => None,
        Some(sh) => Some(gridcraft_core::unquote_sheet(sh.strip_prefix('$').unwrap_or(sh))),
    };
    if cell.contains('#') || sheet.as_deref().is_some_and(|s| s.starts_with('#')) {
        return None;
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
    Some(OdfEnd { sheet, col, row })
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

/// An ODF reference or range address without its brackets (`.A1`, `$'My sheet'.A1:.B2`,
/// `Sheet1.A:Sheet1.A`). `None` for a reference that doesn't read: a deleted cell, another
/// document's (`'file:///x.ods'#$Sheet1.A1`), or one whose second end names a sheet when its
/// first doesn't (the formula's own sheet isn't known here).
pub(crate) fn parse_odf_ref(inner: &str) -> Option<Reference> {
    let inner = inner.trim();
    if inner.starts_with('\'') && inner.contains("'#") {
        return None;
    }
    let Some((a, b)) = split_range(inner) else {
        let end = parse_end(inner)?;
        let sheet = end.sheet.clone().map_or(SheetSel::Current, SheetSel::Named);
        return Some(Reference { sheet, kind: RefKind::Cell(end.anchor()?) });
    };
    let (first, second) = (parse_end(a)?, parse_end(b)?);
    let sheet = match (first.sheet.clone(), second.sheet.clone()) {
        (Some(x), Some(y)) if x != y => SheetSel::Span(x, y),
        (Some(x), _) => SheetSel::Named(x),
        (None, Some(_)) => return None,
        (None, None) => SheetSel::Current,
    };
    let kind = match (first.anchor(), second.anchor()) {
        // A whole column (rows 1 to the last row) is a column reference.
        (Some(x), Some(y)) if x.row == 0 && y.row == gridcraft_core::MAX_ROWS - 1 && x.row_abs == y.row_abs => {
            RefKind::Cols(x.col, x.col_abs, y.col, y.col_abs)
        }
        (Some(x), Some(y)) => RefKind::Range(x, y),
        _ => match (first.col, first.row, second.col, second.row) {
            (Some((c0, a0)), None, Some((c1, a1)), None) => RefKind::Cols(c0, a0, c1, a1),
            (None, Some((r0, a0)), None, Some((r1, a1))) => RefKind::Rows(r0, a0, r1, a1),
            // Both ends must be the same kind: cells, whole columns or whole rows.
            _ => return None,
        },
    };
    Some(Reference { sheet, kind })
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

/// The first range of an address list, on a named sheet: (sheet, range).
pub(crate) fn range_on_sheet(list: &str) -> Option<(String, RangeRef)> {
    let r = parse_odf_ref(split_addresses(list).first()?.trim_matches(|c| c == '[' || c == ']'))?;
    match &r.sheet {
        SheetSel::Named(n) => Some((n.clone(), r.range())),
        _ => None,
    }
}

// ------------------------------------------------------------------------------------- writing

/// A sheet name as ODF writes it: quoted unless it is a plain word.
pub(crate) fn quote_sheet(name: &str) -> String {
    let plain = name.chars().all(|c| c.is_alphanumeric() || c == '_') && !name.chars().next().is_some_and(|c| c.is_ascii_digit());
    if plain { name.to_string() } else { format!("'{}'", name.replace('\'', "''")) }
}

/// A range on `sheet` as an ODF range address (`Sheet1.A1:Sheet1.D10`), clamped to the grid.
pub(crate) fn range_address(sheet: &str, r: RangeRef) -> String {
    let q = quote_sheet(sheet);
    format!(
        "{q}.{}{}:{q}.{}{}",
        col_to_letters(r.start.col),
        r.start.row + 1,
        col_to_letters(r.end.col.min(gridcraft_core::MAX_COLS - 1)),
        r.end.row.min(gridcraft_core::MAX_ROWS - 1) + 1
    )
}

/// Looks up a table's ranges for structured references, which ODF formulas have no syntax for.
pub(crate) trait Tables {
    /// The sheet and range a structured reference covers at `at`, or `None` (written as `#REF!`).
    fn resolve(&self, s: &StructRef, at: CellRef) -> Option<(String, RangeRef)>;
}

/// Prints `e`, in a cell at `at` on `sheet` (`None` for a workbook name), as OpenFormula with
/// the `of:=` prefix.
pub(crate) fn to_odf(e: &Expr, at: CellRef, sheet: Option<&str>, tables: &dyn Tables) -> String {
    format!("of:={}", gridcraft_formula::print_with(e, &OpenFormula { at, sheet, tables }))
}

/// The OpenFormula spelling of the shared printer.
struct OpenFormula<'a> {
    at: CellRef,
    sheet: Option<&'a str>,
    tables: &'a dyn Tables,
}

impl Dialect for OpenFormula<'_> {
    fn reference(&self, s: &mut String, r: &Reference, _sep: &str) {
        write_ref(s, r);
    }

    fn name(&self, s: &mut String, n: &str) {
        match n.rsplit_once('!') {
            // A name local to another sheet: `'Sheet 2'.Total` (ODF 1.3 part 4 §5.11).
            Some((sheet, name)) if self.sheet != Some(sheet) => {
                let _ = write!(s, "'{}'.{name}", sheet.replace('\'', "''"));
            }
            Some((_, name)) => s.push_str(name),
            None => s.push_str(n),
        }
    }

    fn struct_ref(&self, s: &mut String, st: &StructRef) {
        match self.tables.resolve(st, self.at) {
            Some((sheet, range)) => {
                let a = Anchor { row: range.start.row, col: range.start.col, row_abs: true, col_abs: true };
                let b = Anchor { row: range.end.row, col: range.end.col, row_abs: true, col_abs: true };
                let kind =
                    if range.start == range.end && !st.specifiers.contains(&StructItem::All) { RefKind::Cell(a) } else { RefKind::Range(a, b) };
                write_ref(s, &Reference { sheet: SheetSel::Named(sheet), kind });
            }
            None => s.push_str("#REF!"),
        }
    }

    fn function<'n>(&self, name: &'n str) -> Cow<'n, str> {
        Cow::Owned(excel_to_odf(name))
    }

    fn boolean(&self, b: bool) -> &'static str {
        if b { "TRUE()" } else { "FALSE()" }
    }

    fn separators(&self) -> (char, char, char) {
        (';', ';', '|')
    }

    fn operator(&self, op: BinOp) -> &'static str {
        match op {
            BinOp::Union => "~",
            BinOp::Intersect => "!",
            other => other.symbol(),
        }
    }

    /// LibreOffice reads Excel's dynamic-array operators as these functions.
    fn unary_call(&self, op: UnOp) -> Option<&'static str> {
        match op {
            UnOp::At => Some("COM.MICROSOFT.SINGLE"),
            UnOp::Spill => Some("COM.MICROSOFT.ANCHORARRAY"),
            UnOp::Trim(_) => Some("COM.MICROSOFT.TRIMRANGE"),
            _ => None,
        }
    }
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
    if let Some(name) = sheet {
        s.push('$');
        s.push_str(&quote_sheet(name));
    }
    s.push('.');
}

/// A reference in a formula: its [`address`] in brackets.
pub(crate) fn write_ref(s: &mut String, r: &Reference) {
    s.push('[');
    s.push_str(&address(r));
    s.push(']');
}

/// A reference as an ODF address (`$Sheet1.A1:.B2`, `.$A:.$C`).
pub(crate) fn address(r: &Reference) -> String {
    let mut s = String::new();
    let (first, last) = match &r.sheet {
        SheetSel::Current => (None, None),
        SheetSel::Named(n) => (Some(n.as_str()), None),
        SheetSel::Span(a, b) => (Some(a.as_str()), Some(b.as_str())),
    };
    push_sheet(&mut s, first);
    match &r.kind {
        RefKind::Cell(a) => {
            push_col(&mut s, a.col, a.col_abs);
            push_row(&mut s, a.row, a.row_abs);
            if let Some(last) = last {
                s.push(':');
                push_sheet(&mut s, Some(last));
                push_col(&mut s, a.col, a.col_abs);
                push_row(&mut s, a.row, a.row_abs);
            }
        }
        RefKind::Range(a, b) => {
            push_col(&mut s, a.col, a.col_abs);
            push_row(&mut s, a.row, a.row_abs);
            s.push(':');
            push_sheet(&mut s, last);
            push_col(&mut s, b.col, b.col_abs);
            push_row(&mut s, b.row, b.row_abs);
        }
        RefKind::Cols(c0, a0, c1, a1) => {
            push_col(&mut s, *c0, *a0);
            s.push(':');
            push_sheet(&mut s, last);
            push_col(&mut s, *c1, *a1);
        }
        RefKind::Rows(r0, a0, r1, a1) => {
            push_row(&mut s, *r0, *a0);
            s.push(':');
            push_sheet(&mut s, last);
            push_row(&mut s, *r1, *a1);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoTables;
    impl Tables for NoTables {
        fn resolve(&self, _: &StructRef, _: CellRef) -> Option<(String, RangeRef)> {
            None
        }
    }

    fn rt(odf: &str, a1: &str) {
        let got = from_odf(odf);
        assert_eq!(got.as_deref(), Some(a1), "reading {odf}");
        let e = gridcraft_formula::parse(a1).unwrap_or(Expr::Missing);
        assert_eq!(to_odf(&e, CellRef::new(0, 0), Some("Sheet1"), &NoTables), odf, "writing {a1}");
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
        rt("of:=SUM(([.A1:.B2]~[.C1]))", "SUM((A1:B2,C1))");
        // The union reads as a union wherever it is, kept in the parentheses A1 needs.
        assert_eq!(from_odf("of:=SUM([.A1:.B2]~[.C1])").as_deref(), Some("SUM((A1:B2,C1))"));
        assert_eq!(from_odf("of:=AREAS([.A1]~[.B1]~[.C1])").as_deref(), Some("AREAS((A1,B1,C1))"));
        assert_eq!(from_odf("of:=SUM([.A1]~[.B1];1)").as_deref(), Some("SUM((A1,B1),1)"));
        assert_eq!(from_odf("of:=[.A1]~[.B1]").as_deref(), Some("(A1,B1)"));
        assert_eq!(from_odf("of:=[.A1:.B2]![.B1:.C3]").as_deref(), Some("A1:B2 B1:C3"));
        assert_eq!(from_odf("of:=[.#REF!]+1").as_deref(), Some("#REF!+1"));
        assert_eq!(from_odf("of:=['file:///x.ods'#$Sheet1.A1]").as_deref(), Some("#REF!"));
        assert_eq!(from_odf("of:=ISNA(#N/A)").as_deref(), Some("ISNA(#N/A)"));
        assert_eq!(from_odf("msoxl:=SUM(A1:B2)").as_deref(), Some("SUM(A1:B2)"));
        // Excel's syntax is read as Excel's, sheet names and structured references included.
        assert_eq!(from_odf("msoxl:=Sheet1!A1+1").as_deref(), Some("Sheet1!A1+1"));
        assert_eq!(from_odf("msoxl:=SUM(Table1[Col])").as_deref(), Some("SUM(Table1[Col])"));
        // Writer's table formulas are another syntax.
        assert_eq!(from_odf("ooow:=<A1>+1"), None);
        // A range whose second end alone names a sheet can't be placed.
        assert_eq!(from_odf("of:=SUM([.A1:$Sheet2.B2])").as_deref(), Some("SUM(#REF!)"));
        // Cumulative defaults to TRUE in OpenFormula; Excel requires it.
        assert_eq!(from_odf("of:=CHISQDIST(1;2)").as_deref(), Some("CHISQ.DIST(1,2,TRUE())"));
        assert_eq!(from_odf("foo:=1"), None);
        assert_eq!(from_odf("of:#N/A").as_deref(), Some("#N/A"));
        assert_eq!(split_addresses("'My sheet'.A1:'My sheet'.B2  Data.C3"), vec!["'My sheet'.A1:'My sheet'.B2", "Data.C3"]);
    }

    /// What has no OpenFormula operator is written as the function LibreOffice reads for it, and
    /// reads back as the operator; a name local to another sheet keeps its sheet.
    #[test]
    fn dynamic_array_operators_and_sheet_names_round_trip() {
        rt("of:=SUM(COM.MICROSOFT.ANCHORARRAY([.A1]))", "SUM(A1#)");
        rt("of:=COM.MICROSOFT.SINGLE([.A1:.A10])", "@A1:A10");
        rt("of:='Sheet 2'.Total*2", "'Sheet 2'!Total*2");
        let e = gridcraft_formula::parse("Sheet1!Total").unwrap_or(Expr::Missing);
        assert_eq!(to_odf(&e, CellRef::new(0, 0), Some("Sheet1"), &NoTables), "of:=Total");
    }

    #[test]
    fn hostile_text_does_not_panic() {
        for s in ["of:=[", "of:=\"", "of:=['", "of:=[.ZZZZ1]", "of:=[.A0]", "of:=[.A99999999999]", "of:=#", "of:=[:]", "of:=[.A1:]", "of:={|;}"] {
            let _ = from_odf(s);
        }
    }
}
