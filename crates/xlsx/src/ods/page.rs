//! Page setup: `style:page-layout` (paper, orientation, margins, scaling, what prints) and
//! `style:master-page` headers and footers (ODF 1.3 part 3 §16.5, §16.9) ↔ the sheet's print
//! settings, with header/footer fields as Excel's `&P`, `&N`, `&A`, `&D`, `&T`, `&F` codes.

use std::collections::HashMap;
use std::fmt::Write as _;

use gridcraft_model::{Orientation, PageOrder, PrintSettings, Sheet};

use super::dom::{Elem, Ns, ParagraphText, walk_paragraph};
use super::style::length_pt;

fn inches(e: &Elem, ns: Ns, name: &str) -> Option<f32> {
    e.attr(ns, name).and_then(length_pt).map(|pt| (pt / 72.0) as f32)
}

/// The most text one header or footer region keeps (Excel holds 255 characters per header).
const MAX_REGION: usize = 8 * 1024;

/// Header/footer text as Excel's codes: fields as `&P`, `&A`…, a text `&` doubled.
struct Codes;
impl ParagraphText for Codes {
    fn push(&mut self, out: &mut String, ch: char) {
        if ch == '&' {
            out.push('&');
        }
        out.push(ch);
    }
    fn field(&mut self, e: &Elem) -> Option<&'static str> {
        Some(match e.name.as_str() {
            "page-number" => "&P",
            "page-count" => "&N",
            "sheet-name" => "&A",
            "date" => "&D",
            "time" => "&T",
            "file-name" => match e.attr(Ns::Text, "display") {
                Some("full") => "&Z&F",
                Some("path") => "&Z",
                _ => "&F",
            },
            "title" => "&F",
            _ => return None,
        })
    }
}

/// Header/footer region text as an Excel code section, paragraphs on their own lines.
fn region_code(region: &Elem) -> String {
    let mut out = String::new();
    for (i, p) in region.children(Ns::Text, "p").enumerate() {
        if i > 0 {
            out.push('\n');
        }
        walk_paragraph(p, &mut out, MAX_REGION, &mut Codes);
    }
    out
}

fn hf_code(e: &Elem) -> String {
    if e.attr(Ns::Style, "display") == Some("false") {
        return String::new();
    }
    let mut out = String::new();
    let mut any = false;
    for (tag, code) in [("region-left", "&L"), ("region-center", "&C"), ("region-right", "&R")] {
        if let Some(r) = e.child(Ns::Style, tag) {
            let t = region_code(r);
            if !t.is_empty() {
                out.push_str(code);
                out.push_str(&t);
            }
            any = true;
        }
    }
    if !any {
        let t = region_code(e);
        if !t.is_empty() {
            out.push_str("&C");
            out.push_str(&t);
        }
    }
    out
}

/// The spacing between a header or footer and the body LibreOffice gives a new page style
/// (0.25 cm), written inside the header's height.
const HF_SPACING_IN: f32 = 0.0984;

/// The page margin LibreOffice gives a page style that states none (2 cm, observed).
const ODF_MARGIN_IN: f32 = 0.7874;

/// Applies master page `name` (and its page layout) to the sheet's print settings.
pub(crate) fn apply(sheet: &mut Sheet, name: &str, masters: &HashMap<String, Elem>, layouts: &HashMap<String, Elem>) {
    // Without a page style giving a size the sheet prints on the application's own paper.
    sheet.print.paper.clear();
    // Margins a page style leaves out are the application's own, not Excel's defaults.
    sheet.print.margins = [ODF_MARGIN_IN; 6];
    let Some(master) = masters.get(name) else { return };
    let p = &mut sheet.print;
    let layout = master.attr(Ns::Style, "page-layout-name").and_then(|l| layouts.get(l));
    let header_h = layout.and_then(|l| l.child(Ns::Style, "header-style")).and_then(|h| h.child(Ns::Style, "header-footer-properties"));
    let footer_h = layout.and_then(|l| l.child(Ns::Style, "footer-style")).and_then(|h| h.child(Ns::Style, "header-footer-properties"));
    if let Some(props) = layout.and_then(|l| l.child(Ns::Style, "page-layout-properties")) {
        let w = props.attr(Ns::Fo, "page-width").and_then(length_pt);
        let h = props.attr(Ns::Fo, "page-height").and_then(length_pt);
        // A layout without a page size prints on the application's own paper, upright whatever
        // its orientation says (LibreOffice, observed): the paper is left unstated.
        p.orientation = match props.attr(Ns::Style, "print-orientation") {
            _ if w.is_none() || h.is_none() => Orientation::Portrait,
            Some("landscape") => Orientation::Landscape,
            Some("portrait") => Orientation::Portrait,
            _ if w.zip(h).is_some_and(|(w, h)| w > h) => Orientation::Landscape,
            _ => Orientation::Portrait,
        };
        // A landscape page is its paper turned: the paper is the page's size the other way.
        if let (Some(w), Some(h)) = (w, h) {
            let (pw, ph) = if p.orientation == Orientation::Landscape { (h, w) } else { (w, h) };
            p.paper = gridcraft_model::paper::for_size(pw as f32, ph as f32).unwrap_or("Letter").into();
        }
        let m = |n: &str| inches(props, Ns::Fo, n).or_else(|| inches(props, Ns::Fo, "margin")).unwrap_or(ODF_MARGIN_IN);
        // The header's height holds its spacing to the body (LibreOffice draws the body that
        // far below the page margin, observed), so the spacing is not added to it again.
        let extra = |hf: Option<&Elem>, side: &str| -> Option<f32> {
            let hf = hf?;
            let h = inches(hf, Ns::Fo, "min-height").or_else(|| inches(hf, Ns::Svg, "height")).unwrap_or(0.0);
            Some(h.max(inches(hf, Ns::Fo, side).unwrap_or(0.0)))
        };
        p.margins[0] = m("margin-left");
        p.margins[1] = m("margin-right");
        let header_on = master.child(Ns::Style, "header").is_some_and(|h| h.attr(Ns::Style, "display") != Some("false"));
        let footer_on = master.child(Ns::Style, "footer").is_some_and(|h| h.attr(Ns::Style, "display") != Some("false"));
        let (top, bottom) = (m("margin-top"), m("margin-bottom"));
        (p.margins[4], p.margins[5]) = (top, bottom);
        p.margins[2] = top + extra(header_h, "margin-bottom").filter(|_| header_on).unwrap_or(0.0);
        p.margins[3] = bottom + extra(footer_h, "margin-top").filter(|_| footer_on).unwrap_or(0.0);
        if let Some(s) = props.attr(Ns::Style, "scale-to").and_then(|s| s.trim_end_matches('%').parse::<f64>().ok()) {
            p.scale = s.clamp(10.0, 400.0) as u16;
        }
        let fx = props.attr(Ns::Style, "scale-to-X").and_then(|s| s.parse::<u16>().ok());
        let fy = props.attr(Ns::Style, "scale-to-Y").and_then(|s| s.parse::<u16>().ok());
        if fx.is_some() || fy.is_some() {
            p.fit_to = Some((fx.unwrap_or(0), fy.unwrap_or(0)));
        } else if let Some(n) = props.attr(Ns::Style, "scale-to-pages").and_then(|s| s.parse::<u16>().ok()) {
            p.fit_to = Some((n, n));
        }
        if let Some(what) = props.attr(Ns::Style, "print") {
            p.gridlines = what.split_whitespace().any(|w| w == "grid");
            p.headings = what.split_whitespace().any(|w| w == "headers");
        }
        p.page_order = if props.attr(Ns::Style, "print-page-order") == Some("ltr") { PageOrder::OverThenDown } else { PageOrder::DownThenOver };
        match props.attr(Ns::Style, "table-centering") {
            Some("horizontal") => p.center_h = true,
            Some("vertical") => p.center_v = true,
            Some("both") => {
                p.center_h = true;
                p.center_v = true;
            }
            _ => {}
        }
    }
    for (tag, footer) in [("header", false), ("footer", true)] {
        if let Some(e) = master.child(Ns::Style, tag) {
            *(if footer { &mut p.footer } else { &mut p.header }) = hf_code(e);
        }
        if let Some(e) = master.child(Ns::Style, &format!("{tag}-left"))
            && e.attr(Ns::Style, "display") != Some("false")
        {
            p.different_odd_even = true;
            *(if footer { &mut p.even_footer } else { &mut p.even_header }) = hf_code(e);
        }
        if let Some(e) = master.child(Ns::Style, &format!("{tag}-first")).or_else(|| master.child(Ns::LoExt, &format!("{tag}-first")))
            && e.attr(Ns::Style, "display") != Some("false")
        {
            p.different_first = true;
            *(if footer { &mut p.first_footer } else { &mut p.first_header }) = hf_code(e);
        }
    }
}

// ------------------------------------------------------------------------------------- writing

/// Excel header/footer code → `style:region-*` elements.
fn regions(code: &str) -> String {
    let mut parts: [String; 3] = Default::default();
    let mut cur = 1usize;
    let mut chars = code.chars().peekable();
    let mut text = String::new();
    let flush = |text: &mut String, parts: &mut [String; 3], cur: usize| {
        if !text.is_empty() {
            if let Some(p) = parts.get_mut(cur) {
                super::write::push_text(p, text);
            }
            text.clear();
        }
    };
    while let Some(c) = chars.next() {
        if c != '&' {
            // A line ends at a line feed, a carriage return, or both together.
            if c == '\r' && chars.peek() == Some(&'\n') {
                continue;
            }
            if c == '\n' || c == '\r' {
                flush(&mut text, &mut parts, cur);
                if let Some(p) = parts.get_mut(cur) {
                    p.push_str("</text:p><text:p>");
                }
            } else {
                text.push(c);
            }
            continue;
        }
        let Some(k) = chars.next() else { break };
        let field = match k {
            '&' => {
                text.push('&');
                continue;
            }
            'L' | 'l' => {
                flush(&mut text, &mut parts, cur);
                cur = 0;
                continue;
            }
            'C' | 'c' => {
                flush(&mut text, &mut parts, cur);
                cur = 1;
                continue;
            }
            'R' | 'r' => {
                flush(&mut text, &mut parts, cur);
                cur = 2;
                continue;
            }
            'P' | 'p' => "<text:page-number>1</text:page-number>",
            'N' | 'n' => "<text:page-count>1</text:page-count>",
            'A' | 'a' => "<text:sheet-name>???</text:sheet-name>",
            'D' | 'd' => "<text:date>01/01/2000</text:date>",
            'T' | 't' => "<text:time>00:00:00</text:time>",
            'F' | 'f' => "<text:file-name text:display=\"name-and-extension\">???</text:file-name>",
            // The path, or with `&F` right after it the full name.
            'Z' | 'z' if chars.peek() == Some(&'&') && chars.clone().nth(1).is_some_and(|f| f.eq_ignore_ascii_case(&'F')) => {
                chars.next();
                chars.next();
                "<text:file-name text:display=\"full\">???</text:file-name>"
            }
            'Z' | 'z' => "<text:file-name text:display=\"path\">???</text:file-name>",
            // A colour: six hex digits (`&KFF0000`) or a theme colour and tint (`&K01+000`).
            'K' | 'k' => {
                for _ in 0..6 {
                    if chars.next_if(|x| x.is_ascii_hexdigit() || matches!(x, '+' | '-')).is_none() {
                        break;
                    }
                }
                continue;
            }
            '"' => {
                // A font name: skip to the closing quote.
                for x in chars.by_ref() {
                    if x == '"' {
                        break;
                    }
                }
                continue;
            }
            // A font size (`&12`): its digits carry no text.
            d if d.is_ascii_digit() => {
                while chars.next_if(char::is_ascii_digit).is_some() {}
                continue;
            }
            // Bold, italic, underline and the other switches: what follows is text (`&B2024`).
            _ => continue,
        };
        flush(&mut text, &mut parts, cur);
        if let Some(p) = parts.get_mut(cur) {
            p.push_str(field);
        }
    }
    flush(&mut text, &mut parts, cur);
    let mut out = String::new();
    for (tag, body) in ["region-left", "region-center", "region-right"].iter().zip(parts.iter()) {
        let _ = write!(out, "<style:{tag}><text:p>{body}</text:p></style:{tag}>");
    }
    out
}

/// The page layout and master page elements for sheet `i`: (`style:page-layout`, `style:master-page`).
pub(crate) fn write(i: usize, p: &PrintSettings) -> (String, String) {
    // An unstated paper is left to the application, unless the page is turned: a layout
    // without a size prints upright, so a landscape page names its paper (Letter by default).
    let size = gridcraft_model::paper::size_pt(&p.paper)
        .or_else(|| (p.orientation == Orientation::Landscape).then_some((612.0, 792.0)))
        .map(|(w, h)| if p.orientation == Orientation::Landscape { (h, w) } else { (w, h) });
    let mut print = vec!["charts", "drawings", "objects", "zero-values"];
    if p.gridlines {
        print.push("grid");
    }
    if p.headings {
        print.push("headers");
    }
    let has_header = !p.header.is_empty() || !p.even_header.is_empty() || !p.first_header.is_empty();
    let has_footer = !p.footer.is_empty() || !p.even_footer.is_empty() || !p.first_footer.is_empty();
    // A margin that is no length (a hostile model's) is none.
    let [ml, mr, mt, mb, mh, mf] = p.margins.map(|m| if m.is_finite() { m.max(0.0) } else { 0.0 });
    // ODF's page margin reaches the header; the header's height and spacing reach the body.
    // Without a header there is nothing for its margin to place: only the page margin is kept.
    let (top, header_extra) = if has_header { (mh, (mt - mh).max(0.0)) } else { (mt, 0.0) };
    let (bottom, footer_extra) = if has_footer { (mf, (mb - mf).max(0.0)) } else { (mb, 0.0) };
    let mut layout = format!(
        "<style:page-layout style:name=\"pm{i}\"><style:page-layout-properties{} style:print-orientation=\"{}\" fo:margin-top=\"{top}in\" fo:margin-bottom=\"{bottom}in\" fo:margin-left=\"{ml}in\" fo:margin-right=\"{mr}in\" style:print-page-order=\"{}\" style:print=\"{}\"",
        size.map(|(w, h)| format!(" fo:page-width=\"{w}pt\" fo:page-height=\"{h}pt\"")).unwrap_or_default(),
        if p.orientation == Orientation::Landscape { "landscape" } else { "portrait" },
        if p.page_order == PageOrder::OverThenDown { "ltr" } else { "ttb" },
        print.join(" ")
    );
    match p.fit_to {
        // A page count of 0 (as many as needed) is written by leaving it out: ODF counts from 1.
        Some((x, y)) => {
            if x > 0 {
                let _ = write!(layout, " style:scale-to-X=\"{x}\"");
            }
            if y > 0 {
                let _ = write!(layout, " style:scale-to-Y=\"{y}\"");
            }
        }
        None => {
            let _ = write!(layout, " style:scale-to=\"{}%\"", p.scale.clamp(10, 400));
        }
    }
    let centering = match (p.center_h, p.center_v) {
        (true, true) => "both",
        (true, false) => "horizontal",
        (false, true) => "vertical",
        _ => "none",
    };
    let _ = write!(layout, " style:table-centering=\"{centering}\" style:writing-mode=\"lr-tb\"/>");
    // The spacing to the body lies inside the height: LibreOffice's usual 0.25 cm, or the
    // whole height when it is less, so the body starts where the model's margin puts it.
    let hf = |extra: f32, side: &str| {
        let spacing = extra.min(HF_SPACING_IN);
        format!("<style:header-footer-properties fo:min-height=\"{extra}in\" fo:margin-{side}=\"{spacing}in\"/>")
    };
    let _ = write!(
        layout,
        "<style:header-style>{}</style:header-style><style:footer-style>{}</style:footer-style></style:page-layout>",
        if has_header { hf(header_extra, "bottom") } else { String::new() },
        if has_footer { hf(footer_extra, "top") } else { String::new() }
    );
    let mut master = format!("<style:master-page style:name=\"mp{i}\" style:page-layout-name=\"pm{i}\">");
    let block = |tag: &str, code: &str, show: bool| {
        if show { format!("<style:{tag}>{}</style:{tag}>", regions(code)) } else { format!("<style:{tag} style:display=\"false\"/>") }
    };
    master.push_str(&block("header", &p.header, has_header));
    if p.different_odd_even {
        master.push_str(&block("header-left", &p.even_header, true));
    }
    if p.different_first {
        master.push_str(&block("header-first", &p.first_header, true));
    }
    master.push_str(&block("footer", &p.footer, has_footer));
    if p.different_odd_even {
        master.push_str(&block("footer-left", &p.even_footer, true));
    }
    if p.different_first {
        master.push_str(&block("footer-first", &p.first_footer, true));
    }
    master.push_str("</style:master-page>");
    (layout, master)
}

#[cfg(test)]
mod tests {
    use super::super::dom::parse_tree;
    use super::*;

    #[test]
    fn header_codes_round_trip() {
        let xml = format!(
            r#"<style:header xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">{}</style:header>"#,
            regions("&LLeft && co&CPage &P of &N&R&A")
        );
        let Ok(tree) = parse_tree(xml.as_bytes()) else { return };
        assert_eq!(hf_code(&tree), "&LLeft && co&CPage &P of &N&R&A");
    }

    /// Spaces, the file name and the full path survive.
    #[test]
    fn header_spaces_and_file_names_round_trip() {
        for code in ["&CPage  &P", "&L  lead&RF: &F", "&C&Z&F", "&C&Z"] {
            let xml = format!(
                r#"<style:header xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">{}</style:header>"#,
                regions(code)
            );
            let Ok(tree) = parse_tree(xml.as_bytes()) else { panic!("{xml}") };
            assert_eq!(hf_code(&tree), code);
        }
    }

    /// Colour codes carry no text: neither an RGB colour nor a theme colour leaks into the header.
    #[test]
    fn header_colour_codes_leave_no_text() {
        for code in ["&KFF0000Red", "&K00FF00Red", "&K01+000Red", "&K05-050Red"] {
            assert_eq!(regions(code), regions("Red"), "{code}");
        }
    }

    #[test]
    fn header_spaces_are_odf_whitespace_only() {
        // A no-break space is text, not a space to collapse; a text `&` is doubled.
        let page = |xml: &str| super::region_code(&crate::ods::dom::parse_tree(xml.as_bytes()).unwrap());
        let xml =
            "<r xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\"><text:p>\u{a0}1\u{a0}000 &amp;   <text:page-number/></text:p></r>";
        assert_eq!(page(xml), "\u{a0}1\u{a0}000 && &P");
    }

    #[test]
    fn a_carriage_return_is_a_line_break() {
        assert_eq!(regions("&Ca\rb"), regions("&Ca\nb"));
        assert_eq!(regions("&Ca\r\nb"), regions("&Ca\nb"));
    }

    #[test]
    fn text_after_a_switch_code_is_kept() {
        // Digits after `&` are a font size; after `&B` or `&U` they are text.
        assert_eq!(regions("&C&B2024 Budget"), regions("&C2024 Budget"));
        assert_eq!(regions("&U1st quarter"), regions("1st quarter"));
        assert_eq!(regions("&12Big"), regions("Big"));
    }
}
