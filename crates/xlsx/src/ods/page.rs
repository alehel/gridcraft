//! Page setup: `style:page-layout` (paper, orientation, margins, scaling, what prints) and
//! `style:master-page` headers and footers (ODF 1.3 part 3 §16.5, §16.9) ↔ the sheet's print
//! settings, with header/footer fields as Excel's `&P`, `&N`, `&A`, `&D`, `&T`, `&F` codes.

use std::collections::HashMap;
use std::fmt::Write as _;

use gridcraft_model::{Orientation, PageOrder, PrintSettings, Sheet};

use super::dom::{Elem, Node, Ns};
use super::style::length_pt;
use crate::xml::{esc, esc_attr};

/// Paper sizes by name, in millimetres (portrait).
const PAPERS: &[(&str, f64, f64)] = &[
    ("Letter", 215.9, 279.4),
    ("Legal", 215.9, 355.6),
    ("Tabloid", 279.4, 431.8),
    ("Executive", 184.15, 266.7),
    ("Statement", 139.7, 215.9),
    ("A3", 297.0, 420.0),
    ("A4", 210.0, 297.0),
    ("A5", 148.0, 210.0),
    ("A6", 105.0, 148.0),
    ("B4", 257.0, 364.0),
    ("B5", 182.0, 257.0),
    ("B6", 128.0, 182.0),
    ("Envelope #10", 104.8, 241.3),
    ("Envelope DL", 110.0, 220.0),
];

fn paper_for(w_mm: f64, h_mm: f64) -> Option<&'static str> {
    let (a, b) = if w_mm <= h_mm { (w_mm, h_mm) } else { (h_mm, w_mm) };
    PAPERS.iter().find(|(_, pw, ph)| (pw - a).abs() < 2.0 && (ph - b).abs() < 2.0).map(|(n, _, _)| *n)
}

fn inches(e: &Elem, ns: Ns, name: &str) -> Option<f32> {
    e.attr(ns, name).and_then(length_pt).map(|pt| (pt / 72.0) as f32)
}

/// Header/footer region text as an Excel code section (`&P`, `&A`…), escaping `&`.
fn region_code(region: &Elem) -> String {
    let mut out = String::new();
    let mut first = true;
    for p in region.children(Ns::Text, "p") {
        if !first {
            out.push('\n');
        }
        first = false;
        let mut stack: Vec<&Node> = p.kids.iter().rev().collect();
        // ODF whitespace: runs collapse to one space, none at the paragraph's start.
        let mut started = false;
        let mut pending = false;
        while let Some(n) = stack.pop() {
            if let Node::Text(t) = n {
                for ch in t.chars() {
                    if ch.is_whitespace() {
                        pending = started;
                    } else {
                        if pending {
                            out.push(' ');
                            pending = false;
                        }
                        if ch == '&' {
                            out.push('&');
                        }
                        out.push(ch);
                        started = true;
                    }
                }
                continue;
            }
            if pending {
                out.push(' ');
                pending = false;
            }
            started = true;
            match n {
                Node::Text(_) => {}
                Node::El(e) if e.ns == Ns::Text => match e.name.as_str() {
                    "page-number" => out.push_str("&P"),
                    "page-count" => out.push_str("&N"),
                    "sheet-name" => out.push_str("&A"),
                    "date" => out.push_str("&D"),
                    "time" => out.push_str("&T"),
                    "file-name" => out.push_str(if e.attr(Ns::Text, "display") == Some("full") { "&Z&F" } else { "&F" }),
                    "title" => out.push_str("&F"),
                    "s" => out.push(' '),
                    "tab" => out.push('\t'),
                    "line-break" => out.push('\n'),
                    _ => stack.extend(e.kids.iter().rev()),
                },
                Node::El(e) => stack.extend(e.kids.iter().rev()),
            }
        }
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

/// Applies master page `name` (and its page layout) to the sheet's print settings.
pub(crate) fn apply(sheet: &mut Sheet, name: &str, masters: &HashMap<String, Elem>, layouts: &HashMap<String, Elem>) {
    let Some(master) = masters.get(name) else { return };
    let p = &mut sheet.print;
    let layout = master.attr(Ns::Style, "page-layout-name").and_then(|l| layouts.get(l));
    let header_h = layout.and_then(|l| l.child(Ns::Style, "header-style")).and_then(|h| h.child(Ns::Style, "header-footer-properties"));
    let footer_h = layout.and_then(|l| l.child(Ns::Style, "footer-style")).and_then(|h| h.child(Ns::Style, "header-footer-properties"));
    if let Some(props) = layout.and_then(|l| l.child(Ns::Style, "page-layout-properties")) {
        let w = props.attr(Ns::Fo, "page-width").and_then(length_pt).map(|pt| pt * 25.4 / 72.0);
        let h = props.attr(Ns::Fo, "page-height").and_then(length_pt).map(|pt| pt * 25.4 / 72.0);
        if let (Some(w), Some(h)) = (w, h)
            && let Some(paper) = paper_for(w, h)
        {
            p.paper = paper.into();
        }
        p.orientation = match props.attr(Ns::Style, "print-orientation") {
            Some("landscape") => Orientation::Landscape,
            Some("portrait") => Orientation::Portrait,
            _ if w.zip(h).is_some_and(|(w, h)| w > h) => Orientation::Landscape,
            _ => Orientation::Portrait,
        };
        let m = |n: &str| inches(props, Ns::Fo, n).or_else(|| inches(props, Ns::Fo, "margin"));
        let extra = |hf: Option<&Elem>, side: &str| -> Option<f32> {
            let hf = hf?;
            let h = inches(hf, Ns::Fo, "min-height").or_else(|| inches(hf, Ns::Svg, "height")).unwrap_or(0.0);
            Some(h + inches(hf, Ns::Fo, side).unwrap_or(0.0))
        };
        if let Some(v) = m("margin-left") {
            p.margins[0] = v;
        }
        if let Some(v) = m("margin-right") {
            p.margins[1] = v;
        }
        let header_on = master.child(Ns::Style, "header").is_some_and(|h| h.attr(Ns::Style, "display") != Some("false"));
        let footer_on = master.child(Ns::Style, "footer").is_some_and(|h| h.attr(Ns::Style, "display") != Some("false"));
        if let Some(v) = m("margin-top") {
            match extra(header_h, "margin-bottom").filter(|_| header_on) {
                Some(e) => {
                    p.margins[4] = v;
                    p.margins[2] = v + e;
                }
                None => p.margins[2] = v,
            }
        }
        if let Some(v) = m("margin-bottom") {
            match extra(footer_h, "margin-top").filter(|_| footer_on) {
                Some(e) => {
                    p.margins[5] = v;
                    p.margins[3] = v + e;
                }
                None => p.margins[3] = v,
            }
        }
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
                p.push_str(&esc(text));
            }
            text.clear();
        }
    };
    while let Some(c) = chars.next() {
        if c != '&' {
            if c == '\n' {
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
            'F' | 'f' => "<text:title>???</text:title>",
            'Z' | 'z' => "",
            '"' => {
                // A font name: skip to the closing quote.
                for x in chars.by_ref() {
                    if x == '"' {
                        break;
                    }
                }
                continue;
            }
            _ => {
                // Font size digits and the like carry no text.
                while chars.peek().is_some_and(char::is_ascii_digit) {
                    chars.next();
                }
                continue;
            }
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
    let (w, h) = PAPERS.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(&p.paper)).map(|(_, w, h)| (*w, *h)).unwrap_or((215.9, 279.4));
    let (w, h) = if p.orientation == Orientation::Landscape { (h, w) } else { (w, h) };
    let mut print = vec!["charts", "drawings", "objects", "zero-values"];
    if p.gridlines {
        print.push("grid");
    }
    if p.headings {
        print.push("headers");
    }
    let has_header = !p.header.is_empty() || !p.even_header.is_empty() || !p.first_header.is_empty();
    let has_footer = !p.footer.is_empty() || !p.even_footer.is_empty() || !p.first_footer.is_empty();
    let [ml, mr, mt, mb, mh, mf] = p.margins;
    // ODF's page margin reaches the header; the header's height and spacing reach the body.
    let (top, header_extra) = if has_header { (mh.max(0.0), (mt - mh).max(0.0)) } else { (mt, 0.0) };
    let (bottom, footer_extra) = if has_footer { (mf.max(0.0), (mb - mf).max(0.0)) } else { (mb, 0.0) };
    let mut layout = format!(
        "<style:page-layout style:name=\"pm{i}\"><style:page-layout-properties fo:page-width=\"{w}mm\" fo:page-height=\"{h}mm\" style:print-orientation=\"{}\" fo:margin-top=\"{top}in\" fo:margin-bottom=\"{bottom}in\" fo:margin-left=\"{ml}in\" fo:margin-right=\"{mr}in\" style:print-page-order=\"{}\" style:print=\"{}\"",
        if p.orientation == Orientation::Landscape { "landscape" } else { "portrait" },
        if p.page_order == PageOrder::OverThenDown { "ltr" } else { "ttb" },
        print.join(" ")
    );
    match p.fit_to {
        Some((x, y)) => {
            let _ = write!(layout, " style:scale-to-X=\"{x}\" style:scale-to-Y=\"{y}\"");
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
    let hf = |extra: f32, side: &str| {
        format!("<style:header-footer-properties fo:min-height=\"{}in\" fo:margin-{side}=\"0in\"/>", (extra.max(0.0)).max(0.01))
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
    let _ = esc_attr("");
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
}
