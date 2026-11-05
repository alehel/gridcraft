//! OpenDocument Spreadsheet export: content, styles, number formats, column widths and row
//! heights, outlines, merges, formulas, notes, links, named ranges, validation, conditional
//! formatting, autofilters, pictures, page setup, view settings and document properties.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use gridcraft_core::date::datetime_from_serial;
use gridcraft_core::{CellRef, DateSystem, MAX_COLS, MAX_ROWS, RangeRef, Value, col_to_letters};
use gridcraft_model::{
    Cell, CfOperator, CfRule, CfValueKind, Color, ErrorStyle, LineInfo, Sheet, Style, StyleId, Validation, ValidationKind, Visibility, Workbook,
};

use super::formula::{self, Tables};
use super::read::{anchor_pt, baseline, sheet_lines};
use super::{OdsKind, numfmt, page, settings, style};
use crate::IoError;
use crate::package::ZipOut;
use crate::xml::{esc, esc_attr};

const NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0" xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2" xmlns:loext="urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0" xmlns:calcext="urn:org:documentfoundation:names:experimental:calc:xmlns:calcext:1.0" xmlns:tableooo="http://openoffice.org/2009/table" xmlns:config="urn:oasis:names:tc:opendocument:xmlns:config:1.0" xmlns:ooo="http://openoffice.org/2004/office" xmlns:chart="urn:oasis:names:tc:opendocument:xmlns:chart:1.0""#;

/// Encodes `wb` as an `.ods` package. Cells cache their values as plain text; see
/// [`write_ods_with`] to cache the text as the cells show it.
pub fn write_ods(wb: &Workbook) -> Result<Vec<u8>, IoError> {
    write_ods_with(wb, OdsKind::Spreadsheet, None)
}

/// [`write_ods`], with each value's cached text from `display` (the value as its number format
/// shows it), as spreadsheet applications write it.
pub fn write_ods_with(wb: &Workbook, kind: OdsKind, display: Option<Display<'_>>) -> Result<Vec<u8>, IoError> {
    let mut w = Writer::new(wb, display);
    let parts = w.parts();
    let mut zip = ZipOut::new();
    // ODF 1.3 part 2 §3.3: `mimetype` comes first and is stored uncompressed.
    zip.add_stored("mimetype", kind.mimetype().as_bytes())?;
    let mut manifest = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3"><manifest:file-entry manifest:full-path="/" manifest:version="1.3" manifest:media-type="{}"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="meta.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="settings.xml" manifest:media-type="text/xml"/>"#,
        kind.mimetype()
    );
    for (path, mime, _) in &w.pictures {
        let _ = write!(manifest, r#"<manifest:file-entry manifest:full-path="{}" manifest:media-type="{}"/>"#, esc_attr(path), esc_attr(mime));
    }
    for (dir, _) in &w.objects {
        let _ = write!(
            manifest,
            r#"<manifest:file-entry manifest:full-path="{0}/" manifest:version="1.3" manifest:media-type="application/vnd.oasis.opendocument.chart"/><manifest:file-entry manifest:full-path="{0}/content.xml" manifest:media-type="text/xml"/>"#,
            esc_attr(dir)
        );
    }
    manifest.push_str("</manifest:manifest>");
    let Parts { fonts, styles, content_auto, styles_auto, masters, body } = parts;
    let content = document("document-content", &format!("{fonts}<office:automatic-styles>{content_auto}</office:automatic-styles>{body}"));
    let styles = document("document-styles", &format!("{fonts}{styles}<office:automatic-styles>{styles_auto}</office:automatic-styles>{masters}"));
    zip.add("META-INF/manifest.xml", manifest.as_bytes())?;
    zip.add("content.xml", content.as_bytes())?;
    zip.add("styles.xml", styles.as_bytes())?;
    zip.add("meta.xml", document("document-meta", &meta(wb)).as_bytes())?;
    zip.add("settings.xml", document("document-settings", &settings::write(wb)).as_bytes())?;
    for (path, _, data) in &w.pictures {
        zip.add(path, data)?;
    }
    for (dir, chart) in &w.objects {
        zip.add(&format!("{dir}/content.xml"), chart.package().as_bytes())?;
    }
    zip.finish()
}

/// A package part: `inner` in the root element `office:{root}`.
fn document(root: &str, inner: &str) -> String {
    format!(r#"<?xml version="1.0" encoding="UTF-8"?><office:{root} {NS} office:version="1.3">{inner}</office:{root}>"#)
}

/// What the parts of a document hold, each as its element (or, for automatic styles, its
/// children): a package puts them in its parts, a flat document in one tree.
struct Parts {
    fonts: String,
    /// `office:styles`: the default and named styles.
    styles: String,
    /// The automatic styles of the content (columns, rows, tables, cells, data styles…) and of
    /// the styles part (page layouts).
    content_auto: String,
    styles_auto: String,
    masters: String,
    body: String,
}

/// Encodes `wb` as a flat OpenDocument spreadsheet (`.fods`): one XML document holding what a
/// package splits into parts, pictures as base64 and charts as inline documents.
pub fn write_fods(wb: &Workbook, display: Option<Display<'_>>) -> Result<Vec<u8>, IoError> {
    let mut w = Writer::new(wb, display);
    w.flat = true;
    let Parts { fonts, styles, content_auto, styles_auto, masters, body } = w.parts();
    let o = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><office:document {NS} office:version="1.3" office:mimetype="application/vnd.oasis.opendocument.spreadsheet">{}{}{fonts}{styles}<office:automatic-styles>{content_auto}{styles_auto}</office:automatic-styles>{masters}{body}</office:document>"#,
        meta(wb),
        settings::write(wb)
    );
    Ok(o.into_bytes())
}

fn meta(wb: &Workbook) -> String {
    let mut o = String::from("<office:meta><meta:generator>GridCraft</meta:generator>");
    let p = &wb.props;
    if !p.title.is_empty() {
        let _ = write!(o, "<dc:title>{}</dc:title>", esc(&p.title));
    }
    if !p.subject.is_empty() {
        let _ = write!(o, "<dc:subject>{}</dc:subject>", esc(&p.subject));
    }
    if !p.description.is_empty() {
        let _ = write!(o, "<dc:description>{}</dc:description>", esc(&p.description));
    }
    if !p.author.is_empty() && !wb.remove_personal_info {
        let _ = write!(o, "<meta:initial-creator>{}</meta:initial-creator>", esc(&p.author));
    }
    for k in p.keywords.split([',', ';']).map(str::trim).filter(|k| !k.is_empty()) {
        let _ = write!(o, "<meta:keyword>{}</meta:keyword>", esc(k));
    }
    o.push_str("</office:meta>");
    o
}

/// Structured references resolved against the workbook's tables.
struct WbTables<'a>(&'a Workbook);

impl Tables for WbTables<'_> {
    fn resolve(&self, s: &gridcraft_formula::StructRef, at: CellRef) -> Option<(String, RangeRef)> {
        use gridcraft_formula::StructItem;
        let (sheet, t) = self.0.sheets.iter().find_map(|sh| {
            if s.table.is_empty() {
                sh.tables.iter().find(|t| t.range.contains(at)).map(|t| (sh, t))
            } else {
                sh.tables.iter().find(|t| t.name.eq_ignore_ascii_case(&s.table)).map(|t| (sh, t))
            }
        })?;
        let r = t.range;
        let header = if t.header_row { 1 } else { 0 };
        let totals = if t.totals_row { 1 } else { 0 };
        let (mut r0, mut r1) = (r.start.row + header, r.end.row.saturating_sub(totals));
        for item in &s.specifiers {
            match item {
                StructItem::All => {
                    r0 = r.start.row;
                    r1 = r.end.row;
                }
                StructItem::Headers => {
                    r0 = r.start.row;
                    if !s.specifiers.contains(&StructItem::Data) {
                        r1 = r.start.row;
                    }
                }
                StructItem::Totals => {
                    r1 = r.end.row;
                    if !s.specifiers.contains(&StructItem::Data) {
                        r0 = r.end.row;
                    }
                }
                StructItem::ThisRow => {
                    r0 = at.row;
                    r1 = at.row;
                }
                StructItem::Data => {}
            }
        }
        let col = |name: &Option<String>| name.as_ref().and_then(|n| t.column_index(n)).map(|i| r.start.col + i as u32);
        let c0 = col(&s.col_start).unwrap_or(r.start.col);
        let c1 = col(&s.col_end).or_else(|| col(&s.col_start)).unwrap_or(r.end.col);
        Some((sheet.name.clone(), RangeRef::new(CellRef::new(r0, c0), CellRef::new(r1.max(r0), c1))))
    }
}

/// Formats a cell's value as it shows, for the text a file caches next to the value.
pub type Display<'a> = &'a dyn Fn(&Sheet, CellRef) -> String;

struct Writer<'a> {
    wb: &'a Workbook,
    display: Option<Display<'a>>,
    /// A flat document: pictures and charts inline instead of package parts.
    flat: bool,
    /// Automatic cell styles: style id → name.
    cell_styles: BTreeMap<StyleId, String>,
    /// Data styles by format code: name, elements, and whether a named style uses it (it then
    /// goes with the named styles, where every style can refer to it).
    data_styles: HashMap<String, (String, String, bool)>,
    /// Format codes in the order their data styles were made.
    data_order: Vec<String>,
    /// Column and row styles by size (hundredths of a pixel), and for rows whether optimal.
    col_styles: BTreeMap<u32, String>,
    row_styles: BTreeMap<(u32, bool), String>,
    /// Named conditional-format styles (`ConditionalStyle_N`) and their properties.
    cf_styles: Vec<String>,
    fonts: std::collections::BTreeSet<String>,
    pictures: Vec<(String, String, std::sync::Arc<Vec<u8>>)>,
    /// Embedded chart objects: folder (`Object 1`) and its document.
    objects: Vec<(String, super::chart::ChartDoc)>,
    /// Graphic styles of shapes.
    graphics: Vec<String>,
    validations: Vec<(String, String)>,
}

/// The merges and validation ranges reaching each row, kept as the rows are written in order.
struct Reach<'s> {
    /// Merges (no name) and validation ranges (with their validation's name).
    ranges: Vec<(RangeRef, Option<&'s str>)>,
    /// `ranges` by first row; the ones before `next` have been reached.
    order: Vec<usize>,
    next: usize,
    active: Vec<usize>,
}

impl<'s> Reach<'s> {
    fn new(merges: &[RangeRef], validations: &'s [(RangeRef, String)]) -> Self {
        let ranges: Vec<(RangeRef, Option<&'s str>)> =
            merges.iter().map(|m| (*m, None)).chain(validations.iter().map(|(r, n)| (*r, Some(n.as_str())))).collect();
        let mut order: Vec<usize> = (0..ranges.len()).collect();
        order.sort_by_key(|i| ranges.get(*i).map_or(0, |(r, _)| r.start.row));
        Reach { ranges, order, next: 0, active: Vec::new() }
    }

    /// Moves to `row` (rows only move down): ranges that ended above it leave, ranges starting
    /// at or above it join.
    fn at(&mut self, row: u32) {
        let ranges = &self.ranges;
        self.active.retain(|i| ranges.get(*i).is_some_and(|(r, _)| r.end.row >= row));
        while let Some(&i) = self.order.get(self.next) {
            let Some((r, _)) = ranges.get(i) else { break };
            if r.start.row > row {
                break;
            }
            self.next += 1;
            if r.end.row >= row {
                self.active.push(i);
            }
        }
    }

    /// The first row below `row` whose ranges differ from `row`'s.
    fn next_change(&self, row: u32) -> u32 {
        let starts = self.order.get(self.next).and_then(|i| self.ranges.get(*i)).map_or(u32::MAX, |(r, _)| r.start.row);
        let ends = self.active().map(|(r, _)| r.end.row.saturating_add(1)).min().unwrap_or(u32::MAX);
        // A merge's first row differs from the rows it covers below it.
        let firsts = self.active().filter(|(r, n)| n.is_none() && r.start.row == row).map(|_| row + 1).min().unwrap_or(u32::MAX);
        starts.min(ends).min(firsts)
    }

    fn active(&self) -> impl Iterator<Item = &(RangeRef, Option<&'s str>)> {
        self.active.iter().filter_map(|i| self.ranges.get(*i))
    }
}

/// The outline groups and the header block (rows or columns to repeat on each printed page)
/// around lines (rows or columns) written in order. The header block holds lines only, never a
/// group's edge: inside it the outline stays at its first line's level.
struct Outline {
    /// `row` or `column`.
    axis: &'static str,
    level: u8,
    header: Option<(u32, u32)>,
}

impl Outline {
    /// `title`'s span (either order), when it starts on the sheet of `max` lines, ending on it.
    fn new(axis: &'static str, title: Option<(u32, u32)>, max: u32) -> Outline {
        let header = title.map(|(a, b)| (a.min(b), a.max(b).min(max - 1))).filter(|(a, _)| *a < max);
        Outline { axis, level: 0, header }
    }

    /// Before a run starting at `line` in group level `outline`: the groups and header it opens.
    fn enter(&mut self, out: &mut String, line: u32, outline: u8, hidden: bool) {
        let in_header = self.header.is_some_and(|(a, b)| line > a && line <= b);
        let target = if in_header { self.level } else { outline.min(7) };
        while self.level < target {
            self.level += 1;
            let _ = write!(out, "<table:table-{}-group{}>", self.axis, if hidden { " table:display=\"false\"" } else { "" });
        }
        self.close_to(out, target);
        if self.header.is_some_and(|(a, _)| a == line) {
            let _ = write!(out, "<table:table-header-{}s>", self.axis);
        }
    }

    /// Whether a run may go on from the line before `next` to `next`: not across the header's
    /// edges.
    fn continues(&self, next: u32) -> bool {
        self.header.is_none_or(|(a, b)| next != a && next.checked_sub(1) != Some(b))
    }

    /// After a run of `count` lines from `line`: the header it closes.
    fn leave(&mut self, out: &mut String, line: u32, count: u32) {
        if self.header.is_some_and(|(_, b)| b >= line && b - line < count) {
            let _ = write!(out, "</table:table-header-{}s>", self.axis);
        }
    }

    /// Closes groups down to `level` (0: all).
    fn close_to(&mut self, out: &mut String, level: u8) {
        while self.level > level {
            self.level -= 1;
            let _ = write!(out, "</table:table-{}-group>", self.axis);
        }
    }
}

fn px_to_in(px: f32) -> String {
    format!("{:.4}in", f64::from(px) / 96.0)
}

impl<'a> Writer<'a> {
    fn new(wb: &'a Workbook, display: Option<Display<'a>>) -> Self {
        Writer {
            wb,
            display,
            flat: false,
            cell_styles: BTreeMap::new(),
            data_styles: HashMap::new(),
            data_order: Vec::new(),
            col_styles: BTreeMap::new(),
            row_styles: BTreeMap::new(),
            cf_styles: Vec::new(),
            fonts: std::collections::BTreeSet::new(),
            pictures: Vec::new(),
            objects: Vec::new(),
            graphics: Vec::new(),
            validations: Vec::new(),
        }
    }

    fn font(&mut self, name: &str) {
        if !self.fonts.contains(name) {
            self.fonts.insert(name.to_string());
        }
    }

    /// The name of the data style for `code` (`None` for General), made on first use; `named`
    /// when a named style uses it.
    fn data_style(&mut self, code: &str, named: bool) -> Option<String> {
        if let Some((n, _, used_by_named)) = self.data_styles.get_mut(code) {
            *used_by_named |= named;
            return Some(n.clone());
        }
        let name = format!("N{}", self.data_styles.len() + 100);
        let xml = numfmt::write_style(&name, code)?;
        self.data_styles.insert(code.to_string(), (name.clone(), xml, named));
        self.data_order.push(code.to_string());
        Some(name)
    }

    /// The data styles kept with the named styles (`named`) or with the automatic ones.
    fn data_styles_xml(&self, named: bool) -> String {
        self.data_order.iter().filter_map(|c| self.data_styles.get(c)).filter(|(_, _, n)| *n == named).map(|(_, xml, _)| xml.as_str()).collect()
    }

    /// A table-cell style named `name`: what `s` sets beyond `base`, with its data style.
    fn cell_style_xml(&mut self, name: &str, s: &Style, base: &Style, parent: Option<&str>, named: bool) -> String {
        // The font a style names is declared with the document's font faces.
        if s.font.name != base.font.name {
            self.font(&s.font.name);
        }
        let data = if s.num_fmt != base.num_fmt { self.data_style(s.num_fmt.as_str(), named) } else { None };
        let mut o = format!(r#"<style:style style:name="{}" style:family="table-cell""#, esc_attr(name));
        if let Some(p) = parent {
            let _ = write!(o, r#" style:parent-style-name="{}""#, esc_attr(p));
        }
        if let Some(d) = data {
            let _ = write!(o, r#" style:data-style-name="{d}""#);
        }
        let _ = write!(o, ">{}</style:style>", style::write_props(s, base, &self.wb.theme));
        o
    }

    fn cell_style(&mut self, id: StyleId) -> String {
        if id == StyleId::DEFAULT {
            return "Default".into();
        }
        if let Some(n) = self.cell_styles.get(&id) {
            return n.clone();
        }
        let name = format!("ce{}", id.0);
        self.cell_styles.insert(id, name.clone());
        name
    }

    fn col_style(&mut self, width: f32) -> String {
        let key = (width * 100.0).round() as u32;
        let n = self.col_styles.len() + 1;
        self.col_styles.entry(key).or_insert_with(|| format!("co{n}")).clone()
    }

    fn row_style(&mut self, height: f32, optimal: bool) -> String {
        let key = (height * 100.0).round() as u32;
        let n = self.row_styles.len() + 1;
        self.row_styles.entry((key, optimal)).or_insert_with(|| format!("ro{n}")).clone()
    }

    /// The content's automatic styles but its data styles (they are placed once all are known).
    fn automatic_styles(&mut self) -> String {
        let mut o = String::new();
        for (key, name) in &self.col_styles {
            let _ = write!(
                o,
                r#"<style:style style:name="{name}" style:family="table-column"><style:table-column-properties fo:break-before="auto" style:column-width="{}"/></style:style>"#,
                px_to_in(*key as f32 / 100.0)
            );
        }
        for ((key, optimal), name) in &self.row_styles {
            let _ = write!(
                o,
                r#"<style:style style:name="{name}" style:family="table-row"><style:table-row-properties style:row-height="{}" fo:break-before="auto" style:use-optimal-row-height="{optimal}"/></style:style>"#,
                px_to_in(*key as f32 / 100.0)
            );
        }
        for (i, sh) in self.wb.sheets.iter().enumerate() {
            let mut props = format!(" table:display=\"{}\"", sh.visibility == Visibility::Visible);
            props.push_str(if sh.right_to_left { " style:writing-mode=\"rl-tb\"" } else { " style:writing-mode=\"lr-tb\"" });
            if let Some(c) = sh.tab_color.and_then(|c| c.resolve(&self.wb.theme)) {
                let _ = write!(props, " tableooo:tab-color=\"#{:02x}{:02x}{:02x}\"", c[0], c[1], c[2]);
            }
            let _ = write!(
                o,
                r#"<style:style style:name="ta{i}" style:family="table" style:master-page-name="mp{i}"><style:table-properties{props}/></style:style>"#
            );
        }
        let default = self.wb.styles.get(StyleId::DEFAULT).clone();
        let ids: Vec<(StyleId, String)> = self.cell_styles.iter().map(|(k, v)| (*k, v.clone())).collect();
        for (id, name) in ids {
            let s = self.wb.styles.get(id).clone();
            o.push_str(&self.cell_style_xml(&name, &s, &default, Some("Default"), false));
        }
        o.push_str("<style:style style:name=\"gr1\" style:family=\"graphic\"><style:graphic-properties draw:stroke=\"none\" draw:fill=\"none\"/></style:style>");
        for g in &self.graphics {
            o.push_str(g);
        }
        o
    }

    /// The document's parts, made in the order their contents depend on: the body names the
    /// styles it uses, the automatic and named styles name their data styles and fonts.
    fn parts(&mut self) -> Parts {
        let body = self.body();
        let cells = self.automatic_styles();
        let (styles, styles_auto, masters) = self.styles();
        let content_auto = format!("{}{cells}", self.data_styles_xml(false));
        let styles = format!("<office:styles>{}{styles}</office:styles>", self.data_styles_xml(true));
        let fonts = self.font_decls();
        Parts { fonts, styles, content_auto, styles_auto, masters, body }
    }

    /// `office:body`: the spreadsheet.
    fn body(&mut self) -> String {
        let mut body = String::from("<office:body><office:spreadsheet");
        if self.wb.protected_structure {
            body.push_str(" table:structure-protected=\"true\"");
            body.push_str(&protection_key(self.wb.structure_password.as_ref()));
        }
        body.push('>');
        let calc = &self.wb.calc;
        let d1904 = self.wb.date_system == DateSystem::D1904;
        // Always stated: ODF's defaults (case-sensitive text, regular expressions in
        // criteria, labels found automatically) are not how the workbook calculates.
        let _ = write!(
            body,
            "<table:calculation-settings table:case-sensitive=\"false\" table:precision-as-shown=\"{}\" table:automatic-find-labels=\"false\" table:use-regular-expressions=\"false\" table:use-wildcards=\"true\">",
            calc.precision_as_displayed
        );
        if d1904 {
            body.push_str("<table:null-date table:date-value=\"1904-01-01\"/>");
        }
        if calc.iterative {
            let _ = write!(
                body,
                "<table:iteration table:status=\"enable\" table:steps=\"{}\" table:maximum-difference=\"{}\"/>",
                calc.max_iterations, calc.max_change
            );
        }
        body.push_str("</table:calculation-settings>");
        let mut tables = String::new();
        for i in 0..self.wb.sheets.len() {
            self.table(i, &mut tables);
        }
        if !self.validations.is_empty() {
            body.push_str("<table:content-validations>");
            for (_, xml) in &self.validations {
                body.push_str(xml);
            }
            body.push_str("</table:content-validations>");
        }
        body.push_str(&tables);
        body.push_str(&self.names(None));
        body.push_str(&self.database_ranges());
        body.push_str(&super::pivot::write(self.wb));
        body.push_str("</office:spreadsheet></office:body>");
        body
    }

    fn font_decls(&mut self) -> String {
        let default = self.wb.styles.get(StyleId::DEFAULT).font.name.clone();
        self.font(&default);
        // Each font's family class, as the styles naming it hold it.
        let mut classes: HashMap<&str, &'static str> = HashMap::new();
        for (_, st) in self.wb.styles.iter() {
            if let Some(g) = st.font.family.and_then(style::font_generic) {
                classes.entry(st.font.name.as_str()).or_insert(g);
            }
        }
        let mut o = String::from("<office:font-face-decls>");
        for f in &self.fonts {
            let generic = classes.get(f.as_str()).map(|g| format!(" style:font-family-generic=\"{g}\"")).unwrap_or_default();
            let _ = write!(o, r#"<style:font-face style:name="{0}" svg:font-family="'{0}'"{generic}/>"#, esc_attr(f));
        }
        o.push_str("</office:font-face-decls>");
        o
    }

    /// Named ranges and expressions of one sheet (`Some(i)`) or the workbook (`None`).
    fn names(&self, scope: Option<usize>) -> String {
        let mut o = String::new();
        let base_sheet = scope.and_then(|i| self.wb.sheets.get(i)).or_else(|| self.wb.sheets.first()).map(|s| s.name.clone()).unwrap_or_default();
        let base = format!("${}.$A$1", formula::quote_sheet(&base_sheet));
        // ODF has no hidden names: a hidden name is written as a plain one rather than lost, so
        // formulas that use it keep working.
        for n in self.wb.names.iter().filter(|n| n.scope == scope && !n.name.starts_with("_xlnm")) {
            let Ok(e) = gridcraft_formula::parse(&n.formula) else { continue };
            if gridcraft_formula::parse_name(&n.name).is_err() {
                continue;
            }
            match &e {
                gridcraft_formula::Expr::Ref(r) => {
                    // A cell range address names cells: whole columns and rows by their cells.
                    let addr = formula::address(&formula::cell_range(r));
                    let _ = write!(
                        o,
                        r#"<table:named-range table:name="{}" table:base-cell-address="{}" table:cell-range-address="{}"/>"#,
                        esc_attr(&n.name),
                        esc_attr(&base),
                        esc_attr(&addr)
                    );
                }
                _ => {
                    let text = formula::to_odf(
                        &e,
                        CellRef::new(0, 0),
                        scope.and_then(|i| self.wb.sheets.get(i)).map(|s| s.name.as_str()),
                        &WbTables(self.wb),
                    );
                    let _ = write!(
                        o,
                        r#"<table:named-expression table:name="{}" table:base-cell-address="{}" table:expression="{}"/>"#,
                        esc_attr(&n.name),
                        esc_attr(&base),
                        esc_attr(&text)
                    );
                }
            }
        }
        if o.is_empty() { o } else { format!("<table:named-expressions>{o}</table:named-expressions>") }
    }

    fn database_ranges(&self) -> String {
        let mut o = String::new();
        // One database range: its name, range, whether it shows filter buttons, its filter.
        let range = |o: &mut String,
                     name: &str,
                     sheet: &str,
                     r: RangeRef,
                     header: bool,
                     buttons: bool,
                     criteria: &[(u32, gridcraft_model::FilterCriterion)]| {
            let _ = write!(
                o,
                r#"<table:database-range table:name="{}" table:target-range-address="{}" table:contains-header="{header}" table:display-filter-buttons="{buttons}">"#,
                esc_attr(name),
                esc_attr(&formula::range_address(sheet, r))
            );
            // Only the criteria ODF can hold are written; with none, no filter element.
            let mut conditions = String::new();
            for (col, crit) in criteria {
                filter_condition(&mut conditions, *col, crit);
            }
            if !conditions.is_empty() {
                let _ = write!(o, "<table:filter><table:filter-and>{conditions}</table:filter-and></table:filter>");
            }
            o.push_str("</table:database-range>");
        };
        for (i, sh) in self.wb.sheets.iter().enumerate() {
            if let Some(f) = &sh.autofilter {
                range(&mut o, &format!("__Anonymous_Sheet_DB__{i}"), &sh.name, f.range, true, true, &f.criteria);
            }
            // A table is ODF's named database range: its name stays usable in formulas. Its
            // filter isn't written: a database range with buttons reads as the sheet's autofilter.
            for t in &sh.tables {
                range(&mut o, &t.name, &sh.name, t.range, t.header_row, false, &[]);
            }
        }
        if o.is_empty() { o } else { format!("<table:database-ranges>{o}</table:database-ranges>") }
    }

    fn table(&mut self, i: usize, out: &mut String) {
        let Some(sh) = self.wb.sheets.get(i) else { return };
        let sh: &Sheet = sh;
        let mut attrs = format!(" table:name=\"{}\" table:style-name=\"ta{i}\"", esc_attr(&sh.name));
        if sh.print.excluded {
            attrs.push_str(" table:print=\"false\"");
        }
        if let Some(area) = sh.print.print_area {
            let _ = write!(attrs, " table:print-ranges=\"{}\"", esc_attr(&formula::range_address(&sh.name, area)));
        }
        if let Some(p) = &sh.protection {
            attrs.push_str(" table:protected=\"true\"");
            attrs.push_str(&protection_key(p.password.as_ref()));
        }
        let _ = write!(out, "<table:table{attrs}>");
        if let Some(p) = &sh.protection {
            let _ = write!(
                out,
                "<loext:table-protection loext:select-protected-cells=\"{}\" loext:select-unprotected-cells=\"{}\"/>",
                p.select_locked, p.select_unlocked
            );
        }
        self.shapes(sh, out);
        // Extent: every cell, row and column the sheet says anything about.
        let used = sh.cells.used_range();
        let mut max_row = used.map_or(0, |r| r.end.row);
        let mut max_col = used.map_or(0, |r| r.end.col);
        for m in &sh.merges {
            max_row = max_row.max(m.end.row);
            max_col = max_col.max(m.end.col);
        }
        for c in sh.comments.keys() {
            max_row = max_row.max(c.row);
            max_col = max_col.max(c.col);
        }
        for c in sh.hyperlinks.keys().chain(sh.spill.keys()) {
            max_row = max_row.max(c.row);
            max_col = max_col.max(c.col);
        }
        if let Some((&r, _)) = sh.rows.iter().next_back() {
            max_row = max_row.max(r);
        }
        if let Some((&c, _)) = sh.cols.iter().next_back() {
            max_col = max_col.max(c);
        }
        // Rows and columns to repeat on each printed page are lines like any other.
        if let Some((_, b)) = Outline::new("row", sh.print.title_rows, MAX_ROWS).header {
            max_row = max_row.max(b);
        }
        if let Some((_, b)) = Outline::new("column", sh.print.title_cols, MAX_COLS).header {
            max_col = max_col.max(b);
        }
        // What the model holds past the grid (only a hostile one) is not written.
        let (mut max_row, mut max_col) = (max_row.min(MAX_ROWS - 1), max_col.min(MAX_COLS - 1));
        self.columns(sh, max_col, out);
        // Validations: one per rule, named on the cells it covers.
        let mut valid_at: Vec<(RangeRef, String)> = Vec::new();
        for v in &sh.validations {
            let name = format!("val{}", self.validations.len() + 1);
            let first = v.ranges.first().map(|r| r.start).unwrap_or_default();
            self.validations.push((name.clone(), validation_xml(&name, v, &sh.name, first, self.wb)));
            for r in &v.ranges {
                if r.start.row >= MAX_ROWS || r.start.col >= MAX_COLS {
                    continue;
                }
                let r = RangeRef::new(r.start, CellRef::new(r.end.row.min(MAX_ROWS - 1), r.end.col.min(MAX_COLS - 1)));
                valid_at.push((r, name.clone()));
                max_row = max_row.max(r.end.row);
                max_col = max_col.max(r.end.col);
            }
        }
        self.rows(sh, max_row, max_col, &valid_at, out);
        out.push_str(&self.names(Some(i)));
        self.cond_formats(sh, out);
        out.push_str("</table:table>");
    }

    fn columns(&mut self, sh: &Sheet, max_col: u32, out: &mut String) {
        let mut outline = Outline::new("column", sh.print.title_cols, MAX_COLS);
        let mut col = 0u32;
        let default_w = sh.default_col_width;
        while col < MAX_COLS {
            let info = sh.cols.get(&col).cloned().unwrap_or_default();
            outline.enter(out, col, info.outline, info.hidden);
            // A run of identical columns; past the sheet's extent, the rest are one run.
            let mut end = if col > max_col { MAX_COLS } else { col + 1 };
            while end <= max_col && sh.cols.get(&end).cloned().unwrap_or_default() == info && outline.continues(end) {
                end += 1;
            }
            let w = info.size.unwrap_or(default_w);
            let name = self.col_style(w);
            let _ = write!(out, "<table:table-column table:style-name=\"{name}\"");
            if end - col > 1 {
                let _ = write!(out, " table:number-columns-repeated=\"{}\"", end - col);
            }
            if info.hidden {
                out.push_str(" table:visibility=\"collapse\"");
            }
            let style = info.style.map(|s| self.cell_style(s)).unwrap_or_else(|| "Default".into());
            let _ = write!(out, " table:default-cell-style-name=\"{}\"/>", esc_attr(&style));
            outline.leave(out, col, end - col);
            col = end;
        }
        outline.close_to(out, 0);
    }

    fn rows(&mut self, sh: &Sheet, max_row: u32, max_col: u32, valid_at: &[(RangeRef, String)], out: &mut String) {
        let mut reach = Reach::new(&sh.merges, valid_at);
        // The columns of each row's cells, notes and links: the rows that can't repeat.
        let mut singles: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        let spilled = sh.spill.keys().copied().filter(|p| p.row < MAX_ROWS && p.col < MAX_COLS);
        for pos in sh.cells.iter().map(|(c, _)| c).chain(sh.comments.keys().copied()).chain(sh.hyperlinks.keys().copied()).chain(spilled) {
            singles.entry(pos.row).or_default().push(pos.col);
        }
        // The rows to repeat on each printed page (`table()` counts them in the extent).
        let mut outline = Outline::new("row", sh.print.title_rows, MAX_ROWS);
        let mut row = 0u32;
        let default_h = sh.default_row_height;
        let default_row = LineInfo::default();
        while row < MAX_ROWS {
            let info = *sh.rows.get(&row).unwrap_or(&default_row);
            if row > max_row {
                // The rest of the sheet: empty rows of the default height, in no outline group.
                outline.close_to(out, 0);
                let name = self.row_style(default_h, !sh.default_row_custom);
                let _ = write!(
                    out,
                    "<table:table-row table:style-name=\"{name}\" table:number-rows-repeated=\"{}\"><table:table-cell table:number-columns-repeated=\"{MAX_COLS}\"/></table:table-row>",
                    MAX_ROWS - row
                );
                break;
            }
            reach.at(row);
            outline.enter(out, row, info.outline, info.hidden);
            // Rows without cells, notes or links repeat while alike: the same line settings, and
            // the same merges and validations reaching them.
            let mut repeat = 1u32;
            if !singles.contains_key(&row) {
                let change = reach.next_change(row).min(singles.range(row + 1..).next().map_or(u32::MAX, |(r, _)| *r));
                while row + repeat <= max_row
                    && row + repeat < change
                    && sh.rows.get(&(row + repeat)).unwrap_or(&default_row) == &info
                    && outline.continues(row + repeat)
                {
                    repeat += 1;
                }
            }
            let height = info.size.unwrap_or(default_h);
            // A row of the default height is optimal unless the default was set by hand.
            let name = self.row_style(height, !(info.custom || info.size.is_none() && sh.default_row_custom));
            let _ = write!(out, "<table:table-row table:style-name=\"{name}\"");
            if repeat > 1 {
                let _ = write!(out, " table:number-rows-repeated=\"{repeat}\"");
            }
            if info.hidden {
                out.push_str(" table:visibility=\"collapse\"");
            }
            if let Some(s) = info.style {
                let st = self.cell_style(s);
                let _ = write!(out, " table:default-cell-style-name=\"{}\"", esc_attr(&st));
            }
            out.push('>');
            let cols = singles.get(&row).map(Vec::as_slice).unwrap_or_default();
            self.row_cells(sh, row, max_col, cols, &reach, out);
            out.push_str("</table:table-row>");
            outline.leave(out, row, repeat);
            row += repeat;
        }
        outline.close_to(out, 0);
    }

    /// One row's cells, as runs: each cell, note, link or merge's first cell alone, and between
    /// them runs of alike cells (empty, covered by a merge, or under a validation) repeated.
    fn row_cells(&mut self, sh: &Sheet, row: u32, max_col: u32, singles: &[u32], reach: &Reach<'_>, out: &mut String) {
        let end = max_col + 1;
        let mut edges: Vec<u32> = vec![0, end];
        for &c in singles {
            edges.extend([c, c.saturating_add(1)]);
        }
        // The ranges reaching the row by first column, swept along with the runs: each run
        // looks only at the ranges open at its first column.
        let mut by_start: Vec<&(RangeRef, Option<&str>)> = reach.active().collect();
        by_start.sort_by_key(|(r, _)| r.start.col);
        for (r, name) in &by_start {
            // A merge's first cell stands alone.
            if name.is_none() && r.start.row == row {
                edges.extend([r.start.col, r.start.col.saturating_add(1)]);
            }
            edges.extend([r.start.col, r.end.col.saturating_add(1)]);
        }
        edges.retain(|c| *c <= end);
        edges.sort_unstable();
        edges.dedup();
        let row_style = sh.rows.get(&row).and_then(|r| r.style);
        let (mut next, mut open): (usize, Vec<&(RangeRef, Option<&str>)>) = (0, Vec::new());
        for w in edges.windows(2) {
            let [a, b] = [w.first().copied().unwrap_or(0), w.get(1).copied().unwrap_or(end)];
            let pos = CellRef::new(row, a);
            while let Some(r) = by_start.get(next).filter(|(r, _)| r.start.col <= a) {
                open.push(r);
                next += 1;
            }
            open.retain(|(r, _)| r.end.col >= a);
            let mut covered = false;
            let mut anchor = None;
            let mut validation = None;
            for (r, name) in open.iter().copied().filter(|(r, _)| r.contains(pos)) {
                match name {
                    Some(n) => validation = Some(*n),
                    None if r.start == pos => anchor = Some((r.width(), r.height())),
                    None => covered = true,
                }
            }
            let cell = sh.cell(pos);
            let tag = if covered { "table:covered-table-cell" } else { "table:table-cell" };
            let _ = write!(out, "<{tag}");
            // A run takes its row's or column's default style; a cell states its own when it
            // isn't the default everywhere.
            let inherited = row_style.or_else(|| sh.cols.get(&a).and_then(|c| c.style)).unwrap_or(StyleId::DEFAULT);
            if let Some(style) = cell.map(|c| c.style).filter(|s| *s != StyleId::DEFAULT || inherited != StyleId::DEFAULT) {
                let name = self.cell_style(style);
                let _ = write!(out, " table:style-name=\"{}\"", esc_attr(&name));
            }
            if let Some(v) = validation {
                let _ = write!(out, " table:content-validation-name=\"{}\"", esc_attr(v));
            }
            if b - a > 1 {
                let _ = write!(out, " table:number-columns-repeated=\"{}\"", b - a);
            }
            if let Some((w, h)) = anchor {
                let _ = write!(out, " table:number-columns-spanned=\"{w}\" table:number-rows-spanned=\"{h}\"");
            }
            let mut inner = String::new();
            if let Some(c) = sh.comments.get(&pos) {
                let _ = write!(inner, "<office:annotation office:display=\"{}\">", c.visible);
                if !c.author.is_empty() {
                    let _ = write!(inner, "<dc:creator>{}</dc:creator>", esc(&c.author));
                }
                if let Some(d) = c.dates.first().filter(|d| !d.is_empty()) {
                    let _ = write!(inner, "<dc:date>{}</dc:date>", esc(d));
                }
                let mut text = c.text.clone();
                for (who, reply) in &c.replies {
                    let _ = write!(text, "\n{who}: {reply}");
                }
                paragraphs(&mut inner, &text, None);
                inner.push_str("</office:annotation>");
            }
            // A spilled value is cached in its cell of the matrix.
            let spilled = cell.is_none().then(|| sh.spill.get(&pos)).flatten().map(|v| Cell { value: v.clone(), formula: None, ..Cell::default() });
            if let Some(c) = cell.or(spilled.as_ref()) {
                self.cell_value(sh, pos, c, out, &mut inner);
            } else if let Some(l) = self.link(sh, pos) {
                paragraphs(&mut inner, "", Some(&l));
            }
            if inner.is_empty() {
                out.push_str("/>");
            } else {
                let _ = write!(out, ">{inner}</{tag}>");
            }
        }
        // A row's own style reaches across the sheet, past the cells written.
        if row_style.is_some() && end < MAX_COLS {
            let _ = write!(out, "<table:table-cell table:number-columns-repeated=\"{}\"/>", MAX_COLS - end);
        }
    }

    /// The `xlink:href` of the link at `pos`, if any.
    fn link(&self, sh: &Sheet, pos: CellRef) -> Option<String> {
        sh.hyperlinks.get(&pos).map(|h| {
            if h.external {
                // Relative to the file's folder in the model, to the package in ODF.
                if super::read::is_relative(&h.target) { format!("../{}", h.target) } else { h.target.clone() }
            } else {
                // `'Q1 Sales'!A1` → `#'Q1 Sales'.A1`.
                match h.target.rsplit_once('!') {
                    Some((s, a)) => format!("#{}.{}", formula::quote_sheet(&gridcraft_core::unquote_sheet(s)), a.replace('$', "")),
                    None => format!("#{}", h.target),
                }
            }
        })
    }

    fn cell_value(&mut self, sh: &Sheet, pos: CellRef, c: &Cell, out: &mut String, inner: &mut String) {
        if let Some(f) = &c.formula
            && let Some(e) = f.expr()
        {
            let text = formula::to_odf(&e, pos, Some(&sh.name), &WbTables(self.wb));
            let _ = write!(out, " table:formula=\"{}\"", esc_attr(&text));
            // A dynamic array's spill is written as the matrix it fills, as ODF has no spill.
            let matrix = f.array.or_else(|| sh.spill_ranges.get(&pos).copied().filter(|r| !r.is_single()));
            if let Some(a) = matrix {
                let _ = write!(out, " table:number-matrix-columns-spanned=\"{}\" table:number-matrix-rows-spanned=\"{}\"", a.width(), a.height());
            }
        }
        let fmt = self.wb.styles.get(c.style).num_fmt.as_str().to_string();
        // The paragraph is the value as shown: the caller's formatter, or plain number text.
        let shown = |v: &Value| -> String {
            match (self.display, v) {
                (Some(show), _) => show(sh, pos),
                (None, Value::Number(n)) => crate::xml::num(*n),
                (None, other) => other.display(),
            }
        };
        let link = self.link(sh, pos);
        match &c.value {
            Value::Number(n) => {
                let kind = numfmt::value_kind(&fmt);
                let sys = self.wb.date_system;
                let serial = *n;
                // The fictitious 1900-02-29 has no calendar date: it is written as its number.
                let calendar = datetime_from_serial(sys, serial).filter(|_| sys == DateSystem::D1904 || serial != 60.0);
                match (kind, calendar) {
                    (numfmt::ValueKind::Date, Some(d)) if d.day > 0 => {
                        let _ = write!(out, " office:value-type=\"date\" office:date-value=\"{:04}-{:02}-{:02}", d.year, d.month, d.day);
                        if serial.fract() != 0.0 {
                            let _ = write!(out, "T{:02}:{:02}:{:02}", d.hour, d.minute, d.second);
                            if d.milli > 0 {
                                let _ = write!(out, ".{:03}", d.milli);
                            }
                        }
                        out.push_str("\" calcext:value-type=\"date\"");
                    }
                    (numfmt::ValueKind::Time, _) if *n >= 0.0 => {
                        let _ = write!(out, " office:value-type=\"time\" office:time-value=\"{}\" calcext:value-type=\"time\"", duration(*n));
                    }
                    _ => {
                        let kind = if kind == numfmt::ValueKind::Percent { "percentage" } else { "float" };
                        let _ = write!(out, " office:value-type=\"{kind}\" office:value=\"{}\" calcext:value-type=\"{kind}\"", crate::xml::num(*n));
                    }
                }
                paragraphs(inner, &shown(&c.value), link.as_deref());
            }
            Value::Text(t) => {
                out.push_str(" office:value-type=\"string\" calcext:value-type=\"string\"");
                paragraphs(inner, t, link.as_deref());
            }
            Value::Bool(b) => {
                let _ = write!(out, " office:value-type=\"boolean\" office:boolean-value=\"{b}\" calcext:value-type=\"boolean\"");
                paragraphs(inner, if *b { "TRUE" } else { "FALSE" }, link.as_deref());
            }
            Value::Error(e) => {
                out.push_str(" office:value-type=\"string\" office:string-value=\"\" calcext:value-type=\"error\"");
                paragraphs(inner, e.as_str(), link.as_deref());
            }
            Value::Array(a) => {
                let first = a.get(0, 0).cloned().unwrap_or(Value::Empty);
                self.cell_value(sh, pos, &Cell { value: first, formula: None, ..c.clone() }, out, inner);
            }
            Value::Empty => {
                if let Some(l) = link {
                    paragraphs(inner, "", Some(&l));
                }
            }
        }
    }

    fn shapes(&mut self, sh: &Sheet, out: &mut String) {
        if sh.images.is_empty() && sh.charts.is_empty() && sh.shapes.is_empty() {
            return;
        }
        out.push_str("<table:shapes>");
        // One stacking order over shapes, charts and pictures, in the order they are drawn.
        let mut z = 0usize;
        let lines = sheet_lines(sh);
        for shp in &sh.shapes {
            let name = format!("shp{}", self.graphics.len() + 1);
            let (x, y) = anchor_pt(&lines, &shp.anchor);
            if let Some(xml) = super::shape::shape_xml(shp, &name, x, y, z) {
                z += 1;
                out.push_str(&xml);
                self.graphics.push(super::shape::style_xml(&name, shp, &self.wb.theme));
            }
        }
        for c in &sh.charts {
            let dir = format!("Object {}", self.objects.len() + 1);
            // Anchors are in pixels at 96 dpi; the file takes points.
            let (x, y) = anchor_pt(&lines, &c.anchor);
            let content = super::chart::write(c, &self.wb.theme);
            let object = if self.flat {
                // The chart's own document, inline.
                format!("<draw:object>{}</draw:object>", content.inline())
            } else {
                format!(r#"<draw:object xlink:href="./{}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/>"#, esc_attr(&dir))
            };
            let _ = write!(
                out,
                r#"<draw:frame draw:z-index="{z}" draw:name="Chart {}" svg:width="{}pt" svg:height="{}pt" svg:x="{x}pt" svg:y="{y}pt">{object}</draw:frame>"#,
                c.id,
                c.anchor.width * 0.75,
                c.anchor.height * 0.75,
            );
            self.objects.push((dir, content));
            z += 1;
        }
        for img in &sh.images {
            let ext = crate::drawing::image_ext(&img.mime, &img.data);
            let path = format!("Pictures/{}.{ext}", self.pictures.len() + 1);
            let (x, y) = anchor_pt(&lines, &img.anchor);
            let image = if self.flat {
                format!("<draw:image><office:binary-data>{}</office:binary-data></draw:image>", gridcraft_model::b64::encode(&img.data))
            } else {
                format!(r#"<draw:image xlink:href="{}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/>"#, esc_attr(&path))
            };
            let _ = write!(
                out,
                r#"<draw:frame draw:z-index="{z}" draw:name="Image {}" draw:style-name="gr1" svg:width="{}pt" svg:height="{}pt" svg:x="{x}pt" svg:y="{y}pt">{image}"#,
                img.id,
                img.anchor.width * 0.75,
                img.anchor.height * 0.75,
            );
            if !img.alt.is_empty() {
                let _ = write!(out, "<svg:title>{}</svg:title>", esc(&img.alt));
            }
            out.push_str("</draw:frame>");
            self.pictures.push((path, img.mime.clone(), img.data.clone()));
            z += 1;
        }
        out.push_str("</table:shapes>");
    }

    fn cf_style(&mut self, s: &Style) -> String {
        let name = format!("ConditionalStyle_{}", self.cf_styles.len() + 1);
        let xml = self.cell_style_xml(&name, s, &Style::default(), Some("Default"), true);
        self.cf_styles.push(xml);
        name
    }

    fn cond_formats(&mut self, sh: &Sheet, out: &mut String) {
        if sh.cond_formats.is_empty() {
            return;
        }
        let mut formats: Vec<&gridcraft_model::CondFormat> = sh.cond_formats.iter().collect();
        formats.sort_by_key(|c| c.priority);
        let mut o = String::from("<calcext:conditional-formats>");
        let tables = WbTables(self.wb);
        for cf in formats {
            let Some(first) = cf.ranges.first() else { continue };
            let base = first.start;
            let target: Vec<String> = cf.ranges.iter().map(|r| formula::range_address(&sh.name, *r)).collect();
            let base_addr = format!("{}.{}{}", formula::quote_sheet(&sh.name), col_to_letters(base.col), base.row + 1);
            let expr = |t: &str| formula::odf_expr(t, base, &sh.name, &tables);
            // A threshold as ODF writes it. The range's lowest and highest values are `minimum`
            // and `maximum`, except in an icon set, whose entries are all lower bounds: there the
            // lowest value is 0 percent.
            let entry = |k: &CfValueKind, icons: bool| -> (String, &'static str) {
                match k {
                    CfValueKind::Min if icons => ("0".into(), "percent"),
                    CfValueKind::Max if icons => ("100".into(), "percent"),
                    CfValueKind::Min => ("0".into(), "minimum"),
                    CfValueKind::Max => ("0".into(), "maximum"),
                    CfValueKind::AutoMin => ("0".into(), "auto-minimum"),
                    CfValueKind::AutoMax => ("0".into(), "auto-maximum"),
                    CfValueKind::Number(n) => (crate::xml::num(*n), "number"),
                    CfValueKind::Percent(n) => (crate::xml::num(*n), "percent"),
                    CfValueKind::Percentile(n) => (crate::xml::num(*n), "percentile"),
                    CfValueKind::Formula(f) => (expr(f), "formula"),
                }
            };
            let hex = |c: &Color| c.resolve(&self.wb.theme).map(|[r, g, b]| format!("#{r:02x}{g:02x}{b:02x}")).unwrap_or_else(|| "#000000".into());
            let rule = match &cf.rule {
                CfRule::ColorScale { stops } => {
                    let mut s = String::from("<calcext:color-scale>");
                    for (k, c) in stops {
                        let (v, t) = entry(k, false);
                        let _ = write!(
                            s,
                            r#"<calcext:color-scale-entry calcext:value="{}" calcext:type="{t}" calcext:color="{}"/>"#,
                            esc_attr(&v),
                            hex(c)
                        );
                    }
                    s.push_str("</calcext:color-scale>");
                    s
                }
                CfRule::DataBar { min, max, color, gradient, show_value } => {
                    let (a, ta) = entry(min, false);
                    let (b, tb) = entry(max, false);
                    format!(
                        r##"<calcext:data-bar calcext:positive-color="{}" calcext:negative-color="#ff0000" calcext:gradient="{gradient}" calcext:show-value="{show_value}" calcext:axis-color="#000000"><calcext:formatting-entry calcext:value="{}" calcext:type="{ta}"/><calcext:formatting-entry calcext:value="{}" calcext:type="{tb}"/></calcext:data-bar>"##,
                        hex(color),
                        esc_attr(&a),
                        esc_attr(&b)
                    )
                }
                CfRule::IconSet { set, thresholds, show_value, .. } => {
                    let mut s = format!(r#"<calcext:icon-set calcext:icon-set-type="{}" calcext:show-value="{show_value}">"#, esc_attr(set));
                    for k in thresholds {
                        let (v, t) = entry(k, true);
                        let _ = write!(s, r#"<calcext:formatting-entry calcext:value="{}" calcext:type="{t}"/>"#, esc_attr(&v));
                    }
                    s.push_str("</calcext:icon-set>");
                    s
                }
                CfRule::TimePeriod { period, style } => {
                    let date = match period.as_str() {
                        "last7Days" => "last-7-days",
                        "thisWeek" => "this-week",
                        "lastWeek" => "last-week",
                        "nextWeek" => "next-week",
                        "thisMonth" => "this-month",
                        "lastMonth" => "last-month",
                        "nextMonth" => "next-month",
                        p => p,
                    };
                    let name = self.cf_style(style);
                    format!(r#"<calcext:date-is calcext:date="{}" calcext:style="{name}"/>"#, esc_attr(date))
                }
                other => {
                    let Some((value, style)) = condition_value(other, &expr) else { continue };
                    let name = self.cf_style(style);
                    format!(
                        r#"<calcext:condition calcext:apply-style-name="{name}" calcext:value="{}" calcext:base-cell-address="{}"/>"#,
                        esc_attr(&value),
                        esc_attr(&base_addr)
                    )
                }
            };
            let _ = write!(
                o,
                r#"<calcext:conditional-format calcext:target-range-address="{}">{rule}</calcext:conditional-format>"#,
                esc_attr(&target.join(" "))
            );
        }
        o.push_str("</calcext:conditional-formats>");
        out.push_str(&o);
    }

    /// The styles part's pieces but its data styles: the default and named styles (the
    /// children of `office:styles`), page layouts (automatic styles) and master pages.
    fn styles(&mut self) -> (String, String, String) {
        let default = self.wb.styles.get(StyleId::DEFAULT).clone();
        let base = baseline();
        let mut o = String::new();
        let mut tp = String::new();
        style::write_font(&mut tp, &default.font, &base.font, &self.wb.theme);
        let mut decls = vec![];
        style::write_text_defaults(&mut tp, &self.wb.text_defaults, |f| decls.push(f.to_string()));
        for f in decls {
            self.font(&f);
        }
        let _ = write!(o, "<style:default-style style:family=\"table-cell\"><style:text-properties{tp}/></style:default-style>");
        // The font is the default style's; the Default style states the rest.
        let mut no_font = default.clone();
        no_font.font = base.font.clone();
        o.push_str(&self.cell_style_xml("Default", &no_font, &base, None, true));
        for s in &self.cf_styles {
            o.push_str(s);
        }
        // The arrowhead of arrow shapes: a plain triangle.
        o.push_str(r#"<draw:marker draw:name="Arrow" svg:viewBox="0 0 10 10" svg:d="M5 0 L10 10 L0 10 Z"/>"#);
        let mut layouts = String::new();
        let mut masters = String::new();
        for (i, sh) in self.wb.sheets.iter().enumerate() {
            let (layout, master) = page::write(i, &sh.print);
            layouts.push_str(&layout);
            masters.push_str(&master);
        }
        (o, layouts, format!("<office:master-styles>{masters}</office:master-styles>"))
    }
}

/// A password as `table:protection-key` attributes: only one with an OpenDocument key has a form
/// here (Excel's salted hashes can't be turned into one), so others protect without a password.
fn protection_key(p: Option<&gridcraft_model::PasswordHash>) -> String {
    match p.and_then(|p| p.odf_key()) {
        Some(hash) => {
            format!(" table:protection-key=\"{}\" table:protection-key-digest-algorithm=\"http://www.w3.org/2000/09/xmldsig#sha256\"", esc_attr(hash))
        }
        None => String::new(),
    }
}

/// An ISO 8601 duration (`PT12H30M00S`) for a day fraction.
fn duration(days: f64) -> String {
    let total_ms = (days * 86_400_000.0).round() as i64;
    let h = total_ms / 3_600_000;
    let m = (total_ms / 60_000) % 60;
    let s = (total_ms / 1000) % 60;
    let ms = total_ms % 1000;
    if ms > 0 { format!("PT{h:02}H{m:02}M{s:02}.{ms:03}S") } else { format!("PT{h:02}H{m:02}M{s:02}S") }
}

/// Text as `text:p` paragraphs with ODF's whitespace elements, optionally as one link.
pub(crate) fn paragraphs(out: &mut String, text: &str, link: Option<&str>) {
    // A line ends at a line feed, a carriage return, or both together.
    let text: std::borrow::Cow<'_, str> = if text.contains('\r') { text.replace("\r\n", "\n").into() } else { text.into() };
    for line in text.split(['\n', '\r']) {
        out.push_str("<text:p>");
        if let Some(l) = link {
            let _ = write!(out, "<text:a xlink:href=\"{}\" xlink:type=\"simple\">", esc_attr(l));
        }
        push_text(out, line);
        if link.is_some() {
            out.push_str("</text:a>");
        }
        out.push_str("</text:p>");
    }
}

/// Text inside a paragraph, its spaces and tabs written so XML whitespace rules keep them
/// (ODF 1.3 part 3 §6.1.2).
pub(crate) fn push_text(out: &mut String, line: &str) {
    let mut spaces = 0usize;
    let mut first = true;
    let flush = |out: &mut String, spaces: &mut usize, first: bool| {
        if *spaces == 0 {
            return;
        }
        // One ordinary space survives XML whitespace collapsing unless it starts the line.
        if first {
            let _ = write!(out, "<text:s text:c=\"{spaces}\"/>");
        } else {
            out.push(' ');
            if *spaces > 1 {
                let _ = write!(out, "<text:s text:c=\"{}\"/>", *spaces - 1);
            }
        }
        *spaces = 0;
    };
    for ch in line.chars() {
        match ch {
            ' ' => spaces += 1,
            '\t' => {
                flush(out, &mut spaces, first);
                out.push_str("<text:tab/>");
                first = false;
            }
            '\r' => {}
            c => {
                flush(out, &mut spaces, first);
                first = false;
                crate::xml::push_esc(out, c.encode_utf8(&mut [0u8; 4]));
            }
        }
    }
    // Trailing spaces would collapse: write them explicitly.
    if spaces > 0 {
        let _ = write!(out, "<text:s text:c=\"{spaces}\"/>");
    }
}

fn op_text(op: CfOperator) -> &'static str {
    match op {
        CfOperator::Equal => "=",
        CfOperator::NotEqual => "!=",
        CfOperator::Greater => ">",
        CfOperator::Less => "<",
        CfOperator::GreaterOrEqual => ">=",
        CfOperator::LessOrEqual => "<=",
        CfOperator::Between | CfOperator::NotBetween => "=",
    }
}

/// A `calcext:condition` value and its style.
fn condition_value<'r>(rule: &'r CfRule, expr: &dyn Fn(&str) -> String) -> Option<(String, &'r Style)> {
    let quote_text = |t: &str| format!("\"{}\"", t.replace('"', "\"\""));
    Some(match rule {
        CfRule::CellIs { op: CfOperator::Between, a, b, style } => {
            (format!("between({},{})", expr(a), b.as_deref().map(expr).unwrap_or_default()), style)
        }
        CfRule::CellIs { op: CfOperator::NotBetween, a, b, style } => {
            (format!("not-between({},{})", expr(a), b.as_deref().map(expr).unwrap_or_default()), style)
        }
        CfRule::CellIs { op, a, style, .. } => (format!("{}{}", op_text(*op), expr(a)), style),
        CfRule::Expression { formula, style } => (format!("formula-is({})", expr(formula)), style),
        CfRule::ContainsText { text, style } => (format!("contains-text({})", quote_text(text)), style),
        CfRule::NotContainsText { text, style } => (format!("not-contains-text({})", quote_text(text)), style),
        CfRule::BeginsWith { text, style } => (format!("begins-with({})", quote_text(text)), style),
        CfRule::EndsWith { text, style } => (format!("ends-with({})", quote_text(text)), style),
        CfRule::Blanks { style } => ("is-empty".into(), style),
        CfRule::NoBlanks { style } => ("is-not-empty".into(), style),
        CfRule::Errors { style } => ("is-error".into(), style),
        CfRule::NoErrors { style } => ("is-no-error".into(), style),
        CfRule::Duplicate { style } => ("duplicate".into(), style),
        CfRule::Unique { style } => ("unique".into(), style),
        CfRule::Top10 { bottom, percent, rank, style } => {
            let kind = match (bottom, percent) {
                (false, false) => "top-elements",
                (true, false) => "bottom-elements",
                (false, true) => "top-percent",
                (true, true) => "bottom-percent",
            };
            (format!("{kind}({rank})"), style)
        }
        CfRule::AboveAverage { below, equal, style, .. } => {
            let v = match (below, equal) {
                (false, false) => "above-average",
                (true, false) => "below-average",
                (false, true) => "above-equal-average",
                (true, true) => "below-equal-average",
            };
            (v.into(), style)
        }
        _ => return None,
    })
}

fn validation_xml(name: &str, v: &Validation, sheet: &str, base: CellRef, wb: &Workbook) -> String {
    let tables = WbTables(wb);
    let expr = |t: &str| formula::odf_expr(t, base, sheet, &tables);
    let kind = match v.kind {
        ValidationKind::Whole => "cell-content-is-whole-number() and ",
        ValidationKind::Decimal => "cell-content-is-decimal-number() and ",
        ValidationKind::Date => "cell-content-is-date() and ",
        ValidationKind::Time => "cell-content-is-time() and ",
        _ => "",
    };
    let cmp = |subject: &str| -> String {
        match v.op {
            CfOperator::Between => format!("{subject}-is-between({};{})", expr(&v.f1), v.f2.as_deref().map(expr).unwrap_or_default()),
            CfOperator::NotBetween => format!("{subject}-is-not-between({};{})", expr(&v.f1), v.f2.as_deref().map(expr).unwrap_or_default()),
            op => format!("{subject}(){}{}", op_text(op), expr(&v.f1)),
        }
    };
    let condition = match v.kind {
        ValidationKind::Any => String::new(),
        ValidationKind::List => {
            let f = v.f1.trim();
            if let Some(list) = f.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
                let items: Vec<String> = list.split(',').map(|i| format!("\"{}\"", i.trim().replace('"', "\"\""))).collect();
                format!("of:cell-content-is-in-list({})", items.join(";"))
            } else {
                format!("of:cell-content-is-in-list({})", expr(f))
            }
        }
        ValidationKind::Custom => format!("of:is-true-formula({})", expr(&v.f1)),
        ValidationKind::TextLength => format!("of:{}", cmp("cell-content-text-length")),
        _ => format!("of:{kind}{}", cmp("cell-content")),
    };
    let mut o = format!(r#"<table:content-validation table:name="{name}""#);
    if !condition.is_empty() {
        let _ = write!(o, " table:condition=\"{}\"", esc_attr(&condition));
    }
    let _ = write!(
        o,
        r#" table:allow-empty-cell="{}" table:display-list="{}" table:base-cell-address="{}.{}{}">"#,
        v.allow_blank,
        if v.kind == ValidationKind::List && !v.in_cell_dropdown { "none" } else { "unsorted" },
        esc_attr(&formula::quote_sheet(sheet)),
        col_to_letters(base.col),
        base.row + 1
    );
    let _ = write!(o, r#"<table:help-message table:title="{}" table:display="{}">"#, esc_attr(&v.input_title), v.show_input);
    paragraphs(&mut o, &v.input_message, None);
    o.push_str("</table:help-message>");
    let kind = match v.error_style {
        ErrorStyle::Stop => "stop",
        ErrorStyle::Warning => "warning",
        ErrorStyle::Information => "information",
    };
    let _ =
        write!(o, r#"<table:error-message table:message-type="{kind}" table:title="{}" table:display="{}">"#, esc_attr(&v.error_title), v.show_error);
    paragraphs(&mut o, &v.error_message, None);
    o.push_str("</table:error-message></table:content-validation>");
    o
}

/// Whether a filter value compared by `operator` is a number.
fn compared_as_number(value: &str, operator: &str) -> bool {
    matches!(operator, "<" | ">" | "<=" | ">=" | "=" | "!=") && value.trim().parse::<f64>().is_ok_and(f64::is_finite)
}

fn filter_condition(o: &mut String, col: u32, crit: &gridcraft_model::FilterCriterion) {
    use gridcraft_model::FilterCriterion as F;
    match crit {
        F::Values { values, blanks: true } if values.is_empty() => {
            let _ = write!(o, r#"<table:filter-condition table:field-number="{col}" table:value="" table:operator="empty"/>"#);
        }
        // A value list always lists its items (one too): a bare `=` is a custom comparison.
        F::Values { values, blanks } => {
            let first = values.first().map(String::as_str).unwrap_or_default();
            let _ = write!(o, r#"<table:filter-condition table:field-number="{col}" table:value="{}" table:operator="=">"#, esc_attr(first));
            for v in values {
                let _ = write!(o, r#"<table:filter-set-item table:value="{}"/>"#, esc_attr(v));
            }
            // The "(Blanks)" choice is an empty item.
            if *blanks {
                o.push_str(r#"<table:filter-set-item table:value=""/>"#);
            }
            o.push_str("</table:filter-condition>");
        }
        F::Custom { a, b, and } => {
            // The filter is an AND of its columns: a column's two conditions joined by OR nest.
            let or = !and && b.is_some();
            if or {
                o.push_str("<table:filter-or>");
            }
            for (op, v) in std::iter::once(a).chain(b.iter()) {
                // A wildcard at either end is one of ODF's text operators.
                let wildcard = super::read::WILDCARD_OPS.iter().find_map(|(odf, x, before, after)| {
                    let inner = v.strip_prefix('*').filter(|_| *before).unwrap_or(v);
                    let inner = inner.strip_suffix('*').filter(|_| *after).unwrap_or(inner);
                    let shape = (v.starts_with('*'), v.ends_with('*') && v.len() > 1) == (*before, *after);
                    (op == x && shape && !inner.is_empty() && !inner.contains(['*', '?'])).then(|| (*odf, inner.to_string()))
                });
                let (operator, value) = match wildcard {
                    Some(w) => w,
                    None if op == "<>" => ("!=", v.clone()),
                    None => (op.as_str(), v.clone()),
                };
                // A number is compared as one (ODF compares text by default).
                let number = compared_as_number(&value, operator);
                let _ = write!(
                    o,
                    r#"<table:filter-condition table:field-number="{col}" table:value="{}"{} table:operator="{}"/>"#,
                    esc_attr(&value),
                    if number { r#" table:data-type="number""# } else { "" },
                    esc_attr(operator)
                );
            }
            if or {
                o.push_str("</table:filter-or>");
            }
        }
        F::Top10 { bottom, percent, count } => {
            let op = match (bottom, percent) {
                (false, false) => "top values",
                (true, false) => "bottom values",
                (false, true) => "top percent",
                (true, true) => "bottom percent",
            };
            let _ = write!(
                o,
                r#"<table:filter-condition table:field-number="{col}" table:value="{count}" table:data-type="number" table:operator="{op}"/>"#
            );
        }
        _ => {}
    }
}
