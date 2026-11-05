//! A namespace-aware ODF element tree with ordered mixed content, and a streaming reader that
//! hands complete subtrees (rows, styles, validations…) to a sink while it walks the containers
//! around them, so a large `content.xml` never needs a whole tree.

use quick_xml::NsReader;
use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;

use crate::IoError;

/// Most elements a captured subtree may hold, and most bytes of text it may hold.
const MAX_NODES: usize = 2_000_000;
/// The most XML namespace declarations in scope at once (LibreOffice declares about 50).
const MAX_BINDINGS: usize = 1024;
const MAX_TEXT: usize = 64 * 1024 * 1024;
/// Longest attribute value accepted.
const MAX_ATTR: usize = 16 * 1024 * 1024;

/// The ODF namespaces GridCraft reads, by meaning rather than by the prefix a file chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ns {
    Office,
    Style,
    Text,
    Table,
    Draw,
    Fo,
    XLink,
    Dc,
    Meta,
    Number,
    Svg,
    Chart,
    Of,
    CalcExt,
    LoExt,
    Config,
    Manifest,
    TableOoo,
    Other,
    None,
}

impl Ns {
    fn from_uri(uri: &[u8]) -> Ns {
        match uri {
            b"urn:oasis:names:tc:opendocument:xmlns:office:1.0" => Ns::Office,
            b"urn:oasis:names:tc:opendocument:xmlns:style:1.0" => Ns::Style,
            b"urn:oasis:names:tc:opendocument:xmlns:text:1.0" => Ns::Text,
            b"urn:oasis:names:tc:opendocument:xmlns:table:1.0" => Ns::Table,
            b"urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" => Ns::Draw,
            b"urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" => Ns::Fo,
            b"http://www.w3.org/1999/xlink" => Ns::XLink,
            b"http://purl.org/dc/elements/1.1/" => Ns::Dc,
            b"urn:oasis:names:tc:opendocument:xmlns:meta:1.0" => Ns::Meta,
            b"urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0" => Ns::Number,
            b"urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" => Ns::Svg,
            b"urn:oasis:names:tc:opendocument:xmlns:chart:1.0" => Ns::Chart,
            b"urn:oasis:names:tc:opendocument:xmlns:of:1.2" => Ns::Of,
            b"urn:org:documentfoundation:names:experimental:calc:xmlns:calcext:1.0" => Ns::CalcExt,
            b"urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0" => Ns::LoExt,
            b"urn:oasis:names:tc:opendocument:xmlns:config:1.0" => Ns::Config,
            b"urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" => Ns::Manifest,
            b"http://openoffice.org/2009/table" => Ns::TableOoo,
            _ => Ns::Other,
        }
    }
}

/// An element: its namespace and local name, attributes in file order and children in order.
#[derive(Clone, Debug)]
pub(crate) struct Elem {
    pub ns: Ns,
    pub name: String,
    pub attrs: Vec<(Ns, String, String)>,
    pub kids: Vec<Node>,
}

#[derive(Clone, Debug)]
pub(crate) enum Node {
    El(Elem),
    Text(String),
}

impl Elem {
    pub fn is(&self, ns: Ns, name: &str) -> bool {
        self.ns == ns && self.name == name
    }
    pub fn attr(&self, ns: Ns, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, k, _)| *n == ns && k == name).map(|(_, _, v)| v.as_str())
    }
    pub fn elems(&self) -> impl Iterator<Item = &Elem> {
        self.kids.iter().filter_map(|n| match n {
            Node::El(e) => Some(e),
            Node::Text(_) => None,
        })
    }
    pub fn child(&self, ns: Ns, name: &str) -> Option<&Elem> {
        self.elems().find(|e| e.is(ns, name))
    }
    pub fn children<'a>(&'a self, ns: Ns, name: &'a str) -> impl Iterator<Item = &'a Elem> + 'a {
        self.elems().filter(move |e| e.is(ns, name))
    }
    /// Every descendant element named so, depth first (explicit stack: no recursion).
    pub fn descendants<'a>(&'a self, ns: Ns, name: &'a str) -> Vec<&'a Elem> {
        let mut out = Vec::new();
        let mut stack: Vec<&Elem> = self.elems().collect::<Vec<_>>().into_iter().rev().collect();
        while let Some(e) = stack.pop() {
            if e.is(ns, name) {
                out.push(e);
            }
            stack.extend(e.elems().collect::<Vec<_>>().into_iter().rev());
        }
        out
    }
    /// How many nodes (elements and text) this element holds, itself included.
    pub fn size(&self) -> usize {
        let mut n = 0;
        let mut stack = vec![self];
        while let Some(e) = stack.pop() {
            n += 1 + e.kids.iter().filter(|k| matches!(k, Node::Text(_))).count();
            stack.extend(e.elems());
        }
        n
    }
    /// The concatenated character data of this element and every descendant, as is.
    pub fn raw_text(&self) -> String {
        let mut out = String::new();
        let mut stack: Vec<&Node> = self.kids.iter().rev().collect();
        while let Some(n) = stack.pop() {
            match n {
                Node::Text(t) => out.push_str(t),
                Node::El(e) => stack.extend(e.kids.iter().rev()),
            }
        }
        out
    }
    /// The text of the `text:p`/`text:h` paragraphs directly below this element, joined by line
    /// breaks, with ODF whitespace rules (ODF 1.3 part 3 §6.1.2): runs of XML whitespace collapse
    /// to one space and leading whitespace of a paragraph is dropped, while `text:s`, `text:tab`
    /// and `text:line-break` expand to what they stand for.
    pub fn paragraphs_text(&self) -> String {
        let mut out = String::new();
        let mut first = true;
        for p in self.elems().filter(|e| e.ns == Ns::Text && (e.name == "p" || e.name == "h")) {
            if !first {
                out.push('\n');
            }
            first = false;
            paragraph_text(p, &mut out);
        }
        out
    }
}

/// A cell holds at most 1 MiB of text; the rest of a longer text is dropped.
pub(crate) const MAX_CELL_TEXT: usize = 1024 * 1024;

/// Appends a paragraph's text to `out` (see [`Elem::paragraphs_text`]), keeping `out` within
/// [`MAX_CELL_TEXT`].
pub(crate) fn paragraph_text(p: &Elem, out: &mut String) {
    walk_paragraph(p, out, MAX_CELL_TEXT, &mut Plain);
}

/// What [`walk_paragraph`] makes of a paragraph's text and of its fields.
pub(crate) trait ParagraphText {
    /// Appends one character of text.
    fn push(&mut self, out: &mut String, ch: char) {
        out.push(ch);
    }
    /// The text a field element stands for (`text:page-number`…), or `None` to read inside it.
    fn field(&mut self, _e: &Elem) -> Option<&'static str> {
        None
    }
}

/// The text as it reads, fields read for what they hold.
struct Plain;
impl ParagraphText for Plain {}

/// Appends a paragraph's text to `out` as ODF reads it: whitespace runs (XML whitespace only)
/// collapse to one space, none at the start; `text:s`, `text:tab` and `text:line-break` are
/// what they stand for; notes and bookmarks are left out. `out` stays within `cap` bytes.
pub(crate) fn walk_paragraph(p: &Elem, out: &mut String, cap: usize, text: &mut dyn ParagraphText) {
    let mut started = false;
    let mut pending = false;
    let mut stack: Vec<&Node> = p.kids.iter().rev().collect();
    while let Some(n) = stack.pop() {
        if out.len() >= cap {
            break;
        }
        match n {
            Node::Text(t) => {
                for ch in t.chars() {
                    if out.len() >= cap {
                        break;
                    }
                    if matches!(ch, ' ' | '\t' | '\n' | '\r') {
                        pending = started;
                    } else {
                        if pending {
                            out.push(' ');
                            pending = false;
                        }
                        text.push(out, ch);
                        started = true;
                    }
                }
            }
            Node::El(e) if e.ns == Ns::Text => {
                let special = match e.name.as_str() {
                    "s" => Some((
                        " ",
                        e.attr(Ns::Text, "c").and_then(|c| c.trim().parse::<usize>().ok()).unwrap_or(1).min(cap.saturating_sub(out.len())),
                    )),
                    "tab" => Some(("\t", 1)),
                    "line-break" => Some(("\n", 1)),
                    _ => text.field(e).map(|f| (f, 1)),
                };
                match special {
                    Some((s, count)) => {
                        if pending {
                            out.push(' ');
                            pending = false;
                        }
                        for _ in 0..count {
                            out.push_str(s);
                        }
                        started = true;
                    }
                    // Notes and annotations inside a paragraph are not part of its text.
                    None if matches!(e.name.as_str(), "note" | "bookmark" | "bookmark-start" | "bookmark-end" | "soft-page-break") => {}
                    None => stack.extend(e.kids.iter().rev()),
                }
            }
            // An office:annotation (a note) sits inside the cell, never inside the text.
            Node::El(e) if e.ns == Ns::Office && e.name == "annotation" => {}
            Node::El(e) => stack.extend(e.kids.iter().rev()),
        }
    }
    if out.len() > cap {
        out.truncate(out.floor_char_boundary(cap));
    }
}

/// What a streaming parse does with each element below the root.
pub(crate) trait Sink {
    /// Whether the element is read whole and handed to [`Sink::subtree`].
    fn capture(&mut self, ns: Ns, name: &str) -> bool;
    /// A streamed element starts (attributes only).
    fn open(&mut self, el: &Elem) -> Result<(), IoError>;
    /// The streamed element opened last ends.
    fn close(&mut self, el: &Elem) -> Result<(), IoError>;
    /// A captured element, complete.
    fn subtree(&mut self, el: Elem) -> Result<(), IoError>;
    /// Whether the sink keeps every subtree it is given (a whole tree is built): the element
    /// and text limits then hold for the document, not each subtree.
    fn keeps_subtrees(&self) -> bool {
        false
    }
}

pub(crate) fn xml_error(e: impl std::fmt::Display) -> IoError {
    IoError::Xml(format!("ODS: {e}"))
}

fn start_elem(reader: &NsReader<&[u8]>, e: &BytesStart<'_>) -> Result<Elem, IoError> {
    let (ns, local) = reader.resolver().resolve_element(e.name());
    let ns = match ns {
        ResolveResult::Bound(n) => Ns::from_uri(n.as_ref()),
        ResolveResult::Unbound => Ns::None,
        ResolveResult::Unknown(_) => Ns::Other,
    };
    let mut attrs = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(xml_error)?;
        let key = a.key.as_ref();
        if key == b"xmlns" || key.starts_with(b"xmlns:") {
            continue;
        }
        let (resolved, local) = reader.resolver().resolve_attribute(a.key);
        let ans = match resolved {
            ResolveResult::Bound(n) => Ns::from_uri(n.as_ref()),
            ResolveResult::Unbound => Ns::None,
            ResolveResult::Unknown(_) => Ns::Other,
        };
        let value = a.normalized_value(XmlVersion::Explicit1_0).map_err(xml_error)?;
        if value.len() > MAX_ATTR {
            return Err(IoError::TooLarge("ODS attribute exceeds 16 MiB".into()));
        }
        attrs.push((ans, String::from_utf8_lossy(local.as_ref()).into_owned(), value.into_owned()));
    }
    Ok(Elem { ns, name: String::from_utf8_lossy(local.as_ref()).into_owned(), attrs, kids: Vec::new() })
}

struct Capture {
    stack: Vec<Elem>,
    nodes: usize,
    text: usize,
}

impl Capture {
    fn push_text(&mut self, s: &str) -> Result<(), IoError> {
        self.text = self.text.saturating_add(s.len());
        if self.text > MAX_TEXT {
            return Err(IoError::TooLarge("ODS element holds more than 64 MiB of text".into()));
        }
        if let Some(top) = self.stack.last_mut() {
            match top.kids.last_mut() {
                Some(Node::Text(t)) => t.push_str(s),
                _ => top.kids.push(Node::Text(s.to_string())),
            }
        }
        Ok(())
    }
}

/// Parses `xml`, streaming the root and every element the sink doesn't capture.
pub(crate) fn parse(xml: &[u8], sink: &mut dyn Sink) -> Result<(), IoError> {
    let data = crate::xml::to_utf8(xml);
    let mut reader = NsReader::from_reader(data.as_ref());
    let mut streamed: Vec<Elem> = Vec::new();
    let mut capture: Option<Capture> = None;
    let mut root_done = false;
    // What earlier subtrees used of the limits, when they are kept.
    let keeps = sink.keeps_subtrees();
    let (mut kept_nodes, mut kept_text) = (0usize, 0usize);
    // The namespace declarations each open element made: a name is resolved against all of
    // those in scope, so their number is kept small.
    let mut bindings: Vec<usize> = Vec::new();
    let mut in_scope = 0usize;
    loop {
        let event = reader.read_event().map_err(xml_error)?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(event, Event::Empty(_));
                let declared = e.attributes().flatten().filter(|a| a.key.as_ref() == b"xmlns" || a.key.as_ref().starts_with(b"xmlns:")).count();
                if in_scope + declared > MAX_BINDINGS {
                    return Err(IoError::TooLarge("ODS declares too many XML namespaces".into()));
                }
                if !empty {
                    in_scope += declared;
                    bindings.push(declared);
                }
                let el = start_elem(&reader, e)?;
                if root_done {
                    return Err(xml_error("content after the root element"));
                }
                if streamed.len() + capture.as_ref().map_or(0, |c| c.stack.len()) >= crate::xml::MAX_DEPTH {
                    return Err(xml_error("XML nested too deeply"));
                }
                if let Some(cap) = capture.as_mut() {
                    cap.nodes += 1;
                    if cap.nodes > MAX_NODES {
                        return Err(IoError::TooLarge("ODS element holds too many elements".into()));
                    }
                    if empty {
                        if let Some(top) = cap.stack.last_mut() {
                            top.kids.push(Node::El(el));
                        }
                    } else {
                        cap.stack.push(el);
                    }
                    continue;
                }
                if !streamed.is_empty() && sink.capture(el.ns, &el.name) {
                    let (nodes, text) = if keeps { (kept_nodes + 1, kept_text) } else { (1, 0) };
                    if nodes > MAX_NODES {
                        return Err(IoError::TooLarge("ODS part holds too many elements".into()));
                    }
                    if empty {
                        kept_nodes = if keeps { nodes } else { 0 };
                        sink.subtree(el)?;
                    } else {
                        capture = Some(Capture { stack: vec![el], nodes, text });
                    }
                    continue;
                }
                sink.open(&el)?;
                if empty {
                    sink.close(&el)?;
                    if streamed.is_empty() {
                        root_done = true;
                    }
                } else {
                    streamed.push(el);
                }
            }
            Event::End(_) => {
                in_scope = in_scope.saturating_sub(bindings.pop().unwrap_or(0));
                if let Some(cap) = capture.as_mut() {
                    let Some(done) = cap.stack.pop() else { return Err(xml_error("unbalanced element")) };
                    match cap.stack.last_mut() {
                        Some(parent) => parent.kids.push(Node::El(done)),
                        None => {
                            if keeps {
                                (kept_nodes, kept_text) = (cap.nodes, cap.text);
                            }
                            capture = None;
                            sink.subtree(done)?;
                        }
                    }
                    continue;
                }
                let Some(el) = streamed.pop() else { return Err(xml_error("unexpected closing element")) };
                sink.close(&el)?;
                if streamed.is_empty() {
                    root_done = true;
                }
            }
            Event::Text(t) => {
                let s = t.xml_content(XmlVersion::Explicit1_0).map_err(xml_error)?;
                match capture.as_mut() {
                    Some(cap) => cap.push_text(&s)?,
                    None if streamed.is_empty() && !s.trim().is_empty() => return Err(xml_error("text outside the document")),
                    None => {}
                }
            }
            Event::CData(t) => {
                if let Some(cap) = capture.as_mut() {
                    cap.push_text(&t.decode().map_err(xml_error)?)?;
                }
            }
            Event::GeneralRef(r) => {
                if let Some(cap) = capture.as_mut() {
                    let ch = match r.resolve_char_ref().map_err(xml_error)? {
                        Some(c) => c,
                        None => match r.as_ref() {
                            b"amp" => '&',
                            b"lt" => '<',
                            b"gt" => '>',
                            b"apos" => '\'',
                            b"quot" => '"',
                            _ => return Err(xml_error("unsupported XML entity")),
                        },
                    };
                    cap.push_text(ch.encode_utf8(&mut [0; 4]))?;
                }
            }
            Event::DocType(_) => return Err(xml_error("DOCTYPE is not supported")),
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_done || capture.is_some() || !streamed.is_empty() {
        return Err(xml_error("incomplete XML document"));
    }
    Ok(())
}

/// Parses a whole (small) part into one tree.
pub(crate) fn parse_tree(xml: &[u8]) -> Result<Elem, IoError> {
    struct Whole(Option<Elem>, Option<Elem>);
    impl Sink for Whole {
        fn capture(&mut self, _: Ns, _: &str) -> bool {
            true
        }
        fn open(&mut self, el: &Elem) -> Result<(), IoError> {
            self.0 = Some(Elem { kids: Vec::new(), ..el.clone() });
            Ok(())
        }
        fn close(&mut self, _: &Elem) -> Result<(), IoError> {
            self.1 = self.0.take();
            Ok(())
        }
        fn keeps_subtrees(&self) -> bool {
            true
        }
        fn subtree(&mut self, el: Elem) -> Result<(), IoError> {
            if let Some(root) = self.0.as_mut() {
                root.kids.push(Node::El(el));
            }
            Ok(())
        }
    }
    let mut whole = Whole(None, None);
    parse(xml, &mut whole)?;
    whole.1.ok_or_else(|| xml_error("no root element"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_content_keeps_order_and_odf_whitespace() -> Result<(), IoError> {
        let t = parse_tree(
            br#"<r xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><text:p>  a <text:span>b</text:span>  c<text:s text:c="2"/>d<text:tab/>e</text:p><text:p>x<text:line-break/>y</text:p></r>"#,
        )?;
        assert_eq!(t.paragraphs_text(), "a b c  d\te\nx\ny");
        Ok(())
    }

    #[test]
    fn unbalanced_and_trailing_content_is_refused() {
        assert!(parse_tree(b"<a><b></a>").is_err());
        assert!(parse_tree(b"<a/><b/>").is_err());
        assert!(parse_tree(b"<a>").is_err());
    }
}
