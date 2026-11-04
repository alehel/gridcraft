//! Drawing shapes (ODF 1.3 part 3 §10.3–10.5: `draw:custom-shape`, `draw:rect`, `draw:ellipse`,
//! `draw:line`, text-box frames, groups) ↔ GridCraft shapes — kind, box, fill and outline
//! colours, and text with its font, alignment and anchor.

use std::collections::HashMap;
use std::fmt::Write as _;

use gridcraft_model::{Color, Shape, ShapeFormat, ShapeKind, ShapeTextAlign, ShapeTextAnchor, Theme};

use super::dom::{Elem, Ns};
use super::style::{color, hex, length_pt};
use crate::xml::{esc, esc_attr};

/// A shape as read, its box in points from the sheet's origin (or from its cell's corner).
pub(crate) struct ShapeDraft {
    pub kind: ShapeKind,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub fill: Color,
    pub line: Color,
    pub text: String,
    pub format: ShapeFormat,
}

/// Graphic, paragraph and text styles by name, for shapes, and the default graphic style.
#[derive(Default)]
pub(crate) struct DrawStyles {
    pub styles: HashMap<String, Elem>,
    default: Option<Elem>,
}

/// The most parents a graphic style's chain is followed through.
const MAX_STYLE_DEPTH: usize = 16;

impl DrawStyles {
    pub fn add(&mut self, list: &Elem) {
        for s in list.children(Ns::Style, "style") {
            if matches!(s.attr(Ns::Style, "family"), Some("graphic" | "paragraph" | "text"))
                && let Some(n) = s.attr(Ns::Style, "name")
            {
                self.styles.insert(n.to_string(), s.clone());
            }
        }
        if let Some(d) = list.children(Ns::Style, "default-style").find(|d| d.attr(Ns::Style, "family") == Some("graphic")) {
            self.default = Some(d.clone());
        }
    }

    /// A graphic property of a shape's style: its own, its parents', or the default style's.
    fn graphic<'a>(&'a self, e: &Elem, name: &str) -> Option<&'a str> {
        let prop = |s: &'a Elem| s.child(Ns::Style, "graphic-properties").and_then(|g| g.attr(Ns::Draw, name).or_else(|| g.attr(Ns::Svg, name)));
        let mut style = e.attr(Ns::Draw, "style-name").and_then(|n| self.styles.get(n));
        for _ in 0..MAX_STYLE_DEPTH {
            let Some(s) = style else { break };
            if let Some(v) = prop(s) {
                return Some(v);
            }
            style = s.attr(Ns::Style, "parent-style-name").and_then(|n| self.styles.get(n));
        }
        self.default.as_ref().and_then(prop)
    }
    fn text_props<'a>(&'a self, name: Option<&str>) -> Option<&'a Elem> {
        self.styles.get(name?)?.child(Ns::Style, "text-properties")
    }
    /// The text properties a shape's own graphic style states.
    fn shape_text<'a>(&'a self, e: &Elem) -> Option<&'a Elem> {
        self.styles.get(e.attr(Ns::Draw, "style-name")?)?.child(Ns::Style, "text-properties")
    }
}

fn kind_of(e: &Elem) -> Option<ShapeKind> {
    Some(match (e.ns, e.name.as_str()) {
        (Ns::Draw, "rect") => ShapeKind::Rectangle,
        (Ns::Draw, "ellipse" | "circle") => ShapeKind::Ellipse,
        (Ns::Draw, "line" | "connector") => ShapeKind::Line,
        (Ns::Draw, "frame") if e.child(Ns::Draw, "text-box").is_some() => ShapeKind::TextBox,
        (Ns::Draw, "custom-shape") => match e.child(Ns::Draw, "enhanced-geometry").and_then(|g| g.attr(Ns::Draw, "type")).unwrap_or("rectangle") {
            "round-rectangle" | "round-quadrat" | "ooxml-roundRect" => ShapeKind::RoundedRectangle,
            "ellipse" | "circle" | "ooxml-ellipse" => ShapeKind::Ellipse,
            "isosceles-triangle" | "right-triangle" | "ooxml-triangle" => ShapeKind::Triangle,
            _ => ShapeKind::Rectangle,
        },
        _ => return None,
    })
}

/// The paragraphs of a shape (directly or in its text box), and the first run's text style.
fn text_of(e: &Elem) -> (String, Option<String>, Option<String>) {
    let holder = e.child(Ns::Draw, "text-box").unwrap_or(e);
    let first_p = holder.children(Ns::Text, "p").next();
    let p_style = first_p.and_then(|p| p.attr(Ns::Text, "style-name")).map(str::to_string);
    let span_style = first_p.and_then(|p| p.child(Ns::Text, "span")).and_then(|s| s.attr(Ns::Text, "style-name")).map(str::to_string);
    (holder.paragraphs_text(), p_style, span_style)
}

/// Reads a drawing element; groups are read by the caller.
pub(crate) fn read(e: &Elem, styles: &DrawStyles) -> Option<ShapeDraft> {
    let mut kind = kind_of(e)?;
    let len = |n: &str| e.attr(Ns::Svg, n).and_then(length_pt).unwrap_or(0.0);
    let (mut x, mut y, mut width, mut height) = (len("x"), len("y"), len("width"), len("height"));
    if kind == ShapeKind::Line {
        let (x1, y1, x2, y2) = (len("x1"), len("y1"), len("x2"), len("y2"));
        x = x1.min(x2);
        y = y1.min(y2);
        width = (x2 - x1).abs();
        height = (y2 - y1).abs();
        if styles.graphic(e, "marker-end").is_some() || styles.graphic(e, "marker-start").is_some() {
            kind = ShapeKind::Arrow;
        }
    }
    // A shape with a rotation or skew carries its position in `draw:transform`'s translate.
    if let Some(t) = e.attr(Ns::Draw, "transform")
        && let Some(i) = t.find("translate(")
    {
        let args: Vec<f64> = t.get(i + 10..).unwrap_or("").split(')').next().unwrap_or("").split_whitespace().filter_map(length_pt).collect();
        if let [tx, ty] = args.as_slice() {
            x = *tx;
            y = *ty;
        }
    }
    let fill = match (styles.graphic(e, "fill"), styles.graphic(e, "fill-color")) {
        (Some("none"), _) => Color::Auto,
        (_, Some(c)) => color(c).unwrap_or(Color::Auto),
        // LibreOffice's default shape fill, when the file states no default.
        _ if kind != ShapeKind::TextBox && kind != ShapeKind::Line && kind != ShapeKind::Arrow => Color::Rgb(0x729fcf),
        _ => Color::Auto,
    };
    let stroke = styles.graphic(e, "stroke");
    let line = match styles.graphic(e, "stroke-color").and_then(color) {
        Some(c) => c,
        None if kind == ShapeKind::TextBox => Color::Auto,
        // LibreOffice's default outline, when the file states no default.
        None => Color::Rgb(0x3465a4),
    };
    let (text, p_style, span_style) = text_of(e);
    let mut format = ShapeFormat { no_line: stroke == Some("none") || (kind == ShapeKind::TextBox && stroke.is_none()), ..ShapeFormat::default() };
    if let Some(tp) = styles.text_props(span_style.as_deref()).or_else(|| styles.text_props(p_style.as_deref())).or_else(|| styles.shape_text(e)) {
        format.font_size = tp.attr(Ns::Fo, "font-size").and_then(length_pt).map(|v| v as f32);
        format.bold = tp.attr(Ns::Fo, "font-weight").is_some_and(|w| w == "bold" || w.parse::<u32>().is_ok_and(|n| n >= 600));
        format.italic = tp.attr(Ns::Fo, "font-style").is_some_and(|s| s == "italic" || s == "oblique");
        format.text_color = tp.attr(Ns::Fo, "color").and_then(color);
        format.font = tp.attr(Ns::Style, "font-name").or_else(|| tp.attr(Ns::Fo, "font-family")).map(|f| f.trim_matches('\'').to_string());
    }
    format.align = match styles.graphic(e, "textarea-horizontal-align") {
        Some("center") => Some(ShapeTextAlign::Center),
        Some("right") => Some(ShapeTextAlign::Right),
        Some("left") => Some(ShapeTextAlign::Left),
        _ => None,
    };
    format.anchor = match styles.graphic(e, "textarea-vertical-align") {
        Some("middle") => Some(ShapeTextAnchor::Middle),
        Some("bottom") => Some(ShapeTextAnchor::Bottom),
        Some("top") => Some(ShapeTextAnchor::Top),
        _ => None,
    };
    Some(ShapeDraft { kind, x, y, width, height, fill, line, text, format })
}

// ------------------------------------------------------------------------------------- writing

/// The graphic style of a shape (`style:style` family graphic) named `name`.
pub(crate) fn style_xml(name: &str, s: &Shape, theme: &Theme) -> String {
    let mut g = String::new();
    match hex(s.fill, theme) {
        Some(c) => {
            let _ = write!(g, " draw:fill=\"solid\" draw:fill-color=\"{c}\"");
        }
        None => g.push_str(" draw:fill=\"none\""),
    }
    match (s.format.no_line, hex(s.line, theme)) {
        (true, _) => g.push_str(" draw:stroke=\"none\""),
        (false, Some(c)) => {
            let _ = write!(g, " draw:stroke=\"solid\" svg:stroke-color=\"{c}\"");
        }
        // ODF has no automatic stroke colour: an automatic outline is written as the grey
        // GridCraft draws it in (a text box's too: its outline shows unless it has none).
        (false, None) => g.push_str(" draw:stroke=\"solid\" svg:stroke-color=\"#606060\""),
    }
    if s.kind == ShapeKind::Arrow {
        g.push_str(" draw:marker-end=\"Arrow\" draw:marker-end-width=\"0.2cm\"");
    }
    let h = match s.format.align {
        Some(ShapeTextAlign::Left) => "left",
        Some(ShapeTextAlign::Right) => "right",
        Some(ShapeTextAlign::Center) => "center",
        None if s.kind == ShapeKind::TextBox => "left",
        None => "center",
    };
    let v = match s.format.anchor {
        Some(ShapeTextAnchor::Top) => "top",
        Some(ShapeTextAnchor::Bottom) => "bottom",
        Some(ShapeTextAnchor::Middle) => "middle",
        None if s.kind == ShapeKind::TextBox => "top",
        None => "middle",
    };
    let _ = write!(g, " draw:textarea-horizontal-align=\"{h}\" draw:textarea-vertical-align=\"{v}\"");
    let mut t = String::new();
    if let Some(size) = s.format.font_size {
        let _ = write!(t, " fo:font-size=\"{size}pt\"");
    }
    if s.format.bold {
        t.push_str(" fo:font-weight=\"bold\"");
    }
    if s.format.italic {
        t.push_str(" fo:font-style=\"italic\"");
    }
    if let Some(c) = s.format.text_color.and_then(|c| hex(c, theme)) {
        let _ = write!(t, " fo:color=\"{c}\"");
    }
    if let Some(f) = &s.format.font {
        let _ = write!(t, " fo:font-family=\"{}\"", esc_attr(f));
    }
    let text = if t.is_empty() { String::new() } else { format!("<style:text-properties{t}/>") };
    format!(r#"<style:style style:name="{name}" style:family="graphic"><style:graphic-properties{g}/>{text}</style:style>"#)
}

/// The drawing element of `s` with graphic style `style`, at `(x, y)` points.
pub(crate) fn shape_xml(s: &Shape, style: &str, x: f32, y: f32, z: usize) -> Option<String> {
    let w = s.anchor.width * 0.75;
    let h = s.anchor.height * 0.75;
    let mut text = String::new();
    for line in s.text.split('\n').filter(|_| !s.text.is_empty()) {
        let _ = write!(text, "<text:p>{}</text:p>", esc(line));
    }
    let common = format!(r#"draw:z-index="{z}" draw:name="Shape {}" draw:style-name="{style}""#, s.id);
    Some(match s.kind {
        ShapeKind::Line | ShapeKind::Arrow => {
            format!(r#"<draw:line {common} svg:x1="{x}pt" svg:y1="{y}pt" svg:x2="{}pt" svg:y2="{}pt">{text}</draw:line>"#, x + w, y + h)
        }
        ShapeKind::TextBox => format!(
            r#"<draw:frame {common} svg:x="{x}pt" svg:y="{y}pt" svg:width="{w}pt" svg:height="{h}pt"><draw:text-box>{text}</draw:text-box></draw:frame>"#
        ),
        ShapeKind::Rectangle | ShapeKind::RoundedRectangle | ShapeKind::Ellipse | ShapeKind::Triangle => {
            let ty = match s.kind {
                ShapeKind::RoundedRectangle => "round-rectangle",
                ShapeKind::Ellipse => "ellipse",
                ShapeKind::Triangle => "isosceles-triangle",
                _ => "rectangle",
            };
            format!(
                r#"<draw:custom-shape {common} svg:x="{x}pt" svg:y="{y}pt" svg:width="{w}pt" svg:height="{h}pt">{text}<draw:enhanced-geometry svg:viewBox="0 0 21600 21600" draw:type="{ty}"/></draw:custom-shape>"#
            )
        }
        // Icons, ink and equations have no ODF drawing counterpart.
        ShapeKind::Icon | ShapeKind::Ink | ShapeKind::Equation => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::super::dom::parse_tree;
    use super::*;

    #[test]
    fn shapes_survive_writing_and_reading() {
        for kind in [
            ShapeKind::Rectangle,
            ShapeKind::RoundedRectangle,
            ShapeKind::Ellipse,
            ShapeKind::Triangle,
            ShapeKind::TextBox,
            ShapeKind::Line,
            ShapeKind::Arrow,
        ] {
            let s = Shape {
                id: 1,
                kind,
                anchor: gridcraft_model::Anchor { width: 120.0, height: 60.0, ..Default::default() },
                fill: if matches!(kind, ShapeKind::Line | ShapeKind::Arrow | ShapeKind::TextBox) { Color::Auto } else { Color::Rgb(0xff3366) },
                line: Color::Rgb(0x112233),
                text: "Hello\nworld".into(),
                format: ShapeFormat { bold: true, font_size: Some(14.0), ..ShapeFormat::default() },
                math: Default::default(),
            };
            let doc = format!(
                r#"<r xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"><office:automatic-styles>{}</office:automatic-styles>{}</r>"#,
                style_xml("gr1", &s, &Theme::default()),
                shape_xml(&s, "gr1", 10.0, 20.0, 0).unwrap()
            );
            let tree = parse_tree(doc.as_bytes()).unwrap();
            let mut styles = DrawStyles::default();
            styles.add(tree.child(Ns::Office, "automatic-styles").unwrap());
            let el = tree.elems().nth(1).unwrap();
            let d = read(el, &styles).unwrap();
            assert_eq!(d.kind, kind);
            assert_eq!((d.x, d.y), (10.0, 20.0));
            assert_eq!((d.width, d.height), (90.0, 45.0));
            assert_eq!(d.fill, s.fill, "{kind:?}");
            assert_eq!(d.line, Color::Rgb(0x112233));
            if !matches!(kind, ShapeKind::Line | ShapeKind::Arrow) {
                assert_eq!(d.text, "Hello\nworld");
            }
            assert!(d.format.bold);
            assert_eq!(d.format.font_size, Some(14.0));
        }
    }

    /// Fill and outline come from the shape's style, its parents, or the default graphic style.
    #[test]
    fn graphic_properties_are_inherited() {
        let doc = r##"<r xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0"><office:styles><style:default-style style:family="graphic"><style:graphic-properties svg:stroke-color="#00ff00"/></style:default-style><style:style style:name="Shapes" style:family="graphic"><style:graphic-properties draw:fill-color="#ff0000"/></style:style><style:style style:name="gr1" style:family="graphic" style:parent-style-name="Shapes"><style:graphic-properties draw:textarea-vertical-align="top"/></style:style></office:styles><draw:rect draw:style-name="gr1" svg:width="10pt" svg:height="10pt"/></r>"##;
        let tree = parse_tree(doc.as_bytes()).unwrap();
        let mut styles = DrawStyles::default();
        styles.add(tree.child(Ns::Office, "styles").unwrap());
        let d = read(tree.child(Ns::Draw, "rect").unwrap(), &styles).unwrap();
        assert_eq!(d.fill, Color::Rgb(0xff0000));
        assert_eq!(d.line, Color::Rgb(0x00ff00));
        assert_eq!(d.format.anchor, Some(ShapeTextAnchor::Top));
    }
}
