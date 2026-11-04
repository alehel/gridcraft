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
use super::read::baseline;
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
    let content = w.content();
    let styles = w.styles();
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
    zip.add("META-INF/manifest.xml", manifest.as_bytes())?;
    zip.add("content.xml", content.as_bytes())?;
    zip.add("styles.xml", styles.as_bytes())?;
    zip.add("meta.xml", meta(wb).as_bytes())?;
    zip.add("settings.xml", settings::write(wb).as_bytes())?;
    for (path, _, data) in &w.pictures {
        zip.add(path, data)?;
    }
    for (dir, content) in &w.objects {
        zip.add(&format!("{dir}/content.xml"), content.as_bytes())?;
    }
    zip.finish()
}

/// The children of the root element of one of this writer's documents.
fn inner<'s>(doc: &'s str, root: &str) -> &'s str {
    let Some(tag) = doc.find(&format!("<office:{root} ")) else { return "" };
    let Some(open) = doc.get(tag..).and_then(|d| d.find('>')).map(|j| tag + j + 1) else { return "" };
    let end = doc.rfind(&format!("</office:{root}>")).unwrap_or(doc.len());
    doc.get(open..end).unwrap_or("")
}

/// Encodes `wb` as a flat OpenDocument spreadsheet (`.fods`): one XML document holding what a
/// package splits into parts, pictures as base64 and charts as inline documents.
pub fn write_fods(wb: &Workbook, display: Option<Display<'_>>) -> Result<Vec<u8>, IoError> {
    let mut w = Writer::new(wb, display);
    w.flat = true;
    let content = w.content();
    let styles = w.styles();
    let content = inner(&content, "document-content");
    let styles = inner(&styles, "document-styles");
    let meta = meta(wb);
    let settings = settings::write(wb);
    // A part of one of the documents. Only the body holds documents of its own (inline charts,
    // with their own parts), so it ends at the last closing tag; every other part at the first.
    let section = |doc: &str, tag: &str| -> String {
        let open = format!("<office:{tag}>");
        let close = format!("</office:{tag}>");
        let Some(a) = doc.find(&open) else { return String::new() };
        let end = if tag == "body" { doc.rfind(&close) } else { doc.get(a..).and_then(|d| d.find(&close)).map(|b| a + b) };
        match end {
            Some(b) if a < b => doc.get(a..b + close.len()).unwrap_or("").to_string(),
            _ => String::new(),
        }
    };
    let mut o = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><office:document {NS} office:version="1.3" office:mimetype="application/vnd.oasis.opendocument.spreadsheet">"#
    );
    o.push_str(&section(&meta, "meta"));
    o.push_str(&section(&settings, "settings"));
    o.push_str(&section(styles, "font-face-decls"));
    o.push_str(&section(styles, "styles"));
    // Both parts' automatic styles (cells and pages) in one list.
    o.push_str("<office:automatic-styles>");
    let auto = |doc: &str| {
        section(doc, "automatic-styles").trim_start_matches("<office:automatic-styles>").trim_end_matches("</office:automatic-styles>").to_string()
    };
    o.push_str(&auto(content));
    o.push_str(&auto(styles));
    o.push_str("</office:automatic-styles>");
    o.push_str(&section(styles, "master-styles"));
    o.push_str(&section(content, "body"));
    o.push_str("</office:document>");
    Ok(o.into_bytes())
}

fn meta(wb: &Workbook) -> String {
    let mut o = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><office:document-meta {NS} office:version="1.3"><office:meta><meta:generator>GridCraft</meta:generator>"#
    );
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
    o.push_str("</office:meta></office:document-meta>");
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
    /// Data styles: format code → name.
    data_styles: HashMap<String, String>,
    data_xml: String,
    col_styles: Vec<(u32, String)>,
    row_styles: Vec<(u32, bool, String)>,
    /// Named conditional-format styles (`ConditionalStyle_N`) and their properties.
    cf_styles: Vec<String>,
    fonts: Vec<String>,
    pictures: Vec<(String, String, std::sync::Arc<Vec<u8>>)>,
    /// Embedded chart objects: folder (`Object 1`) and its `content.xml`.
    objects: Vec<(String, String)>,
    /// Graphic styles of shapes.
    graphics: Vec<String>,
    validations: Vec<(String, String)>,
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
            data_xml: String::new(),
            col_styles: Vec::new(),
            row_styles: Vec::new(),
            cf_styles: Vec::new(),
            fonts: Vec::new(),
            pictures: Vec::new(),
            objects: Vec::new(),
            graphics: Vec::new(),
            validations: Vec::new(),
        }
    }

    fn font(&mut self, name: &str) {
        if !self.fonts.iter().any(|f| f == name) {
            self.fonts.push(name.to_string());
        }
    }

    fn data_style(&mut self, code: &str) -> Option<String> {
        if let Some(n) = self.data_styles.get(code) {
            return Some(n.clone());
        }
        let name = format!("N{}", self.data_styles.len() + 100);
        let xml = numfmt::write_style(&name, code)?;
        self.data_xml.push_str(&xml);
        self.data_styles.insert(code.to_string(), name.clone());
        Some(name)
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
        if let Some((_, n)) = self.col_styles.iter().find(|(k, _)| *k == key) {
            return n.clone();
        }
        let name = format!("co{}", self.col_styles.len() + 1);
        self.col_styles.push((key, name.clone()));
        name
    }

    fn row_style(&mut self, height: f32, optimal: bool) -> String {
        let key = (height * 100.0).round() as u32;
        if let Some((_, _, n)) = self.row_styles.iter().find(|(k, o, _)| *k == key && *o == optimal) {
            return n.clone();
        }
        let name = format!("ro{}", self.row_styles.len() + 1);
        self.row_styles.push((key, optimal, name.clone()));
        name
    }

    fn automatic_styles(&mut self) -> String {
        let mut o = String::from("<office:automatic-styles>");
        for (key, name) in &self.col_styles {
            let _ = write!(
                o,
                r#"<style:style style:name="{name}" style:family="table-column"><style:table-column-properties fo:break-before="auto" style:column-width="{}"/></style:style>"#,
                px_to_in(*key as f32 / 100.0)
            );
        }
        for (key, optimal, name) in &self.row_styles {
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
        let mut cells = String::new();
        for (id, name) in ids {
            let s = self.wb.styles.get(id).clone();
            if s.font.name != default.font.name {
                self.font(&s.font.name);
            }
            let data = if s.num_fmt != default.num_fmt { self.data_style(s.num_fmt.as_str()) } else { None };
            let data_attr = data.map(|d| format!(" style:data-style-name=\"{d}\"")).unwrap_or_default();
            let props = style::write_props(&s, &default, &self.wb.theme);
            let _ = write!(
                cells,
                r#"<style:style style:name="{name}" style:family="table-cell" style:parent-style-name="Default"{data_attr}>{props}</style:style>"#
            );
        }
        o.push_str(&self.data_xml);
        o.push_str(&cells);
        o.push_str("<style:style style:name=\"gr1\" style:family=\"graphic\"><style:graphic-properties draw:stroke=\"none\" draw:fill=\"none\"/></style:style>");
        for g in &self.graphics {
            o.push_str(g);
        }
        o.push_str("</office:automatic-styles>");
        o
    }

    fn content(&mut self) -> String {
        let mut body = String::from("<office:body><office:spreadsheet");
        if self.wb.protected_structure {
            body.push_str(" table:structure-protected=\"true\"");
            body.push_str(&protection_key(self.wb.structure_password.as_ref()));
        }
        body.push('>');
        let calc = &self.wb.calc;
        let d1904 = self.wb.date_system == DateSystem::D1904;
        if calc.iterative || calc.precision_as_displayed || d1904 {
            let _ = write!(body, "<table:calculation-settings table:precision-as-shown=\"{}\">", calc.precision_as_displayed);
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
        }
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
        let auto = self.automatic_styles();
        let mut o = format!(r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content {NS} office:version="1.3">"#);
        o.push_str(&self.font_decls());
        o.push_str(&auto);
        o.push_str(&body);
        o.push_str("</office:document-content>");
        o
    }

    fn font_decls(&mut self) -> String {
        let default = self.wb.styles.get(StyleId::DEFAULT).font.name.clone();
        self.font(&default);
        let mut o = String::from("<office:font-face-decls>");
        for f in &self.fonts {
            let _ = write!(o, r#"<style:font-face style:name="{0}" svg:font-family="'{0}'"/>"#, esc_attr(f));
        }
        o.push_str("</office:font-face-decls>");
        o
    }

    /// Named ranges and expressions of one sheet (`Some(i)`) or the workbook (`None`).
    fn names(&self, scope: Option<usize>) -> String {
        let mut o = String::new();
        let base_sheet = scope.and_then(|i| self.wb.sheets.get(i)).or_else(|| self.wb.sheets.first()).map(|s| s.name.clone()).unwrap_or_default();
        let base = format!("${}.$A$1", quote(&base_sheet));
        // ODF has no hidden names: a hidden name is written as a plain one rather than lost, so
        // formulas that use it keep working.
        for n in self.wb.names.iter().filter(|n| n.scope == scope && !n.name.starts_with("_xlnm")) {
            let Ok(e) = gridcraft_formula::parse(&n.formula) else { continue };
            if gridcraft_formula::parse_name(&n.name).is_err() {
                continue;
            }
            match &e {
                gridcraft_formula::Expr::Ref(r) => {
                    let mut t = String::new();
                    formula::write_ref(&mut t, r);
                    let addr = t.trim_start_matches('[').trim_end_matches(']').to_string();
                    let _ = write!(
                        o,
                        r#"<table:named-range table:name="{}" table:base-cell-address="{}" table:cell-range-address="{}"/>"#,
                        esc_attr(&n.name),
                        esc_attr(&base),
                        esc_attr(&addr)
                    );
                }
                _ => {
                    let text = formula::to_odf(&e, CellRef::new(0, 0), &WbTables(self.wb));
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
        for (i, sh) in self.wb.sheets.iter().enumerate() {
            let Some(f) = &sh.autofilter else { continue };
            let _ = write!(
                o,
                r#"<table:database-range table:name="__Anonymous_Sheet_DB__{i}" table:target-range-address="{}" table:display-filter-buttons="true">"#,
                esc_attr(&range_address(&sh.name, f.range))
            );
            if !f.criteria.is_empty() {
                o.push_str("<table:filter><table:filter-and>");
                for (col, crit) in &f.criteria {
                    filter_condition(&mut o, *col, crit);
                }
                o.push_str("</table:filter-and></table:filter>");
            }
            o.push_str("</table:database-range>");
        }
        if o.is_empty() { o } else { format!("<table:database-ranges>{o}</table:database-ranges>") }
    }

    fn table(&mut self, i: usize, out: &mut String) {
        let Some(sh) = self.wb.sheets.get(i) else { return };
        let sh: &Sheet = sh;
        let mut attrs = format!(" table:name=\"{}\" table:style-name=\"ta{i}\"", esc_attr(&sh.name));
        if let Some(area) = sh.print.print_area {
            let _ = write!(attrs, " table:print-ranges=\"{}\"", esc_attr(&range_address(&sh.name, area)));
        }
        if let Some(p) = &sh.protection {
            attrs.push_str(" table:protected=\"true\"");
            attrs.push_str(&protection_key(p.password.as_ref()));
        }
        let _ = write!(out, "<table:table{attrs}>");
        if sh.protection.is_some() {
            let p = sh.protection.as_ref();
            let _ = write!(
                out,
                "<loext:table-protection loext:select-protected-cells=\"{}\" loext:select-unprotected-cells=\"{}\"/>",
                p.is_none_or(|p| p.select_locked),
                p.is_none_or(|p| p.select_unlocked)
            );
        }
        self.shapes(i, sh, out);
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
        for c in sh.hyperlinks.keys() {
            max_row = max_row.max(c.row);
            max_col = max_col.max(c.col);
        }
        if let Some((&r, _)) = sh.rows.iter().next_back() {
            max_row = max_row.max(r);
        }
        if let Some((&c, _)) = sh.cols.iter().next_back() {
            max_col = max_col.max(c);
        }
        // Rows to repeat on each printed page are rows like any other: they're written out.
        if let Some((a, b)) = sh.print.title_rows {
            max_row = max_row.max(a.max(b).min(MAX_ROWS - 1));
        }
        self.columns(sh, max_col, out);
        // Validations: one per rule, named on the cells it covers.
        let mut valid_at: Vec<(RangeRef, String)> = Vec::new();
        for v in &sh.validations {
            let name = format!("val{}", self.validations.len() + 1);
            let first = v.ranges.first().map(|r| r.start).unwrap_or_default();
            self.validations.push((name.clone(), validation_xml(&name, v, &sh.name, first, self.wb)));
            for r in &v.ranges {
                // Wide rules stop a thousand columns past the data: each column is its own cell.
                let r = RangeRef::new(
                    r.start,
                    CellRef::new(r.end.row.min(MAX_ROWS - 1), r.end.col.min(MAX_COLS - 1).min(max_col.max(r.start.col) + 1_000)),
                );
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
        let mut level = 0u8;
        let mut col = 0u32;
        let default_w = sh.default_col_width;
        while col < MAX_COLS {
            let info = sh.cols.get(&col).cloned().unwrap_or_default();
            // A run of identical columns.
            let mut end = col + 1;
            let limit = if col > max_col { MAX_COLS } else { max_col + 1 };
            while end < limit && sh.cols.get(&end).cloned().unwrap_or_default() == info {
                end += 1;
            }
            if col > max_col {
                end = MAX_COLS;
            }
            while level < info.outline.min(7) {
                level += 1;
                let hidden = info.hidden && info.outline >= level;
                let _ = write!(out, "<table:table-column-group{}>", if hidden { " table:display=\"false\"" } else { "" });
            }
            while level > info.outline.min(7) {
                level -= 1;
                out.push_str("</table:table-column-group>");
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
            col = end;
        }
        while level > 0 {
            level -= 1;
            out.push_str("</table:table-column-group>");
        }
    }

    fn rows(&mut self, sh: &Sheet, max_row: u32, max_col: u32, valid_at: &[(RangeRef, String)], out: &mut String) {
        // Covered cells of merges, and the spans of their first cells.
        let mut spans: HashMap<CellRef, (u32, u32)> = HashMap::new();
        let mut covered: std::collections::HashSet<CellRef> = std::collections::HashSet::new();
        for m in &sh.merges {
            spans.insert(m.start, (m.width(), m.height()));
            if m.count() <= 1_000_000 {
                for r in m.start.row..=m.end.row {
                    for c in m.start.col..=m.end.col {
                        if (r, c) != (m.start.row, m.start.col) {
                            covered.insert(CellRef::new(r, c));
                        }
                    }
                }
            }
        }
        let covered_rows: std::collections::HashSet<u32> = covered.iter().map(|c| c.row).chain(spans.keys().map(|c| c.row)).collect();
        let note_rows: std::collections::HashSet<u32> = sh.comments.keys().chain(sh.hyperlinks.keys()).map(|c| c.row).collect();
        // The rows to repeat on each printed page (`table()` counts them in the extent).
        let header = sh.print.title_rows.map(|(a, b)| (a.min(b), a.max(b).min(max_row)));
        let mut level = 0u8;
        let mut row = 0u32;
        let default_h = sh.default_row_height;
        let default_row = LineInfo::default();
        while row < MAX_ROWS {
            let info = *sh.rows.get(&row).unwrap_or(&default_row);
            if row > max_row {
                // The rest of the sheet: empty rows of the default height, in no outline group.
                while level > 0 {
                    level -= 1;
                    out.push_str("</table:table-row-group>");
                }
                let name = self.row_style(default_h, true);
                let _ = write!(
                    out,
                    "<table:table-row table:style-name=\"{name}\" table:number-rows-repeated=\"{}\"><table:table-cell table:number-columns-repeated=\"{MAX_COLS}\"/></table:table-row>",
                    MAX_ROWS - row
                );
                break;
            }
            // A header block holds rows only, never a row group's edge: inside it the outline
            // stays at its first row's level.
            let in_header = header.is_some_and(|(a, b)| row > a && row <= b);
            let target = if in_header { level } else { info.outline.min(7) };
            while level < target {
                level += 1;
                let _ = write!(out, "<table:table-row-group{}>", if info.hidden { " table:display=\"false\"" } else { "" });
            }
            while level > target {
                level -= 1;
                out.push_str("</table:table-row-group>");
            }
            if header.is_some_and(|(a, _)| a == row) {
                out.push_str("<table:table-header-rows>");
            }
            // Rows without cells, merges or validation repeat while identical.
            let mut repeat = 1u32;
            // Rows that differ only in their validation's place are not alike.
            let validations = |r: u32| -> Vec<usize> {
                valid_at.iter().enumerate().filter(|(_, (v, _))| v.start.row <= r && v.end.row >= r).map(|(i, _)| i).collect()
            };
            let empty = |r: u32| sh.cells.row(r, 0, MAX_COLS - 1).next().is_none() && !covered_rows.contains(&r) && !note_rows.contains(&r);
            if empty(row) {
                let here = validations(row);
                while row + repeat <= max_row
                    && sh.rows.get(&(row + repeat)).unwrap_or(&default_row) == &info
                    && empty(row + repeat)
                    && validations(row + repeat) == here
                    && header.is_none_or(|(a, b)| row + repeat != a && row + repeat - 1 != b)
                {
                    repeat += 1;
                }
            }
            let height = info.size.unwrap_or(default_h);
            let name = self.row_style(height, !info.custom);
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
            self.row_cells(sh, row, max_col, &spans, &covered, valid_at, out);
            out.push_str("</table:table-row>");
            if header.is_some_and(|(_, b)| b >= row && b < row + repeat) {
                out.push_str("</table:table-header-rows>");
            }
            row += repeat;
        }
        while level > 0 {
            level -= 1;
            out.push_str("</table:table-row-group>");
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn row_cells(
        &mut self,
        sh: &Sheet,
        row: u32,
        max_col: u32,
        spans: &HashMap<CellRef, (u32, u32)>,
        covered: &std::collections::HashSet<CellRef>,
        valid_at: &[(RangeRef, String)],
        out: &mut String,
    ) {
        let mut col = 0u32;
        let row_style = sh.rows.get(&row).and_then(|r| r.style);
        let cells: BTreeMap<u32, &Cell> = sh.cells.row(row, 0, MAX_COLS - 1).collect();
        let validation = |c: u32| valid_at.iter().find(|(r, _)| r.contains(CellRef::new(row, c))).map(|(_, n)| n.clone());
        while col <= max_col {
            let pos = CellRef::new(row, col);
            let cell = cells.get(&col).copied();
            let is_covered = covered.contains(&pos);
            let special = cell.is_some()
                || is_covered
                || spans.contains_key(&pos)
                || sh.comments.contains_key(&pos)
                || sh.hyperlinks.contains_key(&pos)
                || validation(col).is_some();
            if !special {
                // A run of untouched cells, which take their row's or column's default style.
                let mut end = col + 1;
                while end <= max_col {
                    let p = CellRef::new(row, end);
                    if cells.contains_key(&end)
                        || covered.contains(&p)
                        || spans.contains_key(&p)
                        || sh.comments.contains_key(&p)
                        || sh.hyperlinks.contains_key(&p)
                        || validation(end).is_some()
                    {
                        break;
                    }
                    end += 1;
                }
                out.push_str("<table:table-cell");
                if end - col > 1 {
                    let _ = write!(out, " table:number-columns-repeated=\"{}\"", end - col);
                }
                out.push_str("/>");
                col = end;
                continue;
            }
            let tag = if is_covered { "table:covered-table-cell" } else { "table:table-cell" };
            let _ = write!(out, "<{tag}");
            let style = cell.map(|c| c.style).unwrap_or_else(|| sh.style_id(pos));
            let inherited = row_style.or_else(|| sh.cols.get(&col).and_then(|c| c.style)).unwrap_or(StyleId::DEFAULT);
            if style != inherited || cell.is_some() && style != StyleId::DEFAULT {
                let name = self.cell_style(style);
                let _ = write!(out, " table:style-name=\"{}\"", esc_attr(&name));
            }
            if let Some(v) = validation(col) {
                let _ = write!(out, " table:content-validation-name=\"{v}\"");
            }
            if let Some((w, h)) = spans.get(&pos) {
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
            if let Some(c) = cell {
                self.cell_value(sh, pos, c, out, &mut inner);
            }
            if inner.is_empty() {
                out.push_str("/>");
            } else {
                let _ = write!(out, ">{inner}</{tag}>");
            }
            col += 1;
        }
    }

    fn cell_value(&mut self, sh: &Sheet, pos: CellRef, c: &Cell, out: &mut String, inner: &mut String) {
        if let Some(f) = &c.formula
            && let Some(e) = f.expr()
        {
            let text = formula::to_odf(&e, pos, &WbTables(self.wb));
            let _ = write!(out, " table:formula=\"{}\"", esc_attr(&text));
            if let Some(a) = f.array {
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
        let link = sh.hyperlinks.get(&pos).map(|h| {
            if h.external {
                // Relative to the file's folder in the model, to the package in ODF.
                if super::read::is_relative(&h.target) { format!("../{}", h.target) } else { h.target.clone() }
            } else {
                // `'Q1 Sales'!A1` → `#'Q1 Sales'.A1`.
                match h.target.rsplit_once('!') {
                    Some((s, a)) => format!("#{}.{}", quote(&gridcraft_core::unquote_sheet(s)), a.replace('$', "")),
                    None => format!("#{}", h.target),
                }
            }
        });
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

    fn shapes(&mut self, _i: usize, sh: &Sheet, out: &mut String) {
        if sh.images.is_empty() && sh.charts.is_empty() && sh.shapes.is_empty() {
            return;
        }
        out.push_str("<table:shapes>");
        for shp in &sh.shapes {
            let name = format!("shp{}", self.graphics.len() + 1);
            let x = geometry_x(sh, shp.anchor.cell.col) + shp.anchor.dx * 0.75;
            let y = geometry_y(sh, shp.anchor.cell.row) + shp.anchor.dy * 0.75;
            if let Some(xml) = super::shape::shape_xml(shp, &name, x, y, self.graphics.len()) {
                out.push_str(&xml);
                self.graphics.push(super::shape::style_xml(&name, shp, &self.wb.theme));
            }
        }
        for c in &sh.charts {
            let dir = format!("Object {}", self.objects.len() + 1);
            // Anchors are in pixels at 96 dpi; the file takes points.
            let x = geometry_x(sh, c.anchor.cell.col) + c.anchor.dx * 0.75;
            let y = geometry_y(sh, c.anchor.cell.row) + c.anchor.dy * 0.75;
            let content = super::chart::write(c, &self.wb.theme);
            let object = if self.flat {
                // The chart's own document, inline.
                let body = content.split_once("<office:document-content").map(|(_, rest)| rest).unwrap_or("");
                let body = body.split_once('>').map(|(_, rest)| rest).unwrap_or("").trim_end_matches("</office:document-content>");
                format!(
                    r#"<draw:object><office:document office:mimetype="application/vnd.oasis.opendocument.chart" office:version="1.3">{body}</office:document></draw:object>"#
                )
            } else {
                format!(r#"<draw:object xlink:href="./{}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/>"#, esc_attr(&dir))
            };
            let _ = write!(
                out,
                r#"<draw:frame draw:z-index="{}" draw:name="Chart {}" svg:width="{}pt" svg:height="{}pt" svg:x="{x}pt" svg:y="{y}pt">{object}</draw:frame>"#,
                self.objects.len(),
                c.id,
                c.anchor.width * 0.75,
                c.anchor.height * 0.75,
            );
            self.objects.push((dir, content));
        }
        for img in &sh.images {
            let ext = picture_extension(&img.mime);
            let path = format!("Pictures/{}.{ext}", self.pictures.len() + 1);
            let x = geometry_x(sh, img.anchor.cell.col) + img.anchor.dx * 0.75;
            let y = geometry_y(sh, img.anchor.cell.row) + img.anchor.dy * 0.75;
            let image = if self.flat {
                format!("<draw:image><office:binary-data>{}</office:binary-data></draw:image>", super::b64::encode(&img.data))
            } else {
                format!(r#"<draw:image xlink:href="{}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/>"#, esc_attr(&path))
            };
            let _ = write!(
                out,
                r#"<draw:frame draw:z-index="{}" draw:name="Image {}" draw:style-name="gr1" svg:width="{}pt" svg:height="{}pt" svg:x="{x}pt" svg:y="{y}pt">{image}"#,
                self.pictures.len(),
                img.id,
                img.anchor.width * 0.75,
                img.anchor.height * 0.75,
            );
            if !img.alt.is_empty() {
                let _ = write!(out, "<svg:title>{}</svg:title>", esc(&img.alt));
            }
            out.push_str("</draw:frame>");
            self.pictures.push((path, img.mime.clone(), img.data.clone()));
        }
        out.push_str("</table:shapes>");
    }

    fn cf_style(&mut self, s: &Style) -> String {
        let name = format!("ConditionalStyle_{}", self.cf_styles.len() + 1);
        let props = style::write_props(s, &Style::default(), &self.wb.theme);
        let data = if s.num_fmt != Style::default().num_fmt { self.data_style(s.num_fmt.as_str()) } else { None };
        let data_attr = data.map(|d| format!(" style:data-style-name=\"{d}\"")).unwrap_or_default();
        self.cf_styles.push(format!(
            r#"<style:style style:name="{name}" style:family="table-cell" style:parent-style-name="Default"{data_attr}>{props}</style:style>"#
        ));
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
            let target: Vec<String> = cf.ranges.iter().map(|r| range_address(&sh.name, *r)).collect();
            let base_addr = format!("{}.{}{}", quote(&sh.name), col_to_letters(base.col), base.row + 1);
            let expr = |t: &str| -> String {
                match gridcraft_formula::parse(t.trim_start_matches('=')) {
                    Ok(e) => formula::to_odf(&e, base, &tables).trim_start_matches("of:=").to_string(),
                    Err(_) => t.to_string(),
                }
            };
            let entry = |k: &CfValueKind| -> (String, &'static str) {
                match k {
                    CfValueKind::Min => ("0".into(), "minimum"),
                    CfValueKind::Max => ("0".into(), "maximum"),
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
                        let (v, t) = entry(k);
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
                    let (a, ta) = entry(min);
                    let (b, tb) = entry(max);
                    let ta = if ta == "minimum" { "auto-minimum" } else { ta };
                    let tb = if tb == "maximum" { "auto-maximum" } else { tb };
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
                        let (v, t) = entry(k);
                        let t = if t == "minimum" { "number" } else { t };
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

    fn styles(&mut self) -> String {
        let default = self.wb.styles.get(StyleId::DEFAULT).clone();
        let base = baseline();
        let mut o = format!(r#"<?xml version="1.0" encoding="UTF-8"?><office:document-styles {NS} office:version="1.3">"#);
        o.push_str(&self.font_decls());
        o.push_str("<office:styles>");
        let mut tp = String::new();
        style::write_font(&mut tp, &default.font, &base.font, &self.wb.theme);
        let _ = write!(o, "<style:default-style style:family=\"table-cell\"><style:text-properties{tp}/></style:default-style>");
        let data = if !default.num_fmt.as_str().eq_ignore_ascii_case("General") { self.data_style(default.num_fmt.as_str()) } else { None };
        let mut no_font = default.clone();
        no_font.font = base.font.clone();
        let props = style::write_props(&no_font, &base, &self.wb.theme);
        let data_attr = data.map(|d| format!(" style:data-style-name=\"{d}\"")).unwrap_or_default();
        o.push_str(&self.data_xml_styles());
        let _ = write!(o, r#"<style:style style:name="Default" style:family="table-cell"{data_attr}>{props}</style:style>"#);
        for s in &self.cf_styles {
            o.push_str(s);
        }
        // The arrowhead of arrow shapes: a plain triangle.
        o.push_str(r#"<draw:marker draw:name="Arrow" svg:viewBox="0 0 10 10" svg:d="M5 0 L10 10 L0 10 Z"/>"#);
        o.push_str("</office:styles><office:automatic-styles>");
        let mut masters = String::new();
        for (i, sh) in self.wb.sheets.iter().enumerate() {
            let (layout, master) = page::write(i, &sh.print);
            o.push_str(&layout);
            masters.push_str(&master);
        }
        let _ = write!(o, "</office:automatic-styles><office:master-styles>{masters}</office:master-styles></office:document-styles>");
        o
    }

    /// Data styles for the styles part: the conditional and default ones, which named styles use.
    fn data_xml_styles(&self) -> String {
        self.data_xml.clone()
    }
}

/// A password as `table:protection-key` attributes: only an OpenDocument key has a form here
/// (Excel's salted hashes can't be turned into one), so others protect without a password.
fn protection_key(p: Option<&gridcraft_model::PasswordHash>) -> String {
    match p {
        Some(gridcraft_model::PasswordHash::Iterated { algorithm, hash, .. }) if algorithm == gridcraft_model::PasswordHash::ODF_SHA256 => {
            format!(" table:protection-key=\"{}\" table:protection-key-digest-algorithm=\"http://www.w3.org/2000/09/xmldsig#sha256\"", esc_attr(hash))
        }
        _ => String::new(),
    }
}

/// The file extension for a picture of media type `mime`.
fn picture_extension(mime: &str) -> &str {
    match mime {
        "image/jpeg" => "jpg",
        "image/svg+xml" => "svg",
        "image/tiff" => "tif",
        "image/x-emf" | "image/emf" => "emf",
        "image/x-wmf" | "image/wmf" => "wmf",
        _ => match mime.strip_prefix("image/") {
            Some(sub) if !sub.is_empty() && sub.chars().all(|c| c.is_ascii_alphanumeric()) => sub,
            _ => "png",
        },
    }
}

fn quote(sheet: &str) -> String {
    let plain = sheet.chars().all(|c| c.is_alphanumeric() || c == '_') && !sheet.chars().next().is_some_and(|c| c.is_ascii_digit());
    if plain { sheet.to_string() } else { format!("'{}'", sheet.replace('\'', "''")) }
}

/// `Sheet1.A1:Sheet1.D10`.
fn range_address(sheet: &str, r: RangeRef) -> String {
    let q = quote(sheet);
    format!(
        "{q}.{}{}:{q}.{}{}",
        col_to_letters(r.start.col),
        r.start.row + 1,
        col_to_letters(r.end.col.min(MAX_COLS - 1)),
        r.end.row.min(MAX_ROWS - 1) + 1
    )
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
fn paragraphs(out: &mut String, text: &str, link: Option<&str>) {
    for line in text.split('\n') {
        out.push_str("<text:p>");
        if let Some(l) = link {
            let _ = write!(out, "<text:a xlink:href=\"{}\" xlink:type=\"simple\">", esc_attr(l));
        }
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
                    let mut b = [0u8; 4];
                    out.push_str(&esc(c.encode_utf8(&mut b)));
                }
            }
        }
        // Trailing spaces would collapse: write them explicitly.
        if spaces > 0 {
            let _ = write!(out, "<text:s text:c=\"{spaces}\"/>");
        }
        if link.is_some() {
            out.push_str("</text:a>");
        }
        out.push_str("</text:p>");
    }
}

fn geometry_x(sh: &Sheet, col: u32) -> f32 {
    let custom: f32 = sh.cols.range(..col).map(|(_, c)| if c.hidden { 0.0 } else { c.size.unwrap_or(sh.default_col_width) }).sum();
    let n = sh.cols.range(..col).count() as f32;
    (custom + (col as f32 - n) * sh.default_col_width) * 0.75
}

fn geometry_y(sh: &Sheet, row: u32) -> f32 {
    let custom: f32 = sh.rows.range(..row).map(|(_, r)| if r.hidden { 0.0 } else { r.size.unwrap_or(sh.default_row_height) }).sum();
    let n = sh.rows.range(..row).count() as f32;
    (custom + (row as f32 - n) * sh.default_row_height) * 0.75
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
    let expr = |t: &str| -> String {
        match gridcraft_formula::parse(t.trim_start_matches('=')) {
            Ok(e) => formula::to_odf(&e, base, &tables).trim_start_matches("of:=").to_string(),
            Err(_) => t.to_string(),
        }
    };
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
        esc_attr(&quote(sheet)),
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

fn filter_condition(o: &mut String, col: u32, crit: &gridcraft_model::FilterCriterion) {
    use gridcraft_model::FilterCriterion as F;
    match crit {
        F::Values { values, blanks: true } if values.is_empty() => {
            let _ = write!(o, r#"<table:filter-condition table:field-number="{col}" table:value="" table:operator="empty"/>"#);
        }
        F::Values { values, blanks } => match values.as_slice() {
            [only] if !blanks => {
                let _ = write!(o, r#"<table:filter-condition table:field-number="{col}" table:value="{}" table:operator="="/>"#, esc_attr(only));
            }
            _ => {
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
        },
        F::Custom { a, b, and } => {
            // The filter is an AND of its columns: a column's two conditions joined by OR nest.
            let or = !and && b.is_some();
            if or {
                o.push_str("<table:filter-or>");
            }
            for (op, v) in std::iter::once(a).chain(b.iter()) {
                let (op, v) = match (op.as_str(), v.strip_prefix('*').and_then(|x| x.strip_suffix('*'))) {
                    ("=", Some(inner)) => ("contains", inner.to_string()),
                    ("<>", Some(inner)) => ("!contains", inner.to_string()),
                    ("<>", None) => ("!=", v.clone()),
                    (o, _) => (o, v.clone()),
                };
                let _ = write!(
                    o,
                    r#"<table:filter-condition table:field-number="{col}" table:value="{}" table:operator="{}"/>"#,
                    esc_attr(&v),
                    esc_attr(op)
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
