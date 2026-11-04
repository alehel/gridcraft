//! ODF cell styles (`style:style style:family="table-cell"`, ODF 1.3 part 3 §17.18 and §20) ↔
//! GridCraft styles, plus lengths and colours.

use std::collections::HashMap;
use std::fmt::Write as _;

use gridcraft_model::style::Protection;
use gridcraft_model::{BorderLine, BorderStyle, Color, Fill, Font, HAlign, PatternType, Style, Underline, VAlign, VertAlign};

use super::dom::{Elem, Ns};
use crate::xml::esc_attr;

/// A length (`0.889in`, `2.258cm`, `12pt`, `5mm`, `96px`) in points.
pub(crate) fn length_pt(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic() || c == '%')?;
    let (num, unit) = (s.get(..split)?, s.get(split..)?);
    let n: f64 = num.trim().parse().ok().filter(|n: &f64| n.is_finite())?;
    let pt = match unit {
        "pt" => n,
        "in" | "inch" => n * 72.0,
        "cm" => n * 72.0 / 2.54,
        "mm" => n * 72.0 / 25.4,
        "pc" => n * 12.0,
        "px" => n * 0.75,
        _ => return None,
    };
    pt.is_finite().then_some(pt)
}

/// Points → pixels at 96 dpi.
pub(crate) fn pt_to_px(pt: f64) -> f32 {
    (pt * 96.0 / 72.0) as f32
}

pub(crate) fn color(s: &str) -> Option<Color> {
    let h = s.trim().strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    u32::from_str_radix(h, 16).ok().map(Color::Rgb)
}

/// Properties a style states; `None` = inherited from the parent.
#[derive(Clone, Debug, Default)]
pub(crate) struct Props {
    pub font_name: Option<String>,
    pub font_size: Option<f32>,
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<Underline>,
    pub strike: Option<bool>,
    pub color: Option<Color>,
    pub vert: Option<VertAlign>,
    pub background: Option<Option<Color>>,
    /// left, right, top, bottom, diagonal top-left → bottom-right, bottom-left → top-right.
    pub borders: [Option<BorderLine>; 6],
    pub h: Option<HAlign>,
    pub v: Option<VAlign>,
    pub wrap: Option<bool>,
    pub shrink: Option<bool>,
    pub indent: Option<u8>,
    pub rotation: Option<i16>,
    pub protection: Option<Protection>,
    pub data_style: Option<String>,
}

/// A named or automatic table-cell style as read.
#[derive(Clone, Debug)]
pub(crate) struct CellStyleDef {
    pub parent: Option<String>,
    pub props: Props,
    /// `style:map` conditions (ODF's own conditional formatting).
    pub maps: Vec<(String, String, Option<String>)>,
}

/// Parses a border (`0.74pt solid #000000`, `none`). A double line's `style:border-line-width`
/// (inner, gap, outer) doesn't change its kind.
fn border(s: &str) -> Option<BorderLine> {
    let s = s.trim();
    if s.is_empty() || s == "none" || s == "hidden" {
        return Some(BorderLine::default());
    }
    let mut width = None;
    let mut kind = "solid";
    let mut col = Color::Auto;
    for part in s.split_whitespace() {
        if let Some(c) = color(part) {
            col = c;
        } else if let Some(w) = length_pt(part) {
            width = Some(w);
        } else {
            match part {
                "thin" => width = Some(0.75),
                "medium" => width = Some(1.75),
                "thick" => width = Some(2.5),
                k => kind = k,
            }
        }
    }
    if kind == "none" || kind == "hidden" {
        return Some(BorderLine::default());
    }
    let w = width.unwrap_or(0.75);
    let style = match kind {
        "double" | "double-thin" => BorderStyle::Double,
        // Excel has no medium dotted line: a thick dotted one is still dotted.
        "dotted" => BorderStyle::Dotted,
        "dashed" | "fine-dashed" => {
            if w > 1.0 {
                BorderStyle::MediumDashed
            } else {
                BorderStyle::Dashed
            }
        }
        "dash-dot" => {
            if w > 1.0 {
                BorderStyle::MediumDashDot
            } else {
                BorderStyle::DashDot
            }
        }
        "dash-dot-dot" => {
            if w > 1.0 {
                BorderStyle::MediumDashDotDot
            } else {
                BorderStyle::DashDotDot
            }
        }
        _ => {
            // LibreOffice's own lines: hairline 0.05pt, thin 0.74pt, medium 1.75pt, thick 2.5pt;
            // a hairline is Excel's Hair (written back as 0.06pt).
            if w < 0.3 {
                BorderStyle::Hair
            } else if w <= 1.5 {
                BorderStyle::Thin
            } else if w <= 2.25 {
                BorderStyle::Medium
            } else {
                BorderStyle::Thick
            }
        }
    };
    Some(BorderLine { style, color: col })
}

/// An ODF angle (`45`, `45deg`, `0.785rad`, `50grad`) in degrees.
fn angle_deg(a: &str) -> Option<f64> {
    let a = a.trim();
    let (n, per_deg) = if let Some(n) = a.strip_suffix("deg") {
        (n, 1.0)
    } else if let Some(n) = a.strip_suffix("grad") {
        (n, 400.0 / 360.0)
    } else if let Some(n) = a.strip_suffix("rad") {
        (n, std::f64::consts::PI / 180.0)
    } else {
        (a, 1.0)
    };
    n.trim().parse::<f64>().ok().map(|v| v / per_deg).filter(|d| d.is_finite())
}

fn font_weight_bold(s: &str) -> Option<bool> {
    match s.trim() {
        "bold" => Some(true),
        "normal" => Some(false),
        n => n.parse::<u32>().ok().map(|w| w >= 600),
    }
}

/// Reads the property elements of a style (or of a default style).
pub(crate) fn read_props(e: &Elem, fonts: &HashMap<String, String>) -> Props {
    let mut p = Props { data_style: e.attr(Ns::Style, "data-style-name").map(str::to_string), ..Props::default() };
    if let Some(tc) = e.child(Ns::Style, "table-cell-properties") {
        cell_props(tc, &mut p);
    }
    if let Some(pp) = e.child(Ns::Style, "paragraph-properties") {
        let cell_align = p.h.is_some();
        paragraph_props(pp, cell_align, &mut p);
    }
    if let Some(tp) = e.child(Ns::Style, "text-properties") {
        text_props(tp, fonts, &mut p);
    }
    p
}

/// `style:table-cell-properties`: fill, borders, wrapping, alignment, rotation, protection.
fn cell_props(tc: &Elem, p: &mut Props) {
    if let Some(bg) = tc.attr(Ns::Fo, "background-color") {
        p.background = Some(if bg == "transparent" { None } else { color(bg) });
    }
    if let Some(b) = tc.attr(Ns::Fo, "border") {
        let all = border(b);
        for side in p.borders.iter_mut().take(4) {
            *side = all;
        }
    }
    // The six lines, in `Props::borders` order.
    let sides = [
        (Ns::Fo, "border-left"),
        (Ns::Fo, "border-right"),
        (Ns::Fo, "border-top"),
        (Ns::Fo, "border-bottom"),
        (Ns::Style, "diagonal-tl-br"),
        (Ns::Style, "diagonal-bl-tr"),
    ];
    for ((ns, name), slot) in sides.into_iter().zip(p.borders.iter_mut()) {
        if let Some(b) = tc.attr(ns, name) {
            *slot = border(b);
        }
    }
    if let Some(w) = tc.attr(Ns::Fo, "wrap-option") {
        p.wrap = Some(w == "wrap");
    }
    if let Some(s) = tc.attr(Ns::Style, "shrink-to-fit") {
        p.shrink = Some(s == "true");
    }
    match tc.attr(Ns::Style, "vertical-align") {
        Some("top") => p.v = Some(VAlign::Top),
        Some("middle") => p.v = Some(VAlign::Center),
        Some("bottom") => p.v = Some(VAlign::Bottom),
        Some("automatic") => p.v = Some(VAlign::Bottom),
        _ => {}
    }
    if let Some(j) = tc.attr(Ns::CalcExt, "vertical-justify").or_else(|| tc.attr(Ns::LoExt, "vertical-justify")) {
        if j == "distribute" {
            p.v = Some(VAlign::Distributed);
        } else if j == "justify" {
            p.v = Some(VAlign::Justify);
        }
    }
    if let Some(a) = tc.attr(Ns::Style, "rotation-angle") {
        let deg = angle_deg(a).map(|d| (d.round() as i64).rem_euclid(360) as i16);
        if let Some(d) = deg {
            p.rotation = Some(match d {
                0..=90 => d,
                270..=359 => d - 360,
                91..=180 => 90,
                _ => -90,
            });
        }
    }
    if tc.attr(Ns::Style, "direction") == Some("ttb") {
        p.rotation = Some(255);
    }
    if tc.attr(Ns::Style, "repeat-content") == Some("true") {
        p.h = Some(HAlign::Fill);
    }
    if tc.attr(Ns::Style, "text-align-source") == Some("value-type") {
        p.h = Some(HAlign::General);
    }
    if let Some(cp) = tc.attr(Ns::Style, "cell-protect") {
        let words: Vec<&str> = cp.split_whitespace().collect();
        let locked = words.iter().any(|w| matches!(*w, "protected" | "hidden-and-protected"));
        let hidden = words.iter().any(|w| matches!(*w, "formula-hidden" | "hidden-and-protected"));
        p.protection = Some(Protection { locked, hidden });
    }
}

/// `style:paragraph-properties`: alignment, unless the cell's properties already decided it
/// (`cell_align`: filled, or aligned by value type), and indent.
fn paragraph_props(pp: &Elem, cell_align: bool, p: &mut Props) {
    if !cell_align {
        match pp.attr(Ns::Fo, "text-align") {
            Some("start" | "left") => p.h = Some(HAlign::Left),
            Some("center") => p.h = Some(HAlign::Center),
            Some("end" | "right") => p.h = Some(HAlign::Right),
            Some("justify") => {
                p.h = Some(if pp.attr(Ns::Fo, "text-align-last") == Some("justify") { HAlign::Distributed } else { HAlign::Justify });
            }
            _ => {}
        }
    }
    if let Some(m) = pp.attr(Ns::Fo, "margin-left").and_then(length_pt) {
        // LibreOffice writes an Excel indent level as 10 points of margin.
        p.indent = Some((m / 10.0).round().clamp(0.0, 250.0) as u8);
    }
}

/// `style:text-properties`: the font.
fn text_props(tp: &Elem, fonts: &HashMap<String, String>, p: &mut Props) {
    if let Some(n) = tp.attr(Ns::Style, "font-name") {
        p.font_name = Some(fonts.get(n).cloned().unwrap_or_else(|| n.to_string()));
    } else if let Some(f) = tp.attr(Ns::Fo, "font-family") {
        p.font_name = Some(f.trim_matches(|c| c == '\'' || c == '"').to_string());
    }
    if let Some(s) = tp.attr(Ns::Fo, "font-size").and_then(length_pt) {
        p.font_size = Some((s as f32).clamp(1.0, 409.0));
    }
    if let Some(w) = tp.attr(Ns::Fo, "font-weight").and_then(font_weight_bold) {
        p.bold = Some(w);
    }
    if let Some(s) = tp.attr(Ns::Fo, "font-style") {
        p.italic = Some(s == "italic" || s == "oblique");
    }
    if let Some(u) = tp.attr(Ns::Style, "text-underline-style") {
        p.underline = Some(if u == "none" {
            Underline::None
        } else if tp.attr(Ns::Style, "text-underline-type") == Some("double") {
            Underline::Double
        } else {
            Underline::Single
        });
    }
    if let Some(s) = tp.attr(Ns::Style, "text-line-through-style") {
        p.strike = Some(s != "none");
    }
    if let Some(c) = tp.attr(Ns::Fo, "color").and_then(color) {
        p.color = Some(c);
    }
    if tp.attr(Ns::Style, "use-window-font-color") == Some("true") {
        p.color = Some(Color::Auto);
    }
    if let Some(pos) = tp.attr(Ns::Style, "text-position") {
        let first = pos.split_whitespace().next().unwrap_or("0");
        p.vert = Some(match first {
            "super" => VertAlign::Superscript,
            "sub" => VertAlign::Subscript,
            n => match n.trim_end_matches('%').parse::<f64>() {
                Ok(v) if v > 0.0 => VertAlign::Superscript,
                Ok(v) if v < 0.0 => VertAlign::Subscript,
                _ => VertAlign::Baseline,
            },
        });
    }
}

/// `over` on top of `base`.
pub(crate) fn merge(base: &Props, over: &Props) -> Props {
    let mut out = base.clone();
    macro_rules! take {
        ($($f:ident),*) => { $( if over.$f.is_some() { out.$f = over.$f.clone(); } )* };
    }
    take!(
        font_name, font_size, bold, italic, underline, strike, color, vert, background, h, v, wrap, shrink, indent, rotation, protection, data_style
    );
    for (o, b) in out.borders.iter_mut().zip(over.borders.iter()) {
        if b.is_some() {
            *o = *b;
        }
    }
    out
}

/// The style the properties describe, on top of `base`.
pub(crate) fn to_style(p: &Props, base: &Style, num_fmt: Option<String>) -> Style {
    let mut s = base.clone();
    // Each stated property replaces the base's.
    macro_rules! apply {
        ($($prop:ident => $($field:ident).+),* $(,)?) => { $( if let Some(v) = &p.$prop { s.$($field).+ = v.clone(); } )* };
    }
    apply!(
        font_size => font.size, bold => font.bold, italic => font.italic, underline => font.underline, strike => font.strike,
        color => font.color, vert => font.vert, h => align.h, v => align.v, wrap => align.wrap, shrink => align.shrink,
        indent => align.indent, rotation => align.rotation, protection => protection,
    );
    if let Some(n) = &p.font_name {
        s.font.name.clone_from(n);
        s.font.minor = false;
    }
    if let Some(bg) = p.background {
        s.fill = bg.map_or_else(Fill::default, Fill::solid);
    }
    let slots: [&mut BorderLine; 6] =
        [&mut s.borders.left, &mut s.borders.right, &mut s.borders.top, &mut s.borders.bottom, &mut s.borders.diag_down, &mut s.borders.diag_up];
    for (slot, b) in slots.into_iter().zip(p.borders.iter()) {
        if let Some(b) = b {
            *slot = *b;
        }
    }
    if let Some(code) = num_fmt {
        s.num_fmt = gridcraft_model::NumFmt::new(&code);
    }
    s
}

// ------------------------------------------------------------------------------------- writing

/// A colour as ODF writes it (`#rrggbb`), `None` for automatic.
pub(crate) fn hex(c: Color, theme: &gridcraft_model::Theme) -> Option<String> {
    c.resolve(theme).map(|[r, g, b]| format!("#{r:02x}{g:02x}{b:02x}"))
}

fn border_text(b: &BorderLine, theme: &gridcraft_model::Theme) -> String {
    if b.is_none() {
        return "none".into();
    }
    let (w, kind) = match b.style {
        BorderStyle::None => return "none".into(),
        BorderStyle::Hair => ("0.06pt", "solid"),
        BorderStyle::Thin => ("0.74pt", "solid"),
        BorderStyle::Medium => ("1.75pt", "solid"),
        BorderStyle::Thick => ("2.5pt", "solid"),
        BorderStyle::Dashed => ("0.74pt", "dashed"),
        BorderStyle::Dotted => ("0.74pt", "dotted"),
        BorderStyle::Double => ("1.1pt", "double"),
        BorderStyle::MediumDashed => ("1.75pt", "dashed"),
        BorderStyle::DashDot => ("0.74pt", "dash-dot"),
        BorderStyle::MediumDashDot | BorderStyle::SlantDashDot => ("1.75pt", "dash-dot"),
        BorderStyle::DashDotDot => ("0.74pt", "dash-dot-dot"),
        BorderStyle::MediumDashDotDot => ("1.75pt", "dash-dot-dot"),
    };
    let c = hex(b.color, theme).unwrap_or_else(|| "#000000".into());
    format!("{w} {kind} {c}")
}

/// The property elements of `s` (everything that differs from `base`), for a `style:style`.
pub(crate) fn write_props(s: &Style, base: &Style, theme: &gridcraft_model::Theme) -> String {
    let mut tc = String::new();
    if s.fill != base.fill {
        match (s.fill.pattern, s.fill.fg, s.fill.bg) {
            (PatternType::None, ..) => tc.push_str(" fo:background-color=\"transparent\""),
            (PatternType::Solid, fg, _) => {
                let _ = write!(tc, " fo:background-color=\"{}\"", hex(fg, theme).unwrap_or_else(|| "transparent".into()));
            }
            // A pattern has no ODF equivalent: its colour blended half-and-half reads closest.
            (_, fg, bg) => {
                let a = fg.resolve(theme).unwrap_or([0, 0, 0]);
                let b = bg.resolve(theme).unwrap_or([255, 255, 255]);
                let mix = |i: usize| ((a.get(i).copied().unwrap_or(0) as u16 + b.get(i).copied().unwrap_or(0) as u16) / 2) as u8;
                let _ = write!(tc, " fo:background-color=\"#{:02x}{:02x}{:02x}\"", mix(0), mix(1), mix(2));
            }
        }
    }
    let sides = [("left", s.borders.left), ("right", s.borders.right), ("top", s.borders.top), ("bottom", s.borders.bottom)];
    if s.borders != base.borders {
        for (side, b) in sides {
            let _ = write!(tc, " fo:border-{side}=\"{}\"", border_text(&b, theme));
            if b.style == BorderStyle::Double {
                let _ = write!(tc, " style:border-line-width-{side}=\"0.35pt 0.4pt 0.35pt\"");
            }
        }
        // A diagonal is written whenever it differs from the base's, `none` when it was cleared.
        if s.borders.diag_down != base.borders.diag_down {
            let _ = write!(tc, " style:diagonal-tl-br=\"{}\"", border_text(&s.borders.diag_down, theme));
        }
        if s.borders.diag_up != base.borders.diag_up {
            let _ = write!(tc, " style:diagonal-bl-tr=\"{}\"", border_text(&s.borders.diag_up, theme));
        }
    }
    let a = &s.align;
    let b = &base.align;
    if a.wrap != b.wrap {
        tc.push_str(if a.wrap { " fo:wrap-option=\"wrap\"" } else { " fo:wrap-option=\"no-wrap\"" });
    }
    if a.shrink != b.shrink {
        let _ = write!(tc, " style:shrink-to-fit=\"{}\"", a.shrink);
    }
    if a.v != b.v {
        tc.push_str(match a.v {
            VAlign::Top => " style:vertical-align=\"top\"",
            VAlign::Center => " style:vertical-align=\"middle\"",
            VAlign::Bottom => " style:vertical-align=\"bottom\"",
            VAlign::Justify => " style:vertical-align=\"automatic\" loext:vertical-justify=\"justify\"",
            VAlign::Distributed => " style:vertical-align=\"automatic\" loext:vertical-justify=\"distribute\"",
        });
    }
    if a.rotation != b.rotation {
        if a.rotation == 255 {
            tc.push_str(" style:direction=\"ttb\"");
        } else {
            // Stacked text set back to an angle states its direction, or a parent's `ttb` shows.
            if b.rotation == 255 {
                tc.push_str(" style:direction=\"ltr\"");
            }
            let deg = (a.rotation as i32).rem_euclid(360);
            let _ = write!(tc, " style:rotation-angle=\"{deg}\"");
        }
    }
    if a.h != b.h {
        if a.h == HAlign::General {
            tc.push_str(" style:text-align-source=\"value-type\"");
        } else {
            tc.push_str(" style:text-align-source=\"fix\"");
        }
        tc.push_str(if a.h == HAlign::Fill { " style:repeat-content=\"true\"" } else { " style:repeat-content=\"false\"" });
    }
    if s.protection != base.protection {
        let v = match (s.protection.locked, s.protection.hidden) {
            (true, true) => "hidden-and-protected",
            (true, false) => "protected",
            (false, true) => "formula-hidden",
            (false, false) => "none",
        };
        let _ = write!(tc, " style:cell-protect=\"{v}\"");
    }
    let mut out = String::new();
    if !tc.is_empty() {
        let _ = write!(out, "<style:table-cell-properties{tc}/>");
    }
    let mut pp = String::new();
    if a.h != b.h {
        let align = match a.h {
            HAlign::Left => Some("start"),
            HAlign::Center | HAlign::CenterAcross => Some("center"),
            HAlign::Right => Some("end"),
            HAlign::Justify => Some("justify"),
            HAlign::Distributed => Some("justify\" fo:text-align-last=\"justify"),
            HAlign::General | HAlign::Fill => None,
        };
        if let Some(al) = align {
            let _ = write!(pp, " fo:text-align=\"{al}\"");
        }
    }
    if a.indent != b.indent {
        let _ = write!(pp, " fo:margin-left=\"{}pt\"", a.indent as u32 * 10);
    }
    if !pp.is_empty() {
        let _ = write!(out, "<style:paragraph-properties{pp}/>");
    }
    let f = &s.font;
    let g = &base.font;
    let mut tp = String::new();
    write_font(&mut tp, f, g, theme);
    if !tp.is_empty() {
        let _ = write!(out, "<style:text-properties{tp}/>");
    }
    out
}

/// Text property attributes of `f` that differ from `g`.
pub(crate) fn write_font(tp: &mut String, f: &Font, g: &Font, theme: &gridcraft_model::Theme) {
    if f.name != g.name {
        let n = esc_attr(&f.name);
        let _ = write!(tp, " style:font-name=\"{n}\" fo:font-family=\"{n}\"");
    }
    if f.size != g.size {
        let _ = write!(tp, " fo:font-size=\"{}pt\" style:font-size-asian=\"{0}pt\" style:font-size-complex=\"{0}pt\"", f.size);
    }
    if f.bold != g.bold {
        let w = if f.bold { "bold" } else { "normal" };
        let _ = write!(tp, " fo:font-weight=\"{w}\" style:font-weight-asian=\"{w}\" style:font-weight-complex=\"{w}\"");
    }
    if f.italic != g.italic {
        let w = if f.italic { "italic" } else { "normal" };
        let _ = write!(tp, " fo:font-style=\"{w}\" style:font-style-asian=\"{w}\" style:font-style-complex=\"{w}\"");
    }
    if f.underline != g.underline {
        match f.underline {
            Underline::None => tp.push_str(" style:text-underline-style=\"none\""),
            Underline::Single | Underline::SingleAccounting => {
                tp.push_str(" style:text-underline-style=\"solid\" style:text-underline-width=\"auto\" style:text-underline-color=\"font-color\"")
            }
            Underline::Double | Underline::DoubleAccounting => tp.push_str(
                " style:text-underline-style=\"solid\" style:text-underline-type=\"double\" style:text-underline-width=\"auto\" style:text-underline-color=\"font-color\"",
            ),
        }
    }
    if f.strike != g.strike {
        tp.push_str(if f.strike {
            " style:text-line-through-style=\"solid\" style:text-line-through-type=\"single\""
        } else {
            " style:text-line-through-style=\"none\""
        });
    }
    if f.color != g.color {
        match hex(f.color, theme) {
            Some(c) => {
                let _ = write!(tp, " fo:color=\"{c}\"");
            }
            None => tp.push_str(" style:use-window-font-color=\"true\""),
        }
    }
    if f.vert != g.vert {
        tp.push_str(match f.vert {
            VertAlign::Baseline => " style:text-position=\"0% 100%\"",
            VertAlign::Superscript => " style:text-position=\"super 58%\"",
            VertAlign::Subscript => " style:text-position=\"sub 58%\"",
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths() {
        assert_eq!(length_pt("1in"), Some(72.0));
        assert_eq!(length_pt("2.54cm").map(|v| v.round()), Some(72.0));
        assert_eq!(length_pt("12pt"), Some(12.0));
        assert_eq!(length_pt("bad"), None);
        assert_eq!(length_pt("NaNpt"), None);
        assert_eq!(pt_to_px(72.0), 96.0);
    }

    #[test]
    fn borders_read() {
        assert_eq!(border("0.74pt solid #ff0000"), Some(BorderLine { style: BorderStyle::Thin, color: Color::Rgb(0xff0000) }));
        assert_eq!(border("none"), Some(BorderLine::default()));
        assert_eq!(border("2.49pt solid #000000").map(|b| b.style), Some(BorderStyle::Thick));
        assert_eq!(border("1.1pt double #000000").map(|b| b.style), Some(BorderStyle::Double));
        assert_eq!(border("2.5pt dotted #000000").map(|b| b.style), Some(BorderStyle::Dotted));
    }

    /// An angle just under a full turn is upright, and every ODF angle unit reads.
    #[test]
    fn rotation_angles_read_in_any_unit() {
        let rotation = |a: &str| {
            let xml =
                format!(r#"<style:table-cell-properties xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" style:rotation-angle="{a}"/>"#);
            let Ok(tree) = super::super::dom::parse_tree(xml.as_bytes()) else { return None };
            let mut p = Props::default();
            cell_props(&tree, &mut p);
            p.rotation
        };
        assert_eq!(rotation("359.6"), Some(0));
        assert_eq!(rotation("45deg"), Some(45));
        assert_eq!(rotation("50grad"), Some(45));
        assert_eq!(rotation("1.5707963rad"), Some(90));
        assert_eq!(rotation("315"), Some(-45));
    }

    /// A diagonal the base has and the style clears is written as `none`, so the base's doesn't
    /// show through.
    #[test]
    fn cleared_diagonals_are_written() {
        let theme = gridcraft_model::Theme::default();
        let mut base = Style::default();
        base.borders.diag_down = BorderLine { style: BorderStyle::Thin, color: Color::Auto };
        let props = write_props(&Style::default(), &base, &theme);
        assert!(props.contains("style:diagonal-tl-br=\"none\""), "{props}");
        assert!(!props.contains("diagonal-bl-tr"), "{props}");
    }
}
